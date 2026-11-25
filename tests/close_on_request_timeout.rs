//! A request timeout closes the connection when the deadline passes, before
//! the task's 100 ms reconnect sleep. The server answers the precheck `PING`
//! and never answers anything else, and records for each connection how long
//! after its first unanswered request the client's FIN arrived. A client that
//! left the close to the drop after the sleep would take the timeout plus
//! 100 ms.

use cachecannon::{ColorMode, Config, OutputFormat, create_formatter, run_benchmark_full};
use std::io::{BufRead, BufReader, Read};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const REQUEST_TIMEOUT: Duration = Duration::from_millis(100);
const RECONNECT_SLEEP: Duration = Duration::from_millis(100);

/// Read one CRLF-terminated line, without the CRLF. `None` at EOF.
fn read_line(reader: &mut impl BufRead) -> Option<Vec<u8>> {
    let mut line = Vec::new();
    if reader.read_until(b'\n', &mut line).ok()? == 0 {
        return None;
    }
    line.truncate(line.len().saturating_sub(2));
    Some(line)
}

/// Answer `PING` with `+PONG` and nothing else. Returns the time from the first
/// other command to EOF, or `None` if no other command arrived.
fn serve(stream: TcpStream) -> Option<Duration> {
    use std::io::Write;
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    let mut first_unanswered = None;
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
        if name == b"PING" {
            if writer.write_all(b"+PONG\r\n").is_err() {
                break;
            }
        } else if first_unanswered.is_none() {
            first_unanswered = Some(Instant::now());
        }
    }
    first_unanswered.map(|t| t.elapsed())
}

fn spawn(closes: Arc<Mutex<Vec<Duration>>>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let closes = Arc::clone(&closes);
            std::thread::spawn(move || {
                if let Some(d) = serve(stream) {
                    closes.lock().unwrap().push(d);
                }
            });
        }
    });
    addr
}

#[test]
fn a_request_timeout_closes_the_connection_at_the_deadline() {
    let closes = Arc::new(Mutex::new(Vec::new()));
    let addr = spawn(Arc::clone(&closes));
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
        connections = 2
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
    // Every request times out; the outcome of the run is not what is tested.
    let _ = run_benchmark_full(
        config,
        None,
        create_formatter(OutputFormat::Quiet, ColorMode::Never),
        Arc::new(AtomicBool::new(true)),
    );

    let mut closes = closes.lock().unwrap().clone();
    closes.sort();
    // About 2 s at one timeout and one reconnect per 200 ms per connection.
    assert!(
        closes.len() >= 6,
        "only {} connections timed out: {closes:?}",
        closes.len()
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
