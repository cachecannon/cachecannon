//! `[resp] module = "largeobj"` sends the module's TCP commands. A fake RESP
//! server records the name and argument count of every command it receives and
//! answers as a server with the module would: `BLOB.TCP_GET key` with a value,
//! `BLOB.TCP_SET key value` with `+OK`, any other argument count with an
//! arity error, a plain `GET` with `WRONGTYPE`, and a plain `SET` with `+OK`
//! (a bare SET replaces a key of any type). After a RESP3 run with a prefill
//! and a GET/SET mix, only `HELLO`, `PING`, `BLOB.TCP_GET` with 2 arguments and
//! `BLOB.TCP_SET` with 3 may have arrived.
//!
//! The runner can be started once per process, so this is its own test binary.

use cachecannon::{ColorMode, Config, OutputFormat, create_formatter, run_benchmark_full};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

/// Request count per (command name, argument count including the name).
type Seen = Arc<Mutex<BTreeMap<(Vec<u8>, usize), usize>>>;

fn read_line(reader: &mut impl BufRead) -> Option<Vec<u8>> {
    let mut line = Vec::new();
    if reader.read_until(b'\n', &mut line).ok()? == 0 {
        return None;
    }
    line.truncate(line.len().saturating_sub(2));
    Some(line)
}

/// Count each command by name and argument count and answer it, until EOF.
fn serve(stream: TcpStream, seen: &Mutex<BTreeMap<(Vec<u8>, usize), usize>>) {
    stream.set_nodelay(true).unwrap();
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    while let Some(header) = read_line(&mut reader) {
        let n: usize = std::str::from_utf8(&header[1..]).unwrap().parse().unwrap();
        let mut name = Vec::new();
        for i in 0..n {
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
            if i == 0 {
                arg.truncate(len);
                name = arg.to_ascii_uppercase();
            }
        }
        *seen.lock().unwrap().entry((name.clone(), n)).or_default() += 1;
        let reply: &[u8] = match (name.as_slice(), n) {
            (b"BLOB.TCP_GET", 2) => b"$5\r\nhello\r\n",
            (b"BLOB.TCP_SET", 3) => b"+OK\r\n",
            (b"BLOB.TCP_GET" | b"BLOB.TCP_SET", _) => {
                b"-ERR wrong number of arguments for 'blob.tcp_*' command\r\n"
            }
            (b"PING", _) => b"+PONG\r\n",
            (b"GET", _) => {
                b"-WRONGTYPE Operation against a key holding the wrong kind of value\r\n"
            }
            _ => b"+OK\r\n",
        };
        if writer.write_all(reply).is_err() {
            return;
        }
    }
}

fn spawn() -> (SocketAddr, Seen) {
    let seen: Seen = Arc::default();
    let recorded = Arc::clone(&seen);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let recorded = Arc::clone(&recorded);
            std::thread::spawn(move || serve(stream, &recorded));
        }
    });
    (addr, seen)
}

#[test]
fn largeobj_module_sends_only_tcp_commands() {
    let (addr, seen) = spawn();
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
        pipeline_depth = 4

        [workload]
        rate_limit = 2000
        prefill = true
        prefill_settle = "0s"

        [workload.keyspace]
        count = 64

        [workload.commands]
        get = 80
        set = 20

        [resp]
        module = "largeobj"

        [admin]
        format = "quiet"
        "#
    );
    let path = std::env::temp_dir().join(format!(
        "cachecannon-resp-module-tcp-commands-{}.toml",
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

    let seen = seen.lock().unwrap().clone();
    let names: Vec<String> = seen
        .keys()
        .map(|(k, n)| format!("{} ({n} args)", String::from_utf8_lossy(k)))
        .collect();
    let allowed = |name: &[u8], args: usize| match name {
        b"HELLO" | b"PING" => true,
        b"BLOB.TCP_GET" => args == 2,
        b"BLOB.TCP_SET" => args == 3,
        _ => false,
    };
    assert!(
        seen.keys().all(|(k, n)| allowed(k, *n)),
        "unexpected commands: {names:?}"
    );
    let count = |name: &[u8]| -> usize {
        seen.iter()
            .filter(|((k, _), _)| k.as_slice() == name)
            .map(|(_, c)| c)
            .sum()
    };
    // The prefill alone sends one BLOB.TCP_SET per key, and the measured run
    // is 80% BLOB.TCP_GET.
    let sets = count(b"BLOB.TCP_SET");
    let gets = count(b"BLOB.TCP_GET");
    assert!(
        sets >= 64,
        "{sets} BLOB.TCP_SET for 64 prefilled keys: {names:?}"
    );
    assert!(gets > 0, "no BLOB.TCP_GET: {names:?}");
    // HELLO 3 on every connection.
    assert!(count(b"HELLO") > 0, "no HELLO: {names:?}");
}
