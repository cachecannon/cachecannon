//! Benchmark metrics using sharded counters.

pub use metrics::set_thread_shard;
use metrics::{Counter, CounterGroup};
use metriken::{AtomicHistogram, Gauge, metric};

// Counter groups
static REQUEST: CounterGroup = CounterGroup::new();
static CACHE: CounterGroup = CounterGroup::new();
static CONNECTION: CounterGroup = CounterGroup::new();
static BYTES: CounterGroup = CounterGroup::new();
static OPS: CounterGroup = CounterGroup::new();

/// Counter slot indices for request metrics.
pub mod request {
    pub const SENT: usize = 0;
    pub const RECEIVED: usize = 1;
    pub const ERRORS: usize = 2;
}

/// Counter slot indices for cache metrics.
pub mod cache {
    pub const HITS: usize = 0;
    pub const MISSES: usize = 1;
}

/// Counter slot indices for connection metrics.
pub mod connection {
    pub const FAILED: usize = 1;
    pub const DISCONNECT_EOF: usize = 2;
    pub const DISCONNECT_RECV_ERROR: usize = 3;
    pub const DISCONNECT_SEND_ERROR: usize = 4;
    pub const DISCONNECT_CLOSED_EVENT: usize = 5;
    pub const DISCONNECT_ERROR_EVENT: usize = 6;
    pub const DISCONNECT_CONNECT_FAILED: usize = 7;
}

/// Counter slot indices for bytes metrics.
pub mod bytes {
    pub const TX: usize = 0;
    pub const RX: usize = 1;
}

/// Counter slot indices for operation metrics.
pub mod ops {
    pub const GET: usize = 0;
    pub const SET: usize = 1;
    pub const DELETE: usize = 2;
    pub const BACKFILL_SET: usize = 3;
}

// Request counters
#[metric(name = "requests_sent", description = "Total requests sent")]
pub static REQUESTS_SENT: Counter = Counter::new(&REQUEST, request::SENT);

#[metric(name = "responses_received", description = "Total responses received")]
pub static RESPONSES_RECEIVED: Counter = Counter::new(&REQUEST, request::RECEIVED);

#[metric(name = "request_errors", description = "Total request errors")]
pub static REQUEST_ERRORS: Counter = Counter::new(&REQUEST, request::ERRORS);

// Cache counters
#[metric(name = "cache_hits", description = "Total cache hits")]
pub static CACHE_HITS: Counter = Counter::new(&CACHE, cache::HITS);

#[metric(name = "cache_misses", description = "Total cache misses")]
pub static CACHE_MISSES: Counter = Counter::new(&CACHE, cache::MISSES);

// Bytes counters
#[metric(name = "bytes_tx", description = "Total bytes transmitted")]
pub static BYTES_TX: Counter = Counter::new(&BYTES, bytes::TX);

#[metric(name = "bytes_rx", description = "Total bytes received")]
pub static BYTES_RX: Counter = Counter::new(&BYTES, bytes::RX);

// Operation counters
#[metric(name = "get_count", description = "Total GET operations")]
pub static GET_COUNT: Counter = Counter::new(&OPS, ops::GET);

#[metric(name = "set_count", description = "Total SET operations")]
pub static SET_COUNT: Counter = Counter::new(&OPS, ops::SET);

#[metric(name = "delete_count", description = "Total DELETE operations")]
pub static DELETE_COUNT: Counter = Counter::new(&OPS, ops::DELETE);

#[metric(
    name = "backfill_set_count",
    description = "Backfill SET operations (from backfill_on_miss)"
)]
pub static BACKFILL_SET_COUNT: Counter = Counter::new(&OPS, ops::BACKFILL_SET);

// Connection gauge (tracks current active connections; decremented on disconnect)
#[metric(name = "connections_active", description = "Active connections")]
pub static CONNECTIONS_ACTIVE: Gauge = Gauge::new();

#[metric(name = "connections_failed", description = "Failed connections")]
pub static CONNECTIONS_FAILED: Counter = Counter::new(&CONNECTION, connection::FAILED);

#[metric(name = "disconnects_eof", description = "Disconnects due to EOF")]
pub static DISCONNECTS_EOF: Counter = Counter::new(&CONNECTION, connection::DISCONNECT_EOF);

#[metric(
    name = "disconnects_recv_error",
    description = "Disconnects due to recv error"
)]
pub static DISCONNECTS_RECV_ERROR: Counter =
    Counter::new(&CONNECTION, connection::DISCONNECT_RECV_ERROR);

#[metric(
    name = "disconnects_send_error",
    description = "Disconnects due to send error"
)]
pub static DISCONNECTS_SEND_ERROR: Counter =
    Counter::new(&CONNECTION, connection::DISCONNECT_SEND_ERROR);

#[metric(
    name = "disconnects_closed_event",
    description = "Disconnects due to closed event"
)]
pub static DISCONNECTS_CLOSED_EVENT: Counter =
    Counter::new(&CONNECTION, connection::DISCONNECT_CLOSED_EVENT);

#[metric(
    name = "disconnects_error_event",
    description = "Disconnects due to error event"
)]
pub static DISCONNECTS_ERROR_EVENT: Counter =
    Counter::new(&CONNECTION, connection::DISCONNECT_ERROR_EVENT);

#[metric(
    name = "disconnects_connect_failed",
    description = "Disconnects due to connect failure"
)]
pub static DISCONNECTS_CONNECT_FAILED: Counter =
    Counter::new(&CONNECTION, connection::DISCONNECT_CONNECT_FAILED);

// Target rate gauge (for saturation search)
#[metric(
    name = "target_rate",
    description = "Current target request rate (requests per second)"
)]
pub static TARGET_RATE: Gauge = Gauge::new();

// Cluster metrics
static CLUSTER: CounterGroup = CounterGroup::new();

/// Counter slot indices for cluster metrics.
pub mod cluster {
    pub const REDIRECTS: usize = 0;
}

#[metric(
    name = "cluster_redirects",
    description = "Total MOVED/ASK redirects received"
)]
pub static CLUSTER_REDIRECTS: Counter = Counter::new(&CLUSTER, cluster::REDIRECTS);

// Latency histograms (kept as metriken AtomicHistogram)
#[metric(
    name = "response_latency",
    description = "Response latency histogram (nanoseconds)"
)]
pub static RESPONSE_LATENCY: AtomicHistogram = AtomicHistogram::new(7, 64);

#[metric(
    name = "get_latency",
    description = "GET response latency histogram (nanoseconds)"
)]
pub static GET_LATENCY: AtomicHistogram = AtomicHistogram::new(7, 64);

#[metric(
    name = "set_latency",
    description = "SET response latency histogram (nanoseconds)"
)]
pub static SET_LATENCY: AtomicHistogram = AtomicHistogram::new(7, 64);

#[metric(
    name = "delete_latency",
    description = "DELETE response latency histogram (nanoseconds)"
)]
pub static DELETE_LATENCY: AtomicHistogram = AtomicHistogram::new(7, 64);

#[metric(
    name = "get_ttfb",
    description = "GET time-to-first-byte histogram (nanoseconds)"
)]
pub static GET_TTFB: AtomicHistogram = AtomicHistogram::new(7, 64);

#[metric(
    name = "backfill_set_latency",
    description = "Backfill SET latency histogram (nanoseconds)"
)]
pub static BACKFILL_SET_LATENCY: AtomicHistogram = AtomicHistogram::new(7, 64);
