//! Shared by the `read_values_*` and `prefill_rate_limit_*` tests: fake
//! servers and short benchmark runs against them. `kv` is shared by
//! `kv_replay.rs`.
//!
//! The runner can be started once per process (`init_config_channel`), and the
//! metrics it records are process-global, so each `read_values_*` run is its
//! own test binary. `kv_replay.rs` runs each test in a child process instead
//! (`kv::isolated`).

// Each test binary uses part of this module.
#![allow(dead_code)]

pub mod kv;

use cachecannon::metrics::{CACHE_HITS, CACHE_MISSES, GET_COUNT, VALUES_READ_BYTES};
use cachecannon::{ColorMode, Config, OutputFormat, create_formatter, run_benchmark_full};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Every third GET on a connection is answered with a miss.
pub const MISS_EVERY: u64 = 3;

/// Large enough to span many receive buffers: io_uring's provided buffers are
/// 16 KiB by default and mio reads 8 KiB at a time.
pub const LARGE_VALUE_LEN: usize = 256 * 1024;

/// What the fake servers return: a value of `value_len` bytes for each GET,
/// except every [`MISS_EVERY`]th GET on a connection, which misses.
#[derive(Clone, Copy)]
pub struct Server {
    pub value_len: usize,
}

impl Server {
    fn value(self) -> Vec<u8> {
        (0..self.value_len).map(|i| b'a' + (i % 26) as u8).collect()
    }

    pub fn spawn_resp(self) -> SocketAddr {
        spawn(move |stream| serve_resp(stream, self))
    }

    pub fn spawn_memcache(self) -> SocketAddr {
        spawn(move |stream| serve_memcache(stream, self))
    }
}

/// Accept connections forever, serving each on its own thread.
fn spawn(serve: impl Fn(TcpStream) + Clone + Send + 'static) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let serve = serve.clone();
            std::thread::spawn(move || serve(stream));
        }
    });
    addr
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

/// A RESP2 server: GET hits or misses per [`Server`], DEL `:1`, PING `+PONG`,
/// anything else `+OK`.
fn serve_resp(stream: TcpStream, server: Server) {
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    let mut hit_reply = format!("${}\r\n", server.value_len).into_bytes();
    hit_reply.extend_from_slice(&server.value());
    hit_reply.extend_from_slice(b"\r\n");
    let mut gets = 0u64;
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
        let reply: &[u8] = match args[0].to_ascii_uppercase().as_slice() {
            b"GET" => {
                gets += 1;
                if gets.is_multiple_of(MISS_EVERY) {
                    b"$-1\r\n"
                } else {
                    &hit_reply
                }
            }
            b"PING" => b"+PONG\r\n",
            b"DEL" => b":1\r\n",
            _ => b"+OK\r\n",
        };
        if writer.write_all(reply).is_err() {
            return;
        }
    }
}

/// A memcache ASCII server: `get` hits or misses per [`Server`], `set` is
/// `STORED`.
fn serve_memcache(stream: TcpStream, server: Server) {
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    let value = server.value();
    let mut gets = 0u64;
    while let Some(line) = read_line(&mut reader) {
        let mut words = line.split(|b| *b == b' ');
        let reply = match words.next().unwrap() {
            b"get" => {
                gets += 1;
                if gets.is_multiple_of(MISS_EVERY) {
                    b"END\r\n".to_vec()
                } else {
                    let key = words.next().unwrap();
                    let mut reply = b"VALUE ".to_vec();
                    reply.extend_from_slice(key);
                    reply.extend_from_slice(format!(" 0 {}\r\n", server.value_len).as_bytes());
                    reply.extend_from_slice(&value);
                    reply.extend_from_slice(b"\r\nEND\r\n");
                    reply
                }
            }
            b"set" => {
                let len: usize = std::str::from_utf8(line.rsplit(|b| *b == b' ').next().unwrap())
                    .unwrap()
                    .parse()
                    .unwrap();
                let mut data = vec![0u8; len + 2];
                reader.read_exact(&mut data).unwrap();
                b"STORED\r\n".to_vec()
            }
            b"version" => b"VERSION 1.0\r\n".to_vec(),
            _ => b"ERROR\r\n".to_vec(),
        };
        if writer.write_all(&reply).is_err() {
            return;
        }
    }
}

/// What one run recorded.
#[derive(Debug)]
pub struct Counts {
    pub gets: u64,
    pub hits: u64,
    pub misses: u64,
    pub read: u64,
}

/// Run a one-second, GET-only benchmark against `addr`.
pub fn run(addr: SocketAddr, protocol: &str, read_values: bool) -> Counts {
    let toml = format!(
        r#"
        [general]
        duration = "1s"
        warmup = "0s"
        threads = 1

        [target]
        endpoints = ["{addr}"]
        protocol = "{protocol}"

        [connection]
        connections = 2
        pipeline_depth = 4

        [workload]
        read_values = {read_values}

        [workload.keyspace]
        count = 100

        [workload.commands]
        get = 100
        set = 0

        [admin]
        format = "quiet"
        "#
    );
    let path = std::env::temp_dir().join(format!(
        "cachecannon-read-values-{}-{protocol}-{read_values}.toml",
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
    Counts {
        gets: GET_COUNT.value(),
        hits: CACHE_HITS.value(),
        misses: CACHE_MISSES.value(),
        read: VALUES_READ_BYTES.value(),
    }
}

/// Run with `read_values = true` and check that every hit, and nothing else,
/// was read: some GETs missed, and bytes read are hits times the value length.
pub fn assert_every_hit_read(protocol: &str, server: Server) {
    let addr = match protocol {
        "resp" => server.spawn_resp(),
        "memcache" => server.spawn_memcache(),
        _ => unreachable!(),
    };
    let c = run(addr, protocol, true);
    assert!(c.hits > 0, "no hits recorded: {c:?}");
    assert!(c.misses > 0, "no misses recorded: {c:?}");
    assert_eq!(
        c.hits + c.misses,
        c.gets,
        "a GET was neither hit nor miss: {c:?}"
    );
    assert_eq!(
        c.read,
        c.hits * server.value_len as u64,
        "{} hits of {} bytes, but {} bytes read",
        c.hits,
        server.value_len,
        c.read
    );
}

/// A server for `protocol` (resp or memcache) that stores nothing, answers
/// every SET as stored and every GET as a miss, and records when each SET
/// arrived.
pub fn spawn_set_recorder(protocol: &str) -> (SocketAddr, Arc<Mutex<Vec<Instant>>>) {
    let sets = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&sets);
    let addr = match protocol {
        "resp" => spawn(move |stream| record_resp(stream, &recorded)),
        "memcache" => spawn(move |stream| record_memcache(stream, &recorded)),
        _ => unreachable!(),
    };
    (addr, sets)
}

fn record_resp(stream: TcpStream, sets: &Mutex<Vec<Instant>>) {
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
        let reply: &[u8] = match command.as_slice() {
            b"SET" => {
                sets.lock().unwrap().push(Instant::now());
                b"+OK\r\n"
            }
            b"GET" => b"$-1\r\n",
            b"PING" => b"+PONG\r\n",
            _ => b"+OK\r\n",
        };
        if writer.write_all(reply).is_err() {
            return;
        }
    }
}

fn record_memcache(stream: TcpStream, sets: &Mutex<Vec<Instant>>) {
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    while let Some(line) = read_line(&mut reader) {
        let reply: &[u8] = match line.split(|b| *b == b' ').next().unwrap() {
            b"set" => {
                let len: usize = std::str::from_utf8(line.rsplit(|b| *b == b' ').next().unwrap())
                    .unwrap()
                    .parse()
                    .unwrap();
                let mut data = vec![0u8; len + 2];
                reader.read_exact(&mut data).unwrap();
                sets.lock().unwrap().push(Instant::now());
                b"STORED\r\n"
            }
            b"get" => b"END\r\n",
            b"version" => b"VERSION 1.0\r\n",
            _ => b"ERROR\r\n",
        };
        if writer.write_all(reply).is_err() {
            return;
        }
    }
}

/// The shape of a paced-prefill run: recording servers, threads,
/// connections (total) and pipeline depth.
#[derive(Default)]
pub struct Shape {
    pub endpoints: usize,
    pub threads: usize,
    pub connections: usize,
    pub pipeline: usize,
    /// Extra lines for `[workload.keyspace]`.
    pub keyspace: &'static str,
    /// Extra lines for `[connection]`.
    pub connection: &'static str,
}

/// Prefill `keys` keys at `rate` requests per second against recording
/// servers for `protocol`, and return how long the SETs took from the first to
/// the last across every server. Fails unless every key was written once.
pub fn prefill_span_shaped(protocol: &str, shape: Shape, keys: usize, rate: usize) -> Duration {
    let times = prefill_times_shaped(protocol, shape, keys, rate);
    *times.last().unwrap() - *times.first().unwrap()
}

/// Split sorted SET arrival times into ten equal runs of keys and return each
/// run's span, shortest first. A stall on a busy test host lengthens a few
/// runs; the shortest runs still show the paced rate.
pub fn decile_spans(times: &[Instant]) -> Vec<Duration> {
    let n = times.len() / 10;
    assert!(n >= 2, "too few SETs to split: {}", times.len());
    let mut spans: Vec<Duration> = (0..10)
        .map(|d| times[(d + 1) * n - 1] - times[d * n])
        .collect();
    spans.sort();
    spans
}

/// As [`prefill_span_shaped`], but return every SET's arrival time, sorted.
pub fn prefill_times_shaped(
    protocol: &str,
    shape: Shape,
    keys: usize,
    rate: usize,
) -> Vec<Instant> {
    let servers: Vec<_> = (0..shape.endpoints)
        .map(|_| spawn_set_recorder(protocol))
        .collect();
    let endpoints = servers
        .iter()
        .map(|(addr, _)| format!("\"{addr}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let toml = format!(
        r#"
        [general]
        duration = "1s"
        warmup = "0s"
        threads = {threads}

        [target]
        endpoints = [{endpoints}]
        protocol = "{protocol}"

        [connection]
        connections = {connections}
        pipeline_depth = {pipeline}
        {connection_extra}

        [workload]
        prefill = true
        prefill_settle = "0s"
        prefill_rate_limit = {rate}

        [workload.keyspace]
        count = {keys}
        {keyspace_extra}

        [workload.commands]
        get = 100
        set = 0

        [admin]
        format = "quiet"
        "#,
        connection_extra = shape.connection,
        keyspace_extra = shape.keyspace,
        threads = shape.threads,
        connections = shape.connections,
        pipeline = shape.pipeline,
    );
    let path = std::env::temp_dir().join(format!(
        "cachecannon-prefill-rate-{}-{protocol}.toml",
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
    let mut times: Vec<Instant> = Vec::new();
    for (_, sets) in &servers {
        times.extend(sets.lock().unwrap().iter().copied());
    }
    assert_eq!(times.len(), keys, "every key is written once by prefill");
    times.sort();
    times
}

/// [`prefill_span_shaped`] with one server, 2 threads, 8 connections and a
/// pipeline of 4.
pub fn prefill_span(protocol: &str, keys: usize, rate: usize) -> Duration {
    let shape = Shape {
        endpoints: 1,
        threads: 2,
        connections: 8,
        pipeline: 4,
        ..Default::default()
    };
    prefill_span_shaped(protocol, shape, keys, rate)
}

/// A server for `protocol` (resp or memcache) that answers every GET as a
/// miss, after `delay`, and counts the GETs it received.
pub fn spawn_get_counter(
    protocol: &str,
    delay: Duration,
) -> (SocketAddr, Arc<std::sync::atomic::AtomicU64>) {
    spawn_paced(protocol, Pace::Serial(delay))
}

/// How a counting server times its GET replies.
#[derive(Clone, Copy)]
pub enum Pace {
    /// Each GET takes this long, one at a time.
    Serial(Duration),
    /// Each GET's reply is written this long after the GET was read, however
    /// many are in flight. Replies keep their order.
    FromArrival(Duration),
}

/// A server like [`spawn_get_counter`]'s, with GET replies timed by `pace`.
pub fn spawn_paced(protocol: &str, pace: Pace) -> (SocketAddr, Arc<std::sync::atomic::AtomicU64>) {
    let gets = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let counted = Arc::clone(&gets);
    let addr = match protocol {
        "resp" => spawn(move |stream| count_resp(stream, &counted, pace)),
        "memcache" => spawn(move |stream| count_memcache(stream, &counted, pace)),
        _ => unreachable!(),
    };
    (addr, gets)
}

/// Writes a counting server's replies in order, timed by its [`Pace`].
enum Replies {
    Serial(TcpStream),
    /// Replies with the time to write each, for a writer thread.
    FromArrival(std::sync::mpsc::Sender<(std::time::Instant, &'static [u8])>),
}

impl Replies {
    fn new(stream: &TcpStream, pace: Pace) -> Self {
        let mut writer = stream.try_clone().unwrap();
        match pace {
            Pace::Serial(_) => Replies::Serial(writer),
            Pace::FromArrival(_) => {
                // With Nagle on, a reply written while an earlier one is
                // unacknowledged is held for the ACK, which the client
                // delays when it has nothing to send. On Linux, at pipeline 4
                // and batch 4 (then the default), the control read 650 ms p50
                // queue wait with Nagle on and 6 ms with it off (one run
                // each, no packet capture; attributed, not traced).
                writer.set_nodelay(true).unwrap();
                let (tx, rx) = std::sync::mpsc::channel::<(std::time::Instant, &'static [u8])>();
                std::thread::spawn(move || {
                    for (at, reply) in rx {
                        std::thread::sleep(at.saturating_duration_since(std::time::Instant::now()));
                        if writer.write_all(reply).is_err() {
                            return;
                        }
                    }
                });
                Replies::FromArrival(tx)
            }
        }
    }

    /// Write `reply` after `delay`. False once the client has gone.
    fn send(&mut self, reply: &'static [u8], delay: Duration) -> bool {
        match self {
            Replies::Serial(writer) => {
                std::thread::sleep(delay);
                writer.write_all(reply).is_ok()
            }
            Replies::FromArrival(tx) => tx.send((std::time::Instant::now() + delay, reply)).is_ok(),
        }
    }
}

fn count_resp(stream: TcpStream, gets: &std::sync::atomic::AtomicU64, pace: Pace) {
    let (Pace::Serial(delay) | Pace::FromArrival(delay)) = pace;
    let mut replies = Replies::new(&stream, pace);
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
        let sent = match command.as_slice() {
            b"GET" => {
                gets.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                replies.send(b"$-1\r\n", delay)
            }
            b"PING" => replies.send(b"+PONG\r\n", Duration::ZERO),
            _ => replies.send(b"+OK\r\n", Duration::ZERO),
        };
        if !sent {
            return;
        }
    }
}

fn count_memcache(stream: TcpStream, gets: &std::sync::atomic::AtomicU64, pace: Pace) {
    let (Pace::Serial(delay) | Pace::FromArrival(delay)) = pace;
    let mut replies = Replies::new(&stream, pace);
    let mut reader = BufReader::new(stream);
    while let Some(line) = read_line(&mut reader) {
        let sent = match line.split(|b| *b == b' ').next().unwrap() {
            b"get" => {
                gets.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                replies.send(b"END\r\n", delay)
            }
            b"version" => replies.send(b"VERSION 1.0\r\n", Duration::ZERO),
            _ => replies.send(b"ERROR\r\n", Duration::ZERO),
        };
        if !sent {
            return;
        }
    }
}

/// The shape of a routed run.
pub struct Routed<'a> {
    pub protocol: &'a str,
    pub endpoints: &'a [SocketAddr],
    pub threads: usize,
    pub connections: usize,
    pub pipeline: usize,
    /// `[connection] batch_size`; `None` leaves it unset.
    pub batch: Option<usize>,
    pub rate: u64,
    pub duration: &'a str,
    /// `[workload] routing`.
    pub routing: &'a str,
    /// Lines for `[workload.keyspace]`.
    pub keyspace: &'a str,
}

/// Run a GET-only, rate-limited benchmark of `shape`, with no warmup.
pub fn run_routed(shape: &Routed) {
    let endpoints = shape
        .endpoints
        .iter()
        .map(|addr| format!("\"{addr}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let toml = format!(
        r#"
        [general]
        duration = "{duration}"
        warmup = "0s"
        threads = {threads}

        [target]
        endpoints = [{endpoints}]
        protocol = "{protocol}"

        [connection]
        connections = {connections}
        pipeline_depth = {pipeline}
        {batch}

        [workload]
        rate_limit = {rate}
        routing = "{routing}"

        [workload.keyspace]
        {keyspace}

        [workload.commands]
        get = 100
        set = 0

        [admin]
        format = "quiet"
        "#,
        duration = shape.duration,
        threads = shape.threads,
        protocol = shape.protocol,
        connections = shape.connections,
        pipeline = shape.pipeline,
        batch = shape
            .batch
            .map_or(String::new(), |b| format!("batch_size = {b}")),
        rate = shape.rate,
        routing = shape.routing,
        keyspace = shape.keyspace,
    );
    let path = std::env::temp_dir().join(format!(
        "cachecannon-routed-{}-{}.toml",
        std::process::id(),
        shape.protocol
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
}

/// Run routed against three servers with zipf keys and check that each server
/// received its key share of the GETs. An even split, which is what pinned
/// routing gives, fails: the shares differ by more than the tolerance.
pub fn assert_routed_key_share(protocol: &str) {
    use cachecannon::keydist::KeyDist;

    const KEYS: usize = 1000;
    const THETA: f64 = 0.99;
    let shares = |endpoints: &[SocketAddr]| {
        let keys = Arc::new(cachecannon::worker::build_endpoint_keys(
            KEYS, 16, endpoints, &None,
        ));
        let dist = KeyDist::zipf(KEYS, THETA).with_endpoint_keys(&keys);
        dist.endpoint_sampler().unwrap().shares().to_vec()
    };
    let spread = |s: &[f64]| {
        s.iter().cloned().fold(f64::MIN, f64::max) - s.iter().cloned().fold(f64::MAX, f64::min)
    };
    // Placement follows the servers' addresses, and the ports are whatever
    // the OS hands out, so some placements split the keys nearly evenly. Pick
    // servers whose placement is clearly uneven, or an even split would pass.
    let (servers, endpoints, expected) = (0..100)
        .find_map(|_| {
            let servers: Vec<_> = (0..3)
                .map(|_| spawn_get_counter(protocol, Duration::ZERO))
                .collect();
            let endpoints: Vec<SocketAddr> = servers.iter().map(|(a, _)| *a).collect();
            let expected = shares(&endpoints);
            (spread(&expected) > 0.1).then_some((servers, endpoints, expected))
        })
        .expect("no uneven placement in 100 tries");
    // The hot node takes up to about 60% of the rate. Four connections per
    // node per worker keep each connection's share of it to about 150 req/s,
    // well within what the thread-per-connection test servers serve on a
    // loaded CI host, so the shares measure routing and not the hot node's
    // capacity.
    const RATE: u64 = 2000;
    const SECONDS: u64 = 3;
    run_routed(&Routed {
        protocol,
        endpoints: &endpoints,
        threads: 2,
        connections: 24,
        pipeline: 8,
        batch: None,
        rate: RATE,
        duration: &format!("{SECONDS}s"),
        routing: "routed",
        keyspace: "count = 1000\ndistribution = \"zipf\"\nzipf_theta = 0.99",
    });

    let counts: Vec<u64> = servers
        .iter()
        .map(|(_, g)| g.load(std::sync::atomic::Ordering::Relaxed))
        .collect();
    let dropped = cachecannon::metrics::ROUTED_QUEUE_DROPPED.value();
    assert_eq!(
        dropped, 0,
        "a node's queue filled, so the shares reflect its capacity, not routing \
         (counts {counts:?}, shares {expected:?})"
    );
    let total: u64 = counts.iter().sum();
    assert!(
        total * 10 > RATE * SECONDS * 8,
        "fewer than 80% of the offered GETs were received: {counts:?}"
    );
    for (i, (&c, &want)) in counts.iter().zip(&expected).enumerate() {
        let got = c as f64 / total as f64;
        assert!(
            (got - want).abs() < 0.03,
            "server {i} received {got:.3} of GETs, its key share is {want:.3} \
             (counts {counts:?}, shares {expected:?})"
        );
    }
}

fn p50_ns(h: &cachecannon::metrics::LatencyHistogram) -> u64 {
    let h = h.load().expect("histogram has samples");
    h.quantiles(&[0.5])
        .unwrap()
        .unwrap()
        .entries()
        .values()
        .next()
        .unwrap()
        .end()
}

/// Run routed against one server slower than the offered rate and check that
/// requests waited in the node's queue, that the wait is part of the reported
/// response latency, and that requests finding the queue full were dropped.
pub fn assert_routed_queue_wait(protocol: &str) {
    use cachecannon::metrics::{RESPONSE_LATENCY, ROUTED_QUEUE_DROPPED, ROUTED_QUEUE_WAIT};

    // One connection with a pipeline of one against a server that takes 2 ms
    // per GET serves about 500 requests a second; 2000 are offered.
    let (addr, _) = spawn_get_counter(protocol, Duration::from_millis(2));
    run_routed(&Routed {
        protocol,
        endpoints: &[addr],
        threads: 1,
        connections: 1,
        pipeline: 1,
        batch: None,
        rate: 2000,
        duration: "2s",
        routing: "routed",
        keyspace: "count = 1000",
    });

    assert!(
        ROUTED_QUEUE_DROPPED.value() > 0,
        "the queue holds 16 requests and fills, so some are dropped"
    );
    let wait = p50_ns(&ROUTED_QUEUE_WAIT);
    let latency = p50_ns(&RESPONSE_LATENCY);
    assert!(
        wait > 4_000_000,
        "a full queue of 16 at 2 ms each waits well over 4 ms, got p50 {wait} ns"
    );
    assert!(
        latency >= wait,
        "response latency (p50 {latency} ns) includes the queue wait (p50 {wait} ns)"
    );
}

/// The pipeline depth of [`routed_queue_wait_with_a_reply_pending`].
pub const PENDING_PIPELINE: usize = 32;

/// Run routed with one connection of pipeline [`PENDING_PIPELINE`] at 200
/// requests a second against a server that answers each GET 12 ms after it
/// arrives, and return the p50 of `routed_queue_wait`. About 2.4 requests are
/// in flight, so the connection usually has a reply pending when a request is
/// queued. With room for a batch (`batch` unset, or 1) it sends the request at
/// once; with `batch` at the pipeline depth, the request waits for the
/// replies.
///
/// The connection carries the offered rate while a round trip takes under
/// 160 ms (32 / 200 per second). Past that requests back up in the queue and
/// its wait is the backlog, whatever the batch. The run fails if the
/// connection sent fewer than 350 of the about 410 requests funded, which a
/// round trip over about 180 ms produces; a backlog from a round trip between
/// 160 ms and that is not caught here.
pub fn routed_queue_wait_with_a_reply_pending(protocol: &str, batch: Option<usize>) -> u64 {
    let (addr, _) = spawn_paced(protocol, Pace::FromArrival(Duration::from_millis(12)));
    let started = Instant::now();
    run_routed(&Routed {
        protocol,
        endpoints: &[addr],
        threads: 1,
        connections: 1,
        pipeline: PENDING_PIPELINE,
        batch,
        rate: 200,
        duration: "2s",
        routing: "routed",
        keyspace: "count = 1000",
    });
    let elapsed = started.elapsed();
    use cachecannon::metrics as m;
    let quantiles = |h: &cachecannon::metrics::LatencyHistogram| -> Option<Vec<u64>> {
        let h = h.load()?;
        Some(
            h.quantiles(&[0.0, 0.1, 0.5, 0.9, 0.99, 1.0])
                .unwrap()
                .unwrap()
                .entries()
                .values()
                .map(|b| b.end())
                .collect(),
        )
    };
    eprintln!(
        "run {elapsed:?}; queue wait min/p10/p50/p90/p99/max {:?} ns; \
         response latency {:?} ns; sent {} received {} dropped {} timeouts {} \
         connect failed {} send failed {}",
        quantiles(&m::ROUTED_QUEUE_WAIT),
        quantiles(&m::RESPONSE_LATENCY),
        m::REQUESTS_SENT.value(),
        m::RESPONSES_RECEIVED.value(),
        m::ROUTED_QUEUE_DROPPED.value(),
        m::REQUEST_TIMEOUTS.value(),
        m::CONNECTIONS_FAILED.value(),
        m::REQUESTS_SEND_FAILED.value(),
    );
    let sent = m::REQUESTS_SENT.value();
    assert!(
        sent >= 350,
        "200 req/s for 2 s funds about 400 requests and the connection sent {sent}, \
         so it fell behind and the queue wait is a backlog"
    );
    p50_ns(&m::ROUTED_QUEUE_WAIT)
}
