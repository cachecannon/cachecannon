//! RESP connections send `AUTH` before anything else, and a server that
//! refuses the handshake fails the precheck at once, naming the endpoint.
//!
//! The fake server here behaves like a Valkey server with `requirepass`:
//! until a connection authenticates, every command but `AUTH` gets `NOAUTH`.
//! It records each connection's command names.

mod common;

use cachecannon::{ColorMode, Config, OutputFormat, create_formatter, run_benchmark_full};
use common::kv::isolated;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// When the first connection arrived. The run spends seconds filling its value
/// pool before it connects, so precheck time is measured from here.
static FIRST_CONNECTION: OnceLock<Instant> = OnceLock::new();

/// Time since the first connection arrived.
fn since_first_connection() -> Duration {
    FIRST_CONNECTION
        .get()
        .expect("no connection arrived")
        .elapsed()
}

/// Command names received, one list per connection.
type Log = Arc<Mutex<Vec<Vec<String>>>>;

/// Read one CRLF-terminated line, without the CRLF. `None` at EOF.
fn read_line(reader: &mut impl BufRead) -> Option<Vec<u8>> {
    let mut line = Vec::new();
    if reader.read_until(b'\n', &mut line).ok()? == 0 {
        return None;
    }
    line.truncate(line.len().saturating_sub(2));
    Some(line)
}

/// Read one RESP array command. `None` at EOF.
fn read_command(reader: &mut impl BufRead) -> Option<Vec<Vec<u8>>> {
    let header = read_line(reader)?;
    let n: usize = std::str::from_utf8(&header[1..]).ok()?.parse().ok()?;
    let mut args = Vec::with_capacity(n);
    for _ in 0..n {
        let len_line = read_line(reader)?;
        let len: usize = std::str::from_utf8(&len_line[1..]).ok()?.parse().ok()?;
        let mut arg = vec![0u8; len + 2];
        reader.read_exact(&mut arg).ok()?;
        arg.truncate(len);
        args.push(arg);
    }
    Some(args)
}

/// Serve one connection. `password` is what `AUTH` must carry as its last
/// argument; `None` makes every `AUTH` fail with `WRONGPASS`.
fn serve(stream: TcpStream, password: Option<&'static str>, log: Log) {
    let idx = {
        let mut log = log.lock().unwrap();
        log.push(Vec::new());
        log.len() - 1
    };
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    let mut authed = false;
    while let Some(args) = read_command(&mut reader) {
        let name = String::from_utf8_lossy(&args[0]).to_ascii_uppercase();
        log.lock().unwrap()[idx].push(name.clone());
        let reply: &[u8] = match name.as_str() {
            "AUTH" if password.is_some_and(|p| args.last().unwrap() == p.as_bytes()) => {
                authed = true;
                b"+OK\r\n"
            }
            "AUTH" => b"-WRONGPASS invalid username-password pair or user is disabled.\r\n",
            _ if !authed => b"-NOAUTH Authentication required.\r\n",
            "GET" => b"$-1\r\n",
            _ => b"+OK\r\n",
        };
        if writer.write_all(reply).is_err() {
            break;
        }
    }
}

fn spawn(password: Option<&'static str>) -> (SocketAddr, Log) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let log: Log = Arc::default();
    let server_log = Arc::clone(&log);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            FIRST_CONNECTION.get_or_init(Instant::now);
            let log = Arc::clone(&server_log);
            std::thread::spawn(move || serve(stream, password, log));
        }
    });
    (addr, log)
}

fn run(endpoints: &[SocketAddr], extra_target: &str) -> Result<(), String> {
    let endpoints = endpoints
        .iter()
        .map(|e| format!("\"{e}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let toml = format!(
        r#"
        [general]
        duration = "1s"
        warmup = "0s"
        threads = 1

        [target]
        endpoints = [{endpoints}]
        {extra_target}

        [connection]
        connections = 2
        connect_timeout = "10s"

        [workload.keyspace]
        count = 100

        [admin]
        format = "quiet"
        "#
    );
    let path =
        std::env::temp_dir().join(format!("cachecannon-resp-auth-{}.toml", std::process::id()));
    std::fs::write(&path, toml).unwrap();
    let config = Config::load(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    run_benchmark_full(
        config,
        None,
        create_formatter(OutputFormat::Quiet, ColorMode::Never),
        Arc::new(AtomicBool::new(true)),
    )
    .map(|_| ())
    .map_err(|e| e.to_string())
}

#[test]
fn auth_precedes_hello_and_ping() {
    isolated("auth_precedes_hello_and_ping", || {
        let (addr, log) = spawn(Some("pw"));
        run(
            &[addr],
            "protocol = \"resp3\"\nauth_user = \"bench\"\nauth_password = \"pw\"",
        )
        .unwrap();
        let log = log.lock().unwrap();
        assert!(!log.is_empty());
        for conn in log.iter() {
            assert_eq!(conn.first().map(String::as_str), Some("AUTH"), "{conn:?}");
            assert_eq!(conn.get(1).map(String::as_str), Some("HELLO"), "{conn:?}");
        }
        assert!(log.iter().any(|c| c.iter().any(|n| n == "PING")), "{log:?}");
    });
}

#[test]
fn wrong_password_fails_without_retrying() {
    isolated("wrong_password_fails_without_retrying", || {
        let (addr, log) = spawn(None);
        let err = run(&[addr], "auth_password = \"nope\"").unwrap_err();
        // Well inside connect_timeout (10 s).
        let took = since_first_connection();
        assert!(took < Duration::from_secs(3), "{took:?}");
        assert!(err.contains("the server rejected AUTH"), "{err}");
        assert!(err.contains("WRONGPASS"), "{err}");
        assert!(!err.contains("nope"), "{err}");
        // One AUTH per configured connection, no retries.
        let auths = log
            .lock()
            .unwrap()
            .iter()
            .flatten()
            .filter(|n| *n == "AUTH")
            .count();
        assert!(auths <= 2, "{auths} AUTH attempts");
    });
}

#[test]
fn missing_password_reports_noauth() {
    isolated("missing_password_reports_noauth", || {
        let (addr, _log) = spawn(Some("pw"));
        let err = run(&[addr], "").unwrap_err();
        let took = since_first_connection();
        assert!(took < Duration::from_secs(3), "{took:?}");
        assert!(err.contains("the server requires AUTH"), "{err}");
        assert!(err.contains("CACHECANNON_AUTH_PASSWORD"), "{err}");
    });
}

#[test]
fn one_rejecting_endpoint_fails_the_precheck() {
    isolated("one_rejecting_endpoint_fails_the_precheck", || {
        let (good, _) = spawn(Some("pw"));
        let (bad, _) = spawn(None);
        let err = run(&[good, bad], "auth_password = \"pw\"").unwrap_err();
        assert!(err.contains("1 of 2 endpoints rejected AUTH"), "{err}");
        assert!(err.contains(&bad.to_string()), "{err}");
        assert!(!err.contains(&good.to_string()), "{err}");
    });
}
