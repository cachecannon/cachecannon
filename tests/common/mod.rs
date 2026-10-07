//! Stub memcache server and a capturing formatter for end-to-end tests of
//! `connection.disconnect_rate`.
//!
//! The stub delays every reply, so connections always have requests in
//! flight, and counts connections that went away while it still held requests
//! it had not answered.

use cachecannon::{Config, OutputFormatter, Results, Sample, metrics, run_benchmark_full};
use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long the stub holds each request before answering it.
const REPLY_DELAY: Duration = Duration::from_millis(5);

pub const CONNECTIONS: u64 = 4;
pub const RATE: f64 = 20.0;
pub const DURATION_SECS: f64 = 3.0;

#[derive(Default)]
pub struct StubCounters {
    pub accepted: AtomicU64,
    /// Connections closed by the client (FIN, close_notify or RST) while the
    /// stub still held unanswered requests.
    pub closed_with_pending: AtomicU64,
    /// Of those, the ones the stub saw as a reset or other read error.
    pub reset_with_pending: AtomicU64,
}

/// Serve memcache ASCII requests on one connection, `REPLY_DELAY` after each
/// arrives: `VERSION` for the precheck, `END` (a miss) for everything else.
/// The workload is GET-only, so every request is one line. The underlying
/// socket must have a short read timeout so replies go out on time.
pub fn serve<S: Read + Write>(mut stream: S, counters: Arc<StubCounters>) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    let mut pending: VecDeque<(Instant, &'static [u8])> = VecDeque::new();
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => {
                if !pending.is_empty() {
                    counters.closed_with_pending.fetch_add(1, Ordering::Relaxed);
                }
                return;
            }
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                let now = Instant::now();
                while let Some(pos) = buf.windows(2).position(|w| w == b"\r\n") {
                    let reply: &[u8] = if buf.starts_with(b"version") {
                        b"VERSION 1.0\r\n"
                    } else {
                        b"END\r\n"
                    };
                    pending.push_back((now, reply));
                    buf.drain(..pos + 2);
                }
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(_) => {
                if !pending.is_empty() {
                    counters.closed_with_pending.fetch_add(1, Ordering::Relaxed);
                    counters.reset_with_pending.fetch_add(1, Ordering::Relaxed);
                }
                return;
            }
        }
        let mut out = Vec::new();
        while pending
            .front()
            .is_some_and(|(t, _)| t.elapsed() >= REPLY_DELAY)
        {
            let (_, reply) = pending.pop_front().unwrap();
            out.extend_from_slice(reply);
        }
        if !out.is_empty() && (stream.write_all(&out).is_err() || stream.flush().is_err()) {
            return;
        }
    }
}

/// Bind a stub on localhost and serve each accepted connection on its own
/// thread, through `wrap` (identity for plain TCP, a TLS session for TLS).
pub fn start_stub<F>(wrap: F) -> (SocketAddr, Arc<StubCounters>)
where
    F: Fn(TcpStream, Arc<StubCounters>) + Send + Sync + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let counters = Arc::new(StubCounters::default());
    let wrap = Arc::new(wrap);
    {
        let counters = Arc::clone(&counters);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                stream
                    .set_read_timeout(Some(Duration::from_millis(1)))
                    .unwrap();
                counters.accepted.fetch_add(1, Ordering::Relaxed);
                let counters = Arc::clone(&counters);
                let wrap = Arc::clone(&wrap);
                std::thread::spawn(move || wrap(stream, counters));
            }
        });
    }
    (addr, counters)
}

/// Captures the final results; prints nothing.
struct Capture(Arc<Mutex<Option<Results>>>);

impl OutputFormatter for Capture {
    fn print_config(&self, _config: &Config) {}
    fn print_warmup(&self, _duration: Duration) {}
    fn print_running(&self, _duration: Duration) {}
    fn print_header(&self) {}
    fn print_sample(&self, _sample: &Sample) {}
    fn print_results(&self, results: &Results) {
        *self.0.lock().unwrap() = Some(results.clone());
    }
}

/// Run a GET-only memcache benchmark against `addr` with injected
/// disconnects, then check what the client and the stub recorded.
/// `target_extra` is appended to the `[target]` table.
pub fn run_and_check(addr: SocketAddr, counters: &StubCounters, target_extra: &str) {
    let config: Config = toml::from_str(&format!(
        r#"
[general]
threads = 2
warmup = "1s"
duration = "{DURATION_SECS}s"

[target]
endpoints = ["{addr}"]
protocol = "memcache"
{target_extra}

[connection]
connections = {CONNECTIONS}
pipeline_depth = 8
request_timeout = "2s"
disconnect_rate = {RATE}

[workload.commands]
get = 100
set = 0
delete = 0

[workload.keyspace]
count = 1000
"#
    ))
    .unwrap();

    let captured = Arc::new(Mutex::new(None));
    run_benchmark_full(
        config,
        None,
        Box::new(Capture(Arc::clone(&captured))),
        Arc::new(AtomicBool::new(true)),
    )
    .expect("benchmark run");
    let results = captured.lock().unwrap().take().expect("results printed");

    // The stub threads poll their sockets every millisecond; give them time
    // to see the last closes.
    let injected_total = metrics::DISCONNECTS_INJECTED.value();
    let deadline = Instant::now() + Duration::from_secs(2);
    while counters.closed_with_pending.load(Ordering::Relaxed) < injected_total
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    let accepted = counters.accepted.load(Ordering::Relaxed);
    let closed_with_pending = counters.closed_with_pending.load(Ordering::Relaxed);
    let reset_with_pending = counters.reset_with_pending.load(Ordering::Relaxed);
    eprintln!(
        "recording phase: injected={} abandoned={} responses={} errors={} timeouts={}; \
         whole run: injected={injected_total} abandoned={}; \
         stub: accepted={accepted} closed_with_pending={closed_with_pending} \
         reset_with_pending={reset_with_pending}",
        results.disconnects_injected,
        results.requests_abandoned,
        results.responses,
        results.errors,
        results.timeouts,
        metrics::REQUESTS_ABANDONED.value(),
    );

    // Closes happened at roughly the configured rate.
    let expected = RATE * DURATION_SECS;
    let injected = results.disconnects_injected as f64;
    assert!(
        injected >= expected * 0.5 && injected <= expected * 1.5,
        "expected about {expected} injected disconnects, got {injected}"
    );
    assert!(
        results.requests_abandoned >= results.disconnects_injected,
        "every injected close abandons at least one request"
    );
    assert!(results.responses > 0, "the workload ran between closes");
    // Every injected close reached the server while it held requests it had
    // not answered. Shutdown can add up to one more per connection.
    assert!(
        closed_with_pending >= injected_total,
        "{injected_total} injected, but the stub saw {closed_with_pending} closes with \
         requests pending"
    );
    // Abandoned requests are not errors or timeouts.
    assert_eq!(results.timeouts, 0);
    assert_eq!(results.errors, 0);
    assert_eq!(metrics::REQUEST_TIMEOUTS.value(), 0);
    assert_eq!(metrics::REQUEST_ERRORS.value(), 0);
    // Each close is followed by a reconnect through the normal path. Accepts
    // are the initial connections plus every reconnect, less any still in
    // their reconnect delay when the run stopped, at most one per connection.
    assert!(
        accepted >= injected_total,
        "accepted {accepted} connections for {injected_total} injected closes"
    );
}
