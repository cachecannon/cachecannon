//! For `tests/kv_replay.rs`: an in-process RESP server that stores what it is
//! sent, a trace writer, one closed-loop replay against the server, and
//! [`isolated`], which runs a test in its own process.

use cachecannon::{ColorMode, Config, OutputFormat, create_formatter, run_benchmark_full};
use kvtrace::{ObjectKind, Request, RequestWriter};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Wall time within which a replay that should end before `duration` (60 s
/// in [`replay_file`]) must finish. It covers startup, which fills a 1 GiB
/// value pool and takes about 3.4 s alone, plus the replay; one run of a
/// whole timed test took 11.8 s in a loaded suite on macOS. A run that
/// reaches `duration` exceeds it.
pub const RUN_BOUND: Duration = Duration::from_secs(30);

/// Tokens per chunk in every test trace.
pub const CHUNK_TOKENS: u32 = 16;

/// Key of chunk `chunk`'s object on `rank`, as the replay writes it with the
/// default prefix and hex format.
pub fn key(chunk: u64, rank: u64) -> Vec<u8> {
    let id = kvtrace::obj_id(chunk, ObjectKind::Kv, rank).unwrap();
    format!("kv:{id:016x}").into_bytes()
}

/// One command the server received: its name (upper case), key, and when it
/// was read.
#[derive(Debug, Clone)]
pub struct Op {
    pub cmd: String,
    pub key: Vec<u8>,
    pub at: Instant,
}

/// Server-side changes made at a set point, standing in for an eviction.
#[derive(Default)]
pub struct Hooks {
    /// Once this many SETs have been stored, delete these keys.
    pub delete_after_sets: Option<(usize, Vec<Vec<u8>>)>,
    /// The first time an EXISTS for this key answers 1, delete the key, so
    /// the GET that follows misses.
    pub evict_after_exists: Option<Vec<u8>>,
    /// Hold the reply to the first SET of this key this long. Other
    /// connections are served meanwhile.
    pub delay_set: Option<(Vec<u8>, Duration)>,
    /// After replying to this many SETs in total, wait this long and close
    /// the connection, as a server closes an idle connection.
    pub close_after_sets: Option<(usize, Duration)>,
    /// After replying to this many SETs in total, wait this long and shut
    /// down every connection open then, as an idle timeout closing all idle
    /// connections at once.
    pub close_all_after_sets: Option<(usize, Duration)>,
    /// With `close_all_after_sets`: stop accepting connections when they are
    /// shut, so every reconnect is refused.
    pub refuse_after_close_all: bool,
    /// With `close_all_after_sets`: from then on accept connections and shut
    /// each down at once.
    pub drop_new_after_close_all: bool,
}

#[derive(Default)]
struct State {
    store: HashMap<Vec<u8>, Vec<u8>>,
    log: Vec<Op>,
    sets: usize,
    hooks: Hooks,
    /// Every connection accepted, for `close_all_after_sets`.
    streams: Vec<TcpStream>,
    /// Set by `refuse_after_close_all`: the listener closes.
    refusing: bool,
    /// Set by `drop_new_after_close_all`.
    dropping_new: bool,
}

/// A RESP2 server holding every SET in memory. Answers PING, EXISTS (one
/// key), GET, SET and DEL.
#[derive(Clone, Default)]
pub struct Server {
    state: Arc<Mutex<State>>,
}

impl Server {
    pub fn with_hooks(hooks: Hooks) -> Self {
        let s = Self::default();
        s.state.lock().unwrap().hooks = hooks;
        s
    }

    /// Every command received so far, in order.
    pub fn log(&self) -> Vec<Op> {
        self.state.lock().unwrap().log.clone()
    }

    /// Keys of the SETs received, in order.
    pub fn sets(&self) -> Vec<Vec<u8>> {
        self.ops("SET")
    }

    /// Keys of the `cmd` commands received, in order.
    pub fn ops(&self, cmd: &str) -> Vec<Vec<u8>> {
        self.log()
            .into_iter()
            .filter(|o| o.cmd == cmd)
            .map(|o| o.key)
            .collect()
    }

    /// Length of the value stored at `key`.
    pub fn value_len(&self, key: &[u8]) -> Option<usize> {
        self.state.lock().unwrap().store.get(key).map(Vec::len)
    }

    pub fn spawn(&self) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let state = Arc::clone(&self.state);
        // Non-blocking, so the listener can be closed to refuse reconnects.
        listener.set_nonblocking(true).unwrap();
        std::thread::spawn(move || {
            loop {
                if state.lock().unwrap().refusing {
                    return;
                }
                match listener.accept() {
                    Ok((stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        if state.lock().unwrap().dropping_new {
                            let _ = stream.shutdown(std::net::Shutdown::Both);
                            continue;
                        }
                        {
                            let mut st = state.lock().unwrap();
                            if st.hooks.close_all_after_sets.is_some() {
                                st.streams.push(stream.try_clone().unwrap());
                            }
                        }
                        let state = Arc::clone(&state);
                        std::thread::spawn(move || serve(stream, state));
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(1)),
                }
            }
        });
        addr
    }
}

/// Read one CRLF-terminated line, without the CRLF. `None` at EOF.
fn read_line(reader: &mut impl BufRead) -> Option<Vec<u8>> {
    let mut line = Vec::new();
    if reader.read_until(b'\n', &mut line).ok()? == 0 {
        return None;
    }
    line.truncate(line.len().saturating_sub(2));
    Some(line)
}

fn serve(stream: TcpStream, state: Arc<Mutex<State>>) {
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    while let Some(header) = read_line(&mut reader) {
        let n: usize = std::str::from_utf8(&header[1..]).unwrap().parse().unwrap();
        let mut args = Vec::with_capacity(n);
        for _ in 0..n {
            let len_line = read_line(&mut reader).unwrap();
            let len: usize = std::str::from_utf8(&len_line[1..])
                .unwrap()
                .parse()
                .unwrap();
            let mut arg = vec![0u8; len + 2];
            reader.read_exact(&mut arg).unwrap();
            arg.truncate(len);
            args.push(arg);
        }
        let cmd = String::from_utf8_lossy(&args[0]).to_ascii_uppercase();
        let key = args.get(1).cloned().unwrap_or_default();
        let mut delay = None;
        let mut close = None;
        let reply: Vec<u8> = {
            let mut st = state.lock().unwrap();
            st.log.push(Op {
                cmd: cmd.clone(),
                key: key.clone(),
                at: Instant::now(),
            });
            match cmd.as_str() {
                "PING" => b"+PONG\r\n".to_vec(),
                "EXISTS" => {
                    let found = st.store.contains_key(&key);
                    if found && st.hooks.evict_after_exists.as_ref() == Some(&key) {
                        st.hooks.evict_after_exists = None;
                        st.store.remove(&key);
                    }
                    format!(":{}\r\n", u8::from(found)).into_bytes()
                }
                "GET" => match st.store.get(&key) {
                    Some(v) => {
                        let mut r = format!("${}\r\n", v.len()).into_bytes();
                        r.extend_from_slice(v);
                        r.extend_from_slice(b"\r\n");
                        r
                    }
                    None => b"$-1\r\n".to_vec(),
                },
                "SET" => {
                    if st.hooks.delay_set.as_ref().is_some_and(|(k, _)| *k == key) {
                        delay = st.hooks.delay_set.take().map(|(_, d)| d);
                    }
                    st.store.insert(key, args[2].clone());
                    st.sets += 1;
                    if st.hooks.close_after_sets.is_some_and(|(n, _)| n == st.sets) {
                        close = st.hooks.close_after_sets.take().map(|(_, d)| d);
                    }
                    if st
                        .hooks
                        .close_all_after_sets
                        .is_some_and(|(n, _)| n == st.sets)
                    {
                        let (_, after) = st.hooks.close_all_after_sets.take().unwrap();
                        let refuse = st.hooks.refuse_after_close_all;
                        let drop_new = st.hooks.drop_new_after_close_all;
                        let all: Vec<TcpStream> =
                            st.streams.iter().map(|s| s.try_clone().unwrap()).collect();
                        let state = Arc::clone(&state);
                        std::thread::spawn(move || {
                            std::thread::sleep(after);
                            if refuse {
                                state.lock().unwrap().refusing = true;
                            }
                            if drop_new {
                                state.lock().unwrap().dropping_new = true;
                            }
                            for s in all {
                                let _ = s.shutdown(std::net::Shutdown::Both);
                            }
                        });
                    }
                    if let Some((after, _)) = st.hooks.delete_after_sets
                        && st.sets == after
                    {
                        let (_, keys) = st.hooks.delete_after_sets.take().unwrap();
                        for k in keys {
                            st.store.remove(&k);
                        }
                    }
                    b"+OK\r\n".to_vec()
                }
                "DEL" => format!(":{}\r\n", u8::from(st.store.remove(&key).is_some())).into_bytes(),
                _ => b"+OK\r\n".to_vec(),
            }
        };
        if let Some(d) = delay {
            std::thread::sleep(d);
        }
        if writer.write_all(&reply).is_err() {
            return;
        }
        if let Some(after) = close {
            std::thread::sleep(after);
            // Shut down rather than only drop: another handle to the socket
            // (kept for `close_all_after_sets`) would keep it open.
            let _ = writer.shutdown(std::net::Shutdown::Both);
            return;
        }
    }
}

/// A trace request: `chunks` full chunks of [`CHUNK_TOKENS`], arriving at
/// `timestamp_ms`, with `input_tokens` prompt tokens.
pub fn request(req_id: u64, timestamp_ms: i64, chunks: &[u64], input_tokens: u32) -> Request {
    Request {
        timestamp: timestamp_ms * 1_000_000,
        req_id,
        input_tokens,
        output_tokens: 1,
        chunks: chunks.to_vec(),
        session_id: None,
    }
}

/// A file under the temp directory unique to this process.
pub fn temp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("cachecannon-kv-{}-{name}", std::process::id()))
}

/// Write `requests` as a kvtrace request trace with [`CHUNK_TOKENS`].
pub fn write_trace(requests: &[Request]) -> PathBuf {
    let path = temp_path("trace.parquet");
    let mut w = RequestWriter::create(&path, CHUNK_TOKENS, 1024).unwrap();
    for r in requests {
        w.write(r).unwrap();
    }
    w.finish().unwrap();
    path
}

/// Write `requests` in the given order, which need not be timestamp order,
/// as a request trace kvtrace's writer would refuse: the same schema and
/// metadata, through arrow directly.
pub fn write_unordered_trace(requests: &[Request]) -> PathBuf {
    use arrow::array::{
        ArrayRef, ListBuilder, TimestampNanosecondArray, UInt32Array, UInt64Array, UInt64Builder,
    };
    use arrow::record_batch::RecordBatch;
    use kvtrace::format::{
        FORMAT_REQUESTS, FORMAT_VERSION, KEY_CHUNK_TOKENS, KEY_FORMAT, KEY_VERSION, request_schema,
    };
    use parquet::arrow::ArrowWriter;
    use parquet::file::metadata::KeyValue;

    let path = temp_path("unordered.parquet");
    let schema = Arc::new(request_schema());
    let mut chunks = ListBuilder::new(UInt64Builder::new()).with_field(Arc::new(
        arrow::datatypes::Field::new("item", arrow::datatypes::DataType::UInt64, false),
    ));
    for r in requests {
        chunks.values().append_slice(&r.chunks);
        chunks.append(true);
    }
    let arrays: Vec<ArrayRef> = vec![
        Arc::new(
            TimestampNanosecondArray::from_iter_values(requests.iter().map(|r| r.timestamp))
                .with_timezone("UTC"),
        ),
        Arc::new(UInt64Array::from_iter_values(
            requests.iter().map(|r| r.req_id),
        )),
        Arc::new(UInt32Array::from_iter_values(
            requests.iter().map(|r| r.input_tokens),
        )),
        Arc::new(UInt32Array::from_iter_values(
            requests.iter().map(|r| r.output_tokens),
        )),
        Arc::new(chunks.finish()),
        Arc::new(UInt64Array::from(
            requests.iter().map(|r| r.session_id).collect::<Vec<_>>(),
        )),
    ];
    let mut w =
        ArrowWriter::try_new(std::fs::File::create(&path).unwrap(), schema.clone(), None).unwrap();
    for (k, v) in [
        (KEY_FORMAT, FORMAT_REQUESTS.to_string()),
        (KEY_VERSION, FORMAT_VERSION.to_string()),
        (KEY_CHUNK_TOKENS, CHUNK_TOKENS.to_string()),
    ] {
        w.append_key_value_metadata(KeyValue::new(k.to_string(), v));
    }
    w.write(&RecordBatch::try_new(schema, arrays).unwrap())
        .unwrap();
    w.close().unwrap();
    path
}

/// One row of the `requests_out` CSV.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub req_id: u64,
    pub lag_ns: Option<u64>,
    pub input_tokens: u64,
    pub chunks: u64,
    pub lookup_hit_chunks: u64,
    pub hit_chunks: u64,
    pub cached_tokens: u64,
    pub new_tokens: u64,
    pub gets: u64,
    pub get_nil: u64,
    pub sets: u64,
    pub lookup_ns: u64,
    pub load_ns: u64,
    pub store_ns: u64,
    /// With `store_connections`: the request's store from queued to its
    /// first SET sent. Empty otherwise.
    pub store_queue_ns: Option<u64>,
    pub ready_ns: u64,
    pub store_dropped: bool,
}

fn read_rows(path: &PathBuf) -> Vec<Row> {
    let text = std::fs::read_to_string(path).unwrap();
    let mut lines = text.lines();
    assert_eq!(
        lines.next(),
        Some(cachecannon::kvreplay::REQUESTS_OUT_HEADER)
    );
    lines
        .map(|line| {
            let f: Vec<&str> = line.split(',').collect();
            assert_eq!(f.len(), 19, "{line}");
            let n = |i: usize| f[i].parse::<u64>().unwrap();
            Row {
                req_id: n(0),
                lag_ns: (!f[2].is_empty()).then(|| n(2)),
                input_tokens: n(3),
                chunks: n(4),
                lookup_hit_chunks: n(5),
                hit_chunks: n(6),
                cached_tokens: n(7),
                new_tokens: n(8),
                gets: n(9),
                get_nil: n(10),
                sets: n(11),
                lookup_ns: n(12),
                load_ns: n(13),
                store_ns: n(14),
                store_queue_ns: (!f[16].is_empty()).then(|| n(16)),
                ready_ns: n(17),
                store_dropped: match f[18] {
                    "0" => false,
                    "1" => true,
                    other => panic!("store_dropped {other}: {line}"),
                },
            }
        })
        .collect()
}

/// Replay `requests` against `server` with one connection, closed loop
/// unless `kv_toml` sets `speedup`, 4 bytes of KV per token unless it sets
/// the geometry. `kv_toml` is appended to `[workload.kv_replay]`. Returns
/// the per-request rows in completion order.
pub fn replay(server: &Server, requests: &[Request], kv_toml: &str) -> Vec<Row> {
    replay_on(server, requests, kv_toml, 1)
}

/// [`replay`] on `connections` connections.
pub fn replay_on(
    server: &Server,
    requests: &[Request],
    kv_toml: &str,
    connections: usize,
) -> Vec<Row> {
    replay_with(server, requests, kv_toml, connections, "resp")
}

/// [`replay_on`] with `protocol` (`resp` or `resp3`).
pub fn replay_with(
    server: &Server,
    requests: &[Request],
    kv_toml: &str,
    connections: usize,
    protocol: &str,
) -> Vec<Row> {
    let trace = write_trace(requests);
    let (result, rows) = replay_file(server, trace, kv_toml, connections, protocol);
    result.unwrap();
    rows
}

/// Replay the trace file `trace` (removed afterwards) as [`replay_with`]
/// does, and return the run's result with the rows written before it ended.
pub fn replay_file(
    server: &Server,
    trace: PathBuf,
    kv_toml: &str,
    connections: usize,
    protocol: &str,
) -> (Result<(), String>, Vec<Row>) {
    let addr = server.spawn();
    let out = temp_path("requests.csv");
    let geometry = if kv_toml.contains("model_config") || kv_toml.contains("kv_bytes_per_token") {
        ""
    } else {
        "kv_bytes_per_token = 4"
    };
    let speedup = if kv_toml.contains("speedup") {
        ""
    } else {
        "speedup = 0"
    };
    // A line `general.<key> = <value>` in `kv_toml` sets that `[general]`
    // key in place of its default. `duration` is an upper bound the tests
    // assert runs end well before (`RUN_BOUND`): a run that reaches it is
    // the failure they look for.
    let overrides: Vec<&str> = kv_toml
        .lines()
        .filter_map(|l| l.trim().strip_prefix("general."))
        .collect();
    let general_toml = [
        ("duration", "duration = \"60s\""),
        ("warmup", "warmup = \"0s\""),
    ]
    .into_iter()
    .filter(|(key, _)| {
        !overrides
            .iter()
            .any(|o| o.split('=').next().map(str::trim) == Some(*key))
    })
    .map(|(_, line)| line)
    .chain(overrides.iter().copied())
    .collect::<Vec<_>>()
    .join("\n");
    let kv_toml: String = kv_toml
        .lines()
        .filter(|l| !l.trim().starts_with("general."))
        .collect::<Vec<_>>()
        .join("\n");
    let toml = format!(
        r#"
        [general]
        {general_toml}
        threads = 1

        [target]
        endpoints = ["{addr}"]
        protocol = "{protocol}"

        [connection]
        connections = {connections}
        pipeline_depth = 4

        [workload.kv_replay]
        trace = "{trace}"
        requests_out = "{out}"
        {geometry}
        {speedup}
        {kv_toml}

        [admin]
        format = "quiet"
        "#,
        trace = trace.display(),
        out = out.display(),
    );
    let config = load_config(&toml);
    let result = run_benchmark_full(
        config,
        None,
        create_formatter(OutputFormat::Quiet, ColorMode::Never),
        Arc::new(AtomicBool::new(true)),
    )
    .map_err(|e| e.to_string());
    let rows = read_rows(&out);
    let _ = std::fs::remove_file(&trace);
    let _ = std::fs::remove_file(&out);
    (result, rows)
}

/// Parse `toml` as a config file.
pub fn load_config(toml: &str) -> Config {
    let path = temp_path("config.toml");
    std::fs::write(&path, toml).unwrap();
    let config = Config::load(&path);
    std::fs::remove_file(&path).unwrap();
    config.unwrap()
}

/// Run `body` in a child process of this test binary, one child at a time.
///
/// `run_benchmark_full` can run once per process (the worker config channel
/// is a `OnceLock`), and the metrics the tests read are process-global, so
/// every replay needs a fresh process. The child is this binary run with
/// `name` as an exact filter and `CACHECANNON_KV_TEST=name`, which makes the
/// same test run `body` instead of spawning. `name` must be the running
/// test's own name, which is checked in the parent and in the child. Children
/// run one at a time
/// because each allocates the 1 GiB value pool and the open-loop test
/// measures timing.
pub fn isolated(name: &str, body: impl FnOnce()) {
    const VAR: &str = "CACHECANNON_KV_TEST";
    // libtest runs each test on a thread named after it. A `name` that is
    // another test's exact name would otherwise run that test's body in the
    // child, and pass.
    let running = std::thread::current().name().map(str::to_owned);
    assert_eq!(
        running.as_deref(),
        Some(name),
        "isolated() was given a name that is not the running test's"
    );
    if std::env::var(VAR).as_deref() == Ok(name) {
        body();
        return;
    }
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let out = Command::new(std::env::current_exe().unwrap())
        .args([name, "--exact", "--nocapture", "--test-threads=1"])
        .env(VAR, name)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "{name} failed in its child process\n--- stdout\n{stdout}\n--- stderr\n{stderr}"
    );
    // A filter that matched nothing would also succeed.
    assert!(
        stdout.contains("test result: ok. 1 passed"),
        "{name}: the child process did not run exactly one test\n{stdout}"
    );
}
