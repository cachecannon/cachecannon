//! Shared by the `close_on_request_timeout*` tests. Each test is its own
//! binary because `run_benchmark_full` sets the process-global
//! `CONFIG_CHANNEL` once.
//!
//! A request timeout closes the connection when the deadline passes, before
//! the task's 100 ms reconnect sleep, and not before the deadline. The server
//! answers the precheck `PING`, and the first `answered` other commands on each
//! connection after `reply_delay`, and records how long after its first
//! unanswered request the client's FIN arrived. A client that left the close
//! to the drop after the sleep would take the timeout plus 100 ms; one that
//! closed without waiting for the deadline would take less than the timeout.

use cachecannon::{ColorMode, Config, OutputFormat, create_formatter, run_benchmark_full};
use std::io::{BufRead, BufReader, Read};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const REQUEST_TIMEOUT: Duration = Duration::from_millis(100);
const RECONNECT_SLEEP: Duration = Duration::from_millis(100);
const CONNECTIONS: usize = 2;

/// Read one CRLF-terminated line, without the CRLF. `None` at EOF.
fn read_line(reader: &mut impl BufRead) -> Option<Vec<u8>> {
    let mut line = Vec::new();
    if reader.read_until(b'\n', &mut line).ok()? == 0 {
        return None;
    }
    line.truncate(line.len().saturating_sub(2));
    Some(line)
}

/// Answer `PING` with `+PONG`, and the first `answered` other commands after
/// `reply_delay` (`GET` with a miss, anything else with `+OK`). Returns the
/// time from the first unanswered command to EOF and the time of EOF, or
/// `None` if every command was answered.
fn serve(stream: TcpStream, answered: usize, reply_delay: Duration) -> Option<(Duration, Instant)> {
    use std::io::Write;
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    let mut first_unanswered = None;
    let mut others = 0;
    while let Some(header) = read_line(&mut reader) {
        let Ok(n) = std::str::from_utf8(header.get(1..)?)
            .unwrap_or("")
            .parse::<usize>()
        else {
            break;
        };
        let mut name = Vec::new();
        for i in 0..n {
            let len_line = read_line(&mut reader)?;
            let len: usize = std::str::from_utf8(&len_line[1..]).ok()?.parse().ok()?;
            let mut arg = vec![0u8; len + 2];
            reader.read_exact(&mut arg).ok()?;
            if i == 0 {
                arg.truncate(len);
                name = arg.to_ascii_uppercase();
            }
        }
        let reply: &[u8] = if name == b"PING" {
            b"+PONG\r\n"
        } else if others < answered {
            others += 1;
            std::thread::sleep(reply_delay);
            if name == b"GET" {
                b"$-1\r\n"
            } else {
                b"+OK\r\n"
            }
        } else {
            b""
        };
        if !reply.is_empty() {
            if writer.write_all(reply).is_err() {
                break;
            }
        } else if first_unanswered.is_none() {
            first_unanswered = Some(Instant::now());
        }
    }
    let eof = Instant::now();
    first_unanswered.map(|t| (eof - t, eof))
}

fn spawn(
    closes: Arc<Mutex<Vec<(Duration, Instant)>>>,
    answered: usize,
    reply_delay: Duration,
) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let closes = Arc::clone(&closes);
            std::thread::spawn(move || {
                if let Some(d) = serve(stream, answered, reply_delay) {
                    closes.lock().unwrap().push(d);
                }
            });
        }
    });
    addr
}

/// Run 2 s against a server that answers `answered` commands per connection,
/// each after `reply_delay`, and return each connection's time from first
/// unanswered request to FIN, sorted. Depth 1: each request is fired after the
/// previous reply, so a delayed reply leaves the next request's deadline after
/// the running timer's.
pub fn closes(answered: usize, reply_delay: Duration) -> Vec<Duration> {
    let closes = Arc::new(Mutex::new(Vec::new()));
    let addr = spawn(Arc::clone(&closes), answered, reply_delay);
    let toml = format!(
        r#"
        [general]
        duration = "2s"
        warmup = "0s"
        threads = 1

        [target]
        endpoints = ["{addr}"]
        protocol = "resp"

        [connection]
        connections = {CONNECTIONS}
        pipeline_depth = 1
        request_timeout = "{}ms"

        [workload.keyspace]
        count = 100

        [admin]
        format = "quiet"
        "#,
        REQUEST_TIMEOUT.as_millis()
    );
    let path = std::env::temp_dir().join(format!(
        "cachecannon-close-on-request-timeout-{}.toml",
        std::process::id()
    ));
    std::fs::write(&path, toml).unwrap();
    let config = Config::load(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    // Every connection ends in a timeout; the outcome of the run is not what is
    // tested.
    let _ = run_benchmark_full(
        config,
        None,
        create_formatter(OutputFormat::Quiet, ColorMode::Never),
        Arc::new(AtomicBool::new(true)),
    );
    // The connections open when the run ends are closed by the shutdown,
    // whatever their requests' age, after every timeout close: drop the last
    // `CONNECTIONS` FINs. If a connection was in its reconnect sleep at the
    // end, this drops a timeout close instead, which costs one sample.
    let mut closes = closes.lock().unwrap().clone();
    closes.sort_by_key(|&(_, eof)| eof);
    closes.truncate(closes.len().saturating_sub(CONNECTIONS));
    let mut closes: Vec<Duration> = closes.into_iter().map(|(d, _)| d).collect();
    closes.sort();
    closes
}

/// The connections closed at the deadline: none before it, and the median
/// before the reconnect sleep would have added 100 ms.
pub fn assert_closed_at_the_deadline(closes: &[Duration]) {
    // About 2 s at one timeout and one reconnect per 200 ms per connection.
    assert!(
        closes.len() >= 6,
        "only {} connections timed out: {closes:?}",
        closes.len()
    );
    // The server stamps a request when it reads it, after the client stamped
    // it, so a close at the deadline is at least the timeout less the time from
    // the client's stamp to the server's read (send, loopback, thread
    // wake-up), allowed 25 ms here.
    assert!(
        closes[0] + Duration::from_millis(25) >= REQUEST_TIMEOUT,
        "a connection closed {:?} after its first unanswered request, before the {REQUEST_TIMEOUT:?} timeout: {closes:?}",
        closes[0]
    );
    // The median is used so that one stalled close on a busy host does not fail
    // the test. Closed at the deadline, it is the timeout plus scheduling; left to
    // the drop after the reconnect sleep, it is at least the timeout plus 100 ms.
    let median = closes[closes.len() / 2];
    assert!(
        median < REQUEST_TIMEOUT + RECONNECT_SLEEP / 2,
        "median time from first unanswered request to FIN was {median:?}: {closes:?}"
    );
}
