//! Benchmark metrics using sharded counters.

pub use crate::sharded_counter::set_thread_shard;
use crate::sharded_counter::{Counter, CounterGroup, Gauge as ShardedGauge};
use crate::sharded_histogram::ShardedHistogram;
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
    pub const TIMEOUTS: usize = 3;
    pub const SEND_FAILED: usize = 4;
    pub const ROUTED_DROPPED: usize = 5;
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
    pub const DISCONNECT_TIMEOUT: usize = 8;
}

/// Counter slot indices for bytes metrics.
pub mod bytes {
    pub const TX: usize = 0;
    pub const RX: usize = 1;
    pub const VALUES_READ: usize = 2;
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

#[metric(
    name = "requests_send_failed",
    description = "RESP and memcache main-workload requests not sent because a send failed (usually a full send pool or zero-copy slab)"
)]
pub static REQUESTS_SEND_FAILED: Counter = Counter::new(&REQUEST, request::SEND_FAILED);

#[metric(
    name = "routed_queue_dropped",
    description = "Routed requests the dispatcher funded but dropped because the owning node's queue was full"
)]
pub static ROUTED_QUEUE_DROPPED: Counter = Counter::new(&REQUEST, request::ROUTED_DROPPED);

#[metric(name = "responses_received", description = "Total responses received")]
pub static RESPONSES_RECEIVED: Counter = Counter::new(&REQUEST, request::RECEIVED);

#[metric(name = "request_errors", description = "Total request errors")]
pub static REQUEST_ERRORS: Counter = Counter::new(&REQUEST, request::ERRORS);

#[metric(
    name = "request_timeouts",
    description = "Requests abandoned because no reply arrived within connection.request_timeout (also counted in request_errors)"
)]
pub static REQUEST_TIMEOUTS: Counter = Counter::new(&REQUEST, request::TIMEOUTS);

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

#[metric(
    name = "values_read_bytes",
    description = "GET value bytes checksummed by the client (workload.read_values); 0 when off"
)]
pub static VALUES_READ_BYTES: Counter = Counter::new(&BYTES, bytes::VALUES_READ);

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

// Connection gauge (tracks current active connections; decremented on disconnect).
// Uses a sharded gauge so connect/disconnect from many worker threads doesn't
// contend on a single cache line.
#[metric(name = "connections_active", description = "Active connections")]
pub static CONNECTIONS_ACTIVE: ShardedGauge = ShardedGauge::new();

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

#[metric(
    name = "disconnects_timeout",
    description = "Connections torn down because a request exceeded connection.request_timeout"
)]
pub static DISCONNECTS_TIMEOUT: Counter = Counter::new(&CONNECTION, connection::DISCONNECT_TIMEOUT);

// Target rate gauge (for saturation search)
#[metric(
    name = "target_rate",
    description = "Current target request rate (requests per second)"
)]
pub static TARGET_RATE: Gauge = Gauge::new();

// ── Append stream metrics ────────────────────────────────────────────────
//
// The append role writes on its own connections and is reported separately:
// a batch burst must not inflate the read workload's request, response, or
// latency numbers, which are what a run against a growing keyspace measures.
static APPEND: CounterGroup = CounterGroup::new();

/// Counter slot indices for append metrics.
pub mod append {
    pub const SET: usize = 0;
    pub const ERRORS: usize = 1;
}

#[metric(
    name = "append_set_count",
    description = "Append-stream SET operations confirmed"
)]
pub static APPEND_SET_COUNT: Counter = Counter::new(&APPEND, append::SET);

#[metric(
    name = "append_set_errors",
    description = "Append-stream SET operations that failed and were retried"
)]
pub static APPEND_SET_ERRORS: Counter = Counter::new(&APPEND, append::ERRORS);

#[metric(
    name = "append_set_latency",
    description = "Append-stream SET latency histogram (nanoseconds)"
)]
pub static APPEND_SET_LATENCY: LatencyHistogram = LatencyHistogram::new();

#[metric(
    name = "append_head",
    description = "Keyspace head: number of key ids readers may target (grows as append batches commit)"
)]
pub static APPEND_HEAD: Gauge = Gauge::new();

#[metric(
    name = "append_batches",
    description = "Append batches committed so far"
)]
pub static APPEND_BATCHES: Gauge = Gauge::new();

#[metric(
    name = "append_connections_active",
    description = "Active append-stream writer connections"
)]
pub static APPEND_CONNECTIONS_ACTIVE: ShardedGauge = ShardedGauge::new();

// ── KV-cache replay metrics ──────────────────────────────────────────────
//
// `[workload.kv_replay]`. Tokens are prompt tokens of the replayed trace
// requests; the EXISTS, GET and SET operations they issue are also counted
// in the request, response and per-op metrics above.
static KV: CounterGroup = CounterGroup::new();

/// Counter slot indices for KV replay metrics.
pub mod kv {
    pub const REQUESTS: usize = 0;
    pub const FAILED: usize = 1;
    pub const INPUT_TOKENS: usize = 2;
    pub const CACHED_TOKENS: usize = 3;
    pub const NEW_TOKENS: usize = 4;
    pub const FULL_HITS: usize = 5;
    pub const CHUNK0_MISSES: usize = 6;
    pub const NO_FULL_CHUNK: usize = 7;
    pub const GET_NIL: usize = 8;
    pub const EXISTS: usize = 9;
    pub const LOOKUPS: usize = 10;
    pub const LOADS: usize = 11;
    pub const STORES: usize = 12;
    pub const LOADS_WITH_CACHED: usize = 13;
    pub const TRACE_READ_WAITS: usize = 14;
}

#[metric(
    name = "kv_requests",
    description = "KV replay: trace requests replayed to completion"
)]
pub static KV_REQUESTS: Counter = Counter::new(&KV, kv::REQUESTS);

#[metric(
    name = "kv_requests_failed",
    description = "KV replay: trace requests abandoned on a connection error, send failure or server error"
)]
pub static KV_REQUESTS_FAILED: Counter = Counter::new(&KV, kv::FAILED);

#[metric(
    name = "kv_input_tokens",
    description = "KV replay: prompt tokens of the replayed requests"
)]
pub static KV_INPUT_TOKENS: Counter = Counter::new(&KV, kv::INPUT_TOKENS);

#[metric(
    name = "kv_cached_tokens",
    description = "KV replay: prompt tokens served from the cache"
)]
pub static KV_CACHED_TOKENS: Counter = Counter::new(&KV, kv::CACHED_TOKENS);

#[metric(
    name = "kv_new_tokens",
    description = "KV replay: prompt tokens not served from the cache (input - cached)"
)]
pub static KV_NEW_TOKENS: Counter = Counter::new(&KV, kv::NEW_TOKENS);

#[metric(
    name = "kv_full_hits",
    description = "KV replay: requests whose cached prefix covered the whole prompt"
)]
pub static KV_FULL_HITS: Counter = Counter::new(&KV, kv::FULL_HITS);

#[metric(
    name = "kv_chunk0_misses",
    description = "KV replay: requests whose lookup found the first chunk missing"
)]
pub static KV_CHUNK0_MISSES: Counter = Counter::new(&KV, kv::CHUNK0_MISSES);

#[metric(
    name = "kv_no_full_chunk",
    description = "KV replay: requests shorter than one chunk, which issue no operations"
)]
pub static KV_NO_FULL_CHUNK: Counter = Counter::new(&KV, kv::NO_FULL_CHUNK);

#[metric(
    name = "kv_get_nil",
    description = "KV replay: GETs of an object whose EXISTS returned 1 that returned nil"
)]
pub static KV_GET_NIL: Counter = Counter::new(&KV, kv::GET_NIL);

#[metric(
    name = "kv_trace_read_waits",
    description = "KV replay: times a dispatcher had an idle connection and no trace request \
                   because the trace reader had not read the next one yet"
)]
pub static KV_TRACE_READ_WAITS: Counter = Counter::new(&KV, kv::TRACE_READ_WAITS);

// ── Object trace replay metrics ──────────────────────────────────────────
//
// `[workload.trace_replay]`. The GETs, SETs and DELs the rows send are also
// counted in the request, response and per-op metrics above.
static TRACE: CounterGroup = CounterGroup::new();

/// Counter slot indices for trace replay metrics.
pub mod trace {
    pub const ROWS: usize = 0;
    pub const ROWS_FAILED: usize = 1;
    pub const ROWS_SKIPPED: usize = 2;
    pub const GETS: usize = 3;
    pub const SETS: usize = 4;
    pub const DELS: usize = 5;
    pub const READ_WAITS: usize = 6;
}

#[metric(name = "trace_rows", description = "Trace replay: rows answered")]
pub static TRACE_ROWS: Counter = Counter::new(&TRACE, trace::ROWS);

#[metric(
    name = "trace_rows_failed",
    description = "Trace replay: rows in flight on a connection that failed (not retried), or that the send pool refused outright 3 times"
)]
pub static TRACE_ROWS_FAILED: Counter = Counter::new(&TRACE, trace::ROWS_FAILED);

#[metric(
    name = "trace_rows_skipped",
    description = "Trace replay: rows not sent, with an unknown op or a SET larger than the value pool"
)]
pub static TRACE_ROWS_SKIPPED: Counter = Counter::new(&TRACE, trace::ROWS_SKIPPED);

#[metric(
    name = "trace_gets",
    description = "Trace replay: rows answered that were sent as GET"
)]
pub static TRACE_GETS: Counter = Counter::new(&TRACE, trace::GETS);

#[metric(
    name = "trace_sets",
    description = "Trace replay: rows answered that were sent as SET (backfill SETs are not rows)"
)]
pub static TRACE_SETS: Counter = Counter::new(&TRACE, trace::SETS);

#[metric(
    name = "trace_dels",
    description = "Trace replay: rows answered that were sent as DEL"
)]
pub static TRACE_DELS: Counter = Counter::new(&TRACE, trace::DELS);

#[metric(
    name = "trace_read_waits",
    description = "Trace replay: times a connection had room to send and no row, because the \
                   trace reader had not read the next batch yet"
)]
pub static TRACE_READ_WAITS: Counter = Counter::new(&TRACE, trace::READ_WAITS);

#[metric(name = "kv_exists_count", description = "KV replay: EXISTS operations")]
pub static KV_EXISTS_COUNT: Counter = Counter::new(&KV, kv::EXISTS);

#[metric(
    name = "kv_lookups",
    description = "KV replay: requests that issued a lookup (samples in kv_lookup_time)"
)]
pub static KV_LOOKUPS: Counter = Counter::new(&KV, kv::LOOKUPS);

#[metric(
    name = "kv_loads",
    description = "KV replay: requests that issued GETs (samples in kv_load_time)"
)]
pub static KV_LOADS: Counter = Counter::new(&KV, kv::LOADS);

#[metric(
    name = "kv_stores",
    description = "KV replay: requests that issued SETs (samples in kv_store_time)"
)]
pub static KV_STORES: Counter = Counter::new(&KV, kv::STORES);

#[metric(
    name = "kv_loads_with_cached_tokens",
    description = "KV replay: requests that issued GETs and ended with cached tokens (samples in kv_load_time_per_1k_tokens)"
)]
pub static KV_LOADS_WITH_CACHED: Counter = Counter::new(&KV, kv::LOADS_WITH_CACHED);

#[metric(
    name = "kv_lookup_time",
    description = "KV replay: per request, first EXISTS sent to last EXISTS reply (nanoseconds)"
)]
pub static KV_LOOKUP_TIME: AtomicHistogram = AtomicHistogram::new(7, 64);

#[metric(
    name = "kv_load_time",
    description = "KV replay: per request, first GET sent to last GET reply (nanoseconds)"
)]
pub static KV_LOAD_TIME: AtomicHistogram = AtomicHistogram::new(7, 64);

#[metric(
    name = "kv_store_time",
    description = "KV replay: per request, first SET sent to last SET reply (nanoseconds)"
)]
pub static KV_STORE_TIME: AtomicHistogram = AtomicHistogram::new(7, 64);

#[metric(
    name = "kv_load_time_per_1k_tokens",
    description = "KV replay: per request with cached tokens, load time per 1,000 cached tokens (nanoseconds)"
)]
pub static KV_LOAD_TIME_PER_1K_TOKENS: AtomicHistogram = AtomicHistogram::new(7, 64);

#[metric(
    name = "kv_schedule_lag",
    description = "KV replay, open loop: per request, start minus the time the trace scheduled it (nanoseconds)"
)]
pub static KV_SCHEDULE_LAG: AtomicHistogram = AtomicHistogram::new(7, 64);

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

/// Omitted schedule slip in nanoseconds, derived from the rate limiter's
/// backlog. `available` overdue tokens at `rate` req/s represent
/// `available / rate` seconds of queueing the latency clock did not see.
/// Returns 0 when `rate == 0` (no schedule configured).
pub fn slip_ns(available: u64, rate: u64) -> u64 {
    if rate == 0 {
        return 0;
    }
    available.saturating_mul(1_000_000_000) / rate
}

// Latency histograms: one shard per worker thread, merged on load (see
// sharded_histogram). A single process-wide AtomicHistogram would make every
// worker contend on the same few hot buckets.
pub type LatencyHistogram = ShardedHistogram<7, 64>;
#[metric(
    name = "response_latency",
    description = "Response latency histogram (nanoseconds)"
)]
pub static RESPONSE_LATENCY: LatencyHistogram = LatencyHistogram::new();

#[metric(
    name = "get_latency",
    description = "GET response latency histogram (nanoseconds)"
)]
pub static GET_LATENCY: LatencyHistogram = LatencyHistogram::new();

#[metric(
    name = "set_latency",
    description = "SET response latency histogram (nanoseconds)"
)]
pub static SET_LATENCY: LatencyHistogram = LatencyHistogram::new();

#[metric(
    name = "delete_latency",
    description = "DELETE response latency histogram (nanoseconds)"
)]
pub static DELETE_LATENCY: LatencyHistogram = LatencyHistogram::new();

#[metric(
    name = "get_ttfb",
    description = "GET time-to-first-byte histogram (nanoseconds)"
)]
pub static GET_TTFB: LatencyHistogram = LatencyHistogram::new();

#[metric(
    name = "routed_queue_wait",
    description = "Routed requests: time from the dispatcher funding a request to a connection sending it (nanoseconds); included in response_latency"
)]
pub static ROUTED_QUEUE_WAIT: LatencyHistogram = LatencyHistogram::new();

#[metric(
    name = "backfill_set_latency",
    description = "Backfill SET latency histogram (nanoseconds)"
)]
pub static BACKFILL_SET_LATENCY: LatencyHistogram = LatencyHistogram::new();

// ── Coordinated-omission metrics ─────────────────────────────────────────
#[metric(
    name = "schedule_slip",
    description = "Per-request schedule slip histogram (nanoseconds): queueing time the latency clock omits, from the rate limiter backlog"
)]
pub static SCHEDULE_SLIP: LatencyHistogram = LatencyHistogram::new();

#[metric(
    name = "perceived_latency",
    description = "Arrival-relative (CO-honest) response latency histogram (nanoseconds): response_latency + schedule slip"
)]
pub static PERCEIVED_LATENCY: LatencyHistogram = LatencyHistogram::new();

#[metric(
    name = "current_slip_ns",
    description = "Current schedule slip in nanoseconds (rate limiter backlog / rate)"
)]
pub static CURRENT_SLIP_NS: Gauge = Gauge::new();

#[metric(
    name = "requests_dropped",
    description = "Requests shed by the rate limiter under overload (bucket overflow)"
)]
pub static REQUESTS_DROPPED: Gauge = Gauge::new();

#[cfg(test)]
mod co_tests {
    use super::slip_ns;

    #[test]
    fn slip_is_backlog_over_rate_in_ns() {
        // 1000 overdue tokens at 1000 req/s = 1.0s = 1e9 ns.
        assert_eq!(slip_ns(1000, 1000), 1_000_000_000);
        // 50 tokens at 1000 req/s = 50ms.
        assert_eq!(slip_ns(50, 1000), 50_000_000);
        // No backlog = no slip.
        assert_eq!(slip_ns(0, 1000), 0);
    }

    #[test]
    fn slip_handles_zero_rate_without_panicking() {
        // Rate 0 (unlimited / not configured): no schedule, no slip.
        assert_eq!(slip_ns(1234, 0), 0);
    }
}
