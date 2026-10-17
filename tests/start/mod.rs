//! Shared by the `rate_limit_start_*` tests: a fake RESP server that
//! timestamps one command, and a one-second rate-limited run against it.
//!
//! The runner can be started once per process, so each run is its own test
//! binary.

use cachecannon::{ColorMode, Config, OutputFormat, create_formatter, run_benchmark_full};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const RATE: usize = 2000;

/// How long the server holds each SET reply when the run has a prefill.
pub const PREFILL_SET_DELAY: Duration = Duration::from_millis(16);

fn read_line(reader: &mut impl BufRead) -> Option<Vec<u8>> {
    let mut line = Vec::new();
    if reader.read_until(b'\n', &mut line).ok()? == 0 {
        return None;
    }
    line.truncate(line.len().saturating_sub(2));
    Some(line)
}

/// Answer SET `+OK` after `set_delay`, GET a miss, PING `+PONG`, anything else
/// `+OK`, and record when each `record` command arrived.
///
/// The connection sets `TCP_NODELAY`, as the client's connections do. The client
/// sends a new batch only when all of a connection's pipeline has been
/// answered, so no request carries the ACK of the first reply. Without
/// `TCP_NODELAY`, Nagle holds the remaining replies until that ACK arrives, and
/// Linux can delay it by 40 ms. 16 connections of 4 requests per 40 ms carry at
/// most 1,600 per second, below [`RATE`].
fn serve(
    stream: TcpStream,
    record: &'static [u8],
    set_delay: Duration,
    times: &Mutex<Vec<Instant>>,
) {
    stream.set_nodelay(true).unwrap();
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    while let Some(header) = read_line(&mut reader) {
        let n: usize = std::str::from_utf8(&header[1..]).unwrap().parse().unwrap();
        let mut command = Vec::new();
        for i in 0..n {
            let len_line = read_line(&mut reader).unwrap();
            let len: usize = std::str::from_utf8(&len_line[1..])
                .unwrap()
                .parse()
                .unwrap();
            let mut arg = vec![0u8; len + 2];
            reader.read_exact(&mut arg).unwrap();
            if i == 0 {
                arg.truncate(len);
                command = arg.to_ascii_uppercase();
            }
        }
        if command == record {
            times.lock().unwrap().push(Instant::now());
        }
        if command == b"SET" && !set_delay.is_zero() {
            std::thread::sleep(set_delay);
        }
        let reply: &[u8] = match command.as_slice() {
            b"GET" => b"$-1\r\n",
            b"PING" => b"+PONG\r\n",
            _ => b"+OK\r\n",
        };
        if writer.write_all(reply).is_err() {
            return;
        }
    }
}

fn spawn(record: &'static [u8], set_delay: Duration) -> (SocketAddr, Arc<Mutex<Vec<Instant>>>) {
    let times = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&times);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let recorded = Arc::clone(&recorded);
            std::thread::spawn(move || serve(stream, record, set_delay, &recorded));
        }
    });
    (addr, times)
}

/// Run one second at [`RATE`] with no warmup, after a prefill of 1,000 keys
/// when `prefill` is set, and return the arrival times of the measured requests
/// (GETs after a prefill, SETs without one), relative to the first.
///
/// With a prefill, the server holds each SET reply for [`PREFILL_SET_DELAY`].
/// Each connection's commands are answered in order by one thread, so 16
/// connections complete at most about 1,000 SETs per second and the prefill
/// lasts at least a second.
pub fn arrivals(prefill: bool) -> Vec<Duration> {
    let (record, commands, set_delay): (&'static [u8], &str, Duration) = if prefill {
        (b"GET", "get = 100\n        set = 0", PREFILL_SET_DELAY)
    } else {
        (b"SET", "get = 0\n        set = 100", Duration::ZERO)
    };
    let (addr, times) = spawn(record, set_delay);
    let toml = format!(
        r#"
        [general]
        duration = "1s"
        warmup = "0s"
        threads = 4

        [target]
        endpoints = ["{addr}"]
        protocol = "resp"

        [connection]
        connections = 16
        pipeline_depth = 4

        [workload]
        rate_limit = {RATE}
        prefill = {prefill}
        prefill_settle = "0s"

        [workload.keyspace]
        count = 1000

        [workload.commands]
        {commands}

        [admin]
        format = "quiet"
        "#
    );
    let path = std::env::temp_dir().join(format!(
        "cachecannon-rate-limit-start-{}-{prefill}.toml",
        std::process::id()
    ));
    std::fs::write(&path, toml).unwrap();
    let config = Config::load(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    run_benchmark_full(
        config,
        None,
        create_formatter(OutputFormat::Quiet, ColorMode::Never),
        Arc::new(AtomicBool::new(true)),
    )
    .unwrap();

    let times = times.lock().unwrap();
    assert!(
        times.len() > RATE / 2,
        "only {} requests in a 1 s run",
        times.len()
    );
    let first = times[0];
    times.iter().map(|t| t.duration_since(first)).collect()
}

/// How many of `arrivals` came in the first `ms` milliseconds.
pub fn within(arrivals: &[Duration], ms: u64) -> usize {
    arrivals
        .iter()
        .filter(|t| **t < Duration::from_millis(ms))
        .count()
}
