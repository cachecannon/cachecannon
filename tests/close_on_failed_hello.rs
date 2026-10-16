//! A connection whose `HELLO 3` is refused is closed before the task
//! reconnects. The server here answers `HELLO` with an error and counts how
//! many connections it has open at once; with two connections configured, a
//! task that reconnected without closing would leave one more open every
//! 100 ms until precheck gives up.

use cachecannon::{ColorMode, Config, OutputFormat, create_formatter, run_benchmark_full};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[derive(Default)]
struct Open {
    now: AtomicUsize,
    peak: AtomicUsize,
    accepted: AtomicUsize,
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

/// Answer `HELLO` with an error and anything else with `+OK`, until EOF.
fn serve(stream: TcpStream, open: &Open) {
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    while let Some(header) = read_line(&mut reader) {
        let n: usize = std::str::from_utf8(&header[1..]).unwrap().parse().unwrap();
        let mut name = Vec::new();
        for i in 0..n {
            let Some(len_line) = read_line(&mut reader) else {
                break;
            };
            let len: usize = std::str::from_utf8(&len_line[1..])
                .unwrap()
                .parse()
                .unwrap();
            let mut arg = vec![0u8; len + 2];
            if reader.read_exact(&mut arg).is_err() {
                break;
            }
            if i == 0 {
                arg.truncate(len);
                name = arg.to_ascii_uppercase();
            }
        }
        let reply: &[u8] = if name == b"HELLO" {
            b"-ERR unknown command 'HELLO'\r\n"
        } else {
            b"+OK\r\n"
        };
        if writer.write_all(reply).is_err() {
            break;
        }
    }
    open.now.fetch_sub(1, Ordering::SeqCst);
}

fn spawn(open: Arc<Open>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            open.accepted.fetch_add(1, Ordering::SeqCst);
            let now = open.now.fetch_add(1, Ordering::SeqCst) + 1;
            open.peak.fetch_max(now, Ordering::SeqCst);
            let open = Arc::clone(&open);
            std::thread::spawn(move || serve(stream, &open));
        }
    });
    addr
}

#[test]
fn a_refused_hello_closes_the_connection() {
    let open = Arc::new(Open::default());
    let addr = spawn(Arc::clone(&open));
    let toml = format!(
        r#"
        [general]
        duration = "1s"
        warmup = "0s"
        threads = 1

        [target]
        endpoints = ["{addr}"]
        protocol = "resp3"

        [connection]
        connections = 2
        connect_timeout = "1500ms"

        [admin]
        format = "quiet"
        "#
    );
    let path = std::env::temp_dir().join(format!(
        "cachecannon-close-on-failed-hello-{}.toml",
        std::process::id()
    ));
    std::fs::write(&path, toml).unwrap();
    let config = Config::load(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    // Precheck never completes, because no connection gets past HELLO.
    let result = run_benchmark_full(
        config,
        None,
        create_formatter(OutputFormat::Quiet, ColorMode::Never),
        Arc::new(AtomicBool::new(true)),
    );
    assert!(result.is_err(), "precheck should fail");

    let accepted = open.accepted.load(Ordering::SeqCst);
    let peak = open.peak.load(Ordering::SeqCst);
    // About 1.5 s at one attempt per 100 ms per connection.
    assert!(accepted >= 10, "only {accepted} connections were attempted");
    // Two configured connections, plus one each for a close the server has not
    // read yet when the next connect arrives.
    assert!(
        peak <= 4,
        "{peak} connections open at once out of {accepted} accepted"
    );
}
