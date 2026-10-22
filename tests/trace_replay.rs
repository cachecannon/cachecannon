//! `[workload.trace_replay]` against a fake server that answers as a server
//! with the large-object module would: `BLOB.TCP_SET key value` stores the
//! value's length, `BLOB.TCP_GET key` returns a value of that length or nil,
//! `DEL` removes it. Every command is logged with its key, its value length
//! and the connection it arrived on.
//!
//! Each test runs in its own child process (`common::kv::isolated`): the runner
//! can be started once per process and its metrics are process-global.

mod common;

use cachecannon::metrics::{BACKFILL_SET_COUNT, TRACE_ROWS, TRACE_ROWS_FAILED, TRACE_ROWS_SKIPPED};
use cachecannon::{ColorMode, Config, OutputFormat, create_formatter, run_benchmark_full};
use cachetrace::{TraceEntry, TraceWriter};
use common::kv::isolated;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
struct Cmd {
    name: String,
    key: String,
    /// Value length of a SET.
    len: Option<usize>,
    conn: usize,
    at: Instant,
}

#[derive(Default)]
struct State {
    log: Vec<Cmd>,
    store: HashMap<Vec<u8>, usize>,
}

#[derive(Clone)]
struct Server {
    state: Arc<Mutex<State>>,
    /// How long each `BLOB.TCP_GET` is held before its reply.
    get_delay: Duration,
    /// Connections with a `BLOB.TCP_GET` being held, and the most at once.
    getting: Arc<AtomicUsize>,
    most_getting: Arc<AtomicUsize>,
    /// Close the connection, without replying, on every this-many-th command
    /// across all connections; 0 never.
    drop_every: usize,
    commands: Arc<AtomicUsize>,
}

impl Server {
    fn new(get_delay: Duration) -> Self {
        Self {
            state: Arc::default(),
            get_delay,
            getting: Arc::default(),
            most_getting: Arc::default(),
            drop_every: 0,
            commands: Arc::default(),
        }
    }

    fn spawn(&self) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = self.clone();
        std::thread::spawn(move || {
            for (conn, stream) in listener.incoming().flatten().enumerate() {
                let server = server.clone();
                std::thread::spawn(move || serve(stream, conn, &server));
            }
        });
        addr
    }

    /// Workload commands in arrival order, without HELLO and PING.
    fn workload(&self) -> Vec<Cmd> {
        self.state
            .lock()
            .unwrap()
            .log
            .iter()
            .filter(|c| c.name != "HELLO" && c.name != "PING")
            .cloned()
            .collect()
    }
}

fn read_line(reader: &mut impl BufRead) -> Option<Vec<u8>> {
    let mut line = Vec::new();
    if reader.read_until(b'\n', &mut line).ok()? == 0 {
        return None;
    }
    line.truncate(line.len().saturating_sub(2));
    Some(line)
}

fn serve(stream: TcpStream, conn: usize, server: &Server) {
    let state = &server.state;
    let drop_every = server.drop_every;
    stream.set_nodelay(true).unwrap();
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    while let Some(header) = read_line(&mut reader) {
        let n: usize = std::str::from_utf8(&header[1..]).unwrap().parse().unwrap();
        let mut args = Vec::with_capacity(n);
        for _ in 0..n {
            let Some(len_line) = read_line(&mut reader) else {
                return;
            };
            let len: usize = std::str::from_utf8(&len_line[1..])
                .unwrap()
                .parse()
                .unwrap();
            let mut arg = vec![0u8; len + 2];
            if reader.read_exact(&mut arg).is_err() {
                return;
            }
            arg.truncate(len);
            args.push(arg);
        }
        let name = String::from_utf8_lossy(&args[0]).to_ascii_uppercase();
        let key = args.get(1).cloned().unwrap_or_default();
        if drop_every > 0
            && name.starts_with("BLOB.")
            && (server.commands.fetch_add(1, Ordering::SeqCst) + 1).is_multiple_of(drop_every)
        {
            let _ = writer.shutdown(std::net::Shutdown::Both);
            return;
        }
        let set_len = (name == "BLOB.TCP_SET" && n == 3).then(|| args[2].len());
        let reply: Vec<u8> = {
            let mut st = state.lock().unwrap();
            st.log.push(Cmd {
                name: name.clone(),
                key: String::from_utf8_lossy(&key).into_owned(),
                len: set_len,
                conn,
                at: Instant::now(),
            });
            match (name.as_str(), n) {
                ("PING", _) => b"+PONG\r\n".to_vec(),
                ("BLOB.TCP_SET", 3) => {
                    st.store.insert(key, set_len.unwrap());
                    b"+OK\r\n".to_vec()
                }
                ("BLOB.TCP_GET", 2) => match st.store.get(&key) {
                    Some(&len) => {
                        let mut r = format!("${len}\r\n").into_bytes();
                        r.extend(std::iter::repeat_n(b'v', len));
                        r.extend_from_slice(b"\r\n");
                        r
                    }
                    None => b"$-1\r\n".to_vec(),
                },
                ("DEL", 2) => {
                    format!(":{}\r\n", u8::from(st.store.remove(&key).is_some())).into_bytes()
                }
                ("GET" | "SET", _) => {
                    b"-WRONGTYPE Operation against a key holding the wrong kind of value\r\n"
                        .to_vec()
                }
                _ => b"-ERR unexpected command\r\n".to_vec(),
            }
        };
        if name == "BLOB.TCP_GET" && !server.get_delay.is_zero() {
            let now = server.getting.fetch_add(1, Ordering::SeqCst) + 1;
            server.most_getting.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(server.get_delay);
            server.getting.fetch_sub(1, Ordering::SeqCst);
        }
        if writer.write_all(&reply).is_err() {
            return;
        }
    }
}

/// A trace row: `op` is a libCacheSim op code, `None` for a row without one.
fn row(ts: i64, obj_id: u64, size: u32, op: Option<u8>) -> TraceEntry {
    TraceEntry {
        timestamp: ts,
        obj_id,
        obj_size: size,
        next_access_vtime: -1,
        op,
        ttl: None,
    }
}

const GET: Option<u8> = Some(1);
const SET: Option<u8> = Some(3);
const DEL: Option<u8> = Some(9);

fn write_trace(name: &str, rows: &[TraceEntry]) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "cachecannon-trace-replay-{name}-{}.parquet",
        std::process::id()
    ));
    let mut w = TraceWriter::create(&path, 16).unwrap();
    for r in rows {
        w.write(r).unwrap();
    }
    w.finish().unwrap();
    path
}

/// Run `extra` against `addr` with the trace at `trace`, and return how long
/// the run took. `duration` is long: the run must end because the trace did.
fn run(addr: SocketAddr, trace: &Path, connections: usize, extra: &str) -> Duration {
    let start = Instant::now();
    try_run(addr, trace, 1, connections, 1, extra).unwrap();
    start.elapsed()
}

fn try_run(
    addr: SocketAddr,
    trace: &Path,
    threads: usize,
    connections: usize,
    pipeline: usize,
    extra: &str,
) -> Result<(), String> {
    let toml = format!(
        r#"
        [general]
        duration = "60s"
        warmup = "0s"
        threads = {threads}

        [target]
        endpoints = ["{addr}"]
        protocol = "resp"

        [connection]
        connections = {connections}
        pipeline_depth = {pipeline}

        [workload]
        {extra}

        [workload.trace_replay]
        trace = "{}"
        key_prefix = "o:"

        [resp]
        module = "largeobj"

        [admin]
        format = "quiet"
        "#,
        trace.display()
    );
    let path = trace.with_extension("toml");
    std::fs::write(&path, toml).unwrap();
    let config = Config::load(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    run_benchmark_full(
        config,
        None,
        create_formatter(OutputFormat::Quiet, ColorMode::Never),
        Arc::new(AtomicBool::new(true)),
    )
    .map_err(|e| e.to_string())
}

fn key(id: u64) -> String {
    format!("o:{id:016x}")
}

fn cmd(name: &str, id: u64, len: Option<usize>) -> (String, String, Option<usize>) {
    (name.to_string(), key(id), len)
}

/// One connection with one request in flight sends the rows in file order:
/// SETs with each row's size (one small enough to copy, one large enough to
/// send zero-copy), GETs, a row with no op as a GET whose miss is backfilled
/// at the row's size, DELs, and no command for a row with an unknown op.
#[test]
fn replays_rows_in_order_as_module_commands() {
    isolated("replays_rows_in_order_as_module_commands", || {
        let mut rows = Vec::new();
        let mut ts = 0;
        let mut next = |id, size, op| {
            ts += 1_000;
            row(ts, id, size, op)
        };
        let mut want = Vec::new();
        for id in 1..=50 {
            rows.push(next(id, 100, SET));
            want.push(cmd("BLOB.TCP_SET", id, Some(100)));
        }
        rows.push(next(51, 8192, SET));
        want.push(cmd("BLOB.TCP_SET", 51, Some(8192)));
        for id in 1..=50 {
            rows.push(next(id, 100, GET));
            want.push(cmd("BLOB.TCP_GET", id, None));
        }
        rows.push(next(1000, 300, None));
        want.push(cmd("BLOB.TCP_GET", 1000, None));
        want.push(cmd("BLOB.TCP_SET", 1000, Some(300)));
        for id in 1..=10 {
            rows.push(next(id, 100, DEL));
            want.push(cmd("DEL", id, None));
        }
        rows.push(next(77, 1, Some(15)));

        let server = Server::new(Duration::ZERO);
        let addr = server.spawn();
        let trace = write_trace("order", &rows);
        let took = run(addr, &trace, 1, "backfill_on_miss = true");
        std::fs::remove_file(&trace).unwrap();

        let got: Vec<_> = server
            .workload()
            .into_iter()
            .map(|c| (c.name, c.key, c.len))
            .collect();
        assert_eq!(got, want);
        assert_eq!(TRACE_ROWS.value(), 112);
        assert_eq!(TRACE_ROWS_SKIPPED.value(), 1);
        assert_eq!(TRACE_ROWS_FAILED.value(), 0);
        assert_eq!(BACKFILL_SET_COUNT.value(), 1);
        assert!(
            took < Duration::from_secs(30),
            "the run took {took:?}; it should end when the trace does"
        );
    });
}

/// GETs of one key go to whichever connection is free, so a hot key's GETs
/// are in flight on several connections at once.
#[test]
fn hot_key_gets_spread_over_connections() {
    isolated("hot_key_gets_spread_over_connections", || {
        let mut rows = vec![row(0, 5, 64, SET)];
        rows.extend((1..=200).map(|i| row(i, 5, 64, GET)));
        let server = Server::new(Duration::from_millis(2));
        let addr = server.spawn();
        let trace = write_trace("hot", &rows);
        run(addr, &trace, 4, "");
        std::fs::remove_file(&trace).unwrap();

        let gets: Vec<Cmd> = server
            .workload()
            .into_iter()
            .filter(|c| c.name == "BLOB.TCP_GET")
            .collect();
        assert_eq!(gets.len(), 200);
        assert!(gets.iter().all(|c| c.key == key(5)));
        let conns: std::collections::BTreeSet<usize> = gets.iter().map(|c| c.conn).collect();
        assert!(conns.len() >= 2, "every GET arrived on {conns:?}");
        let most = server.most_getting.load(Ordering::SeqCst);
        assert!(
            most >= 2,
            "at most {most} GET of the hot key in flight at once"
        );
    });
}

/// `rate_limit` paces rows: 100 rows at 200 per second arrive over about half
/// a second (390 ms measured, the limiter's starting allowance included),
/// where unpaced they arrive within a few milliseconds (7 ms measured).
#[test]
fn rate_limit_paces_rows() {
    isolated("rate_limit_paces_rows", || {
        let rows: Vec<_> = (0..100).map(|i| row(i, i as u64, 64, SET)).collect();
        let server = Server::new(Duration::ZERO);
        let addr = server.spawn();
        let trace = write_trace("rate", &rows);
        run(addr, &trace, 1, "rate_limit = 200");
        std::fs::remove_file(&trace).unwrap();

        let cmds = server.workload();
        assert_eq!(cmds.len(), 100);
        let span = cmds[99].at - cmds[0].at;
        assert!(
            span >= Duration::from_millis(250),
            "100 rows at 200/s arrived over {span:?}"
        );
        assert!(span < Duration::from_secs(5), "arrived over {span:?}");
    });
}

/// With connections failing part-way through pipelines on two workers, every
/// row is either answered or counted failed exactly once, and the run ends by
/// itself.
#[test]
fn failed_connections_account_for_every_row() {
    isolated("failed_connections_account_for_every_row", || {
        let rows: Vec<_> = (0..3000).map(|i| row(i, i as u64 % 500, 64, SET)).collect();
        let mut server = Server::new(Duration::ZERO);
        server.drop_every = 300;
        let addr = server.spawn();
        let trace = write_trace("drops", &rows);
        let start = Instant::now();
        try_run(addr, &trace, 2, 4, 16, "").unwrap();
        std::fs::remove_file(&trace).unwrap();
        let (done, failed) = (TRACE_ROWS.value(), TRACE_ROWS_FAILED.value());
        assert!(
            failed > 0,
            "no row failed; the server dropped no connection mid-pipeline?"
        );
        assert_eq!(done + failed, 3000, "answered {done} + failed {failed}");
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "took {:?}",
            start.elapsed()
        );
    });
}

/// A trace whose every row is skipped fails the run and says why.
#[test]
fn every_row_skipped_fails_the_run() {
    isolated("every_row_skipped_fails_the_run", || {
        let rows: Vec<_> = (0..10).map(|i| row(i, i as u64, 64, Some(15))).collect();
        let server = Server::new(Duration::ZERO);
        let addr = server.spawn();
        let trace = write_trace("skipped", &rows);
        let err = try_run(addr, &trace, 1, 1, 1, "").unwrap_err();
        std::fs::remove_file(&trace).unwrap();
        assert!(err.contains("every row of the trace was skipped"), "{err}");
        assert!(server.workload().is_empty());
    });
}
