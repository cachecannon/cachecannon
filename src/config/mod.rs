use crate::output::{ColorMode, OutputFormat};
use serde::Deserialize;
use std::net::SocketAddr;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub general: General,
    pub target: Target,
    #[serde(default)]
    pub connection: Connection,
    #[serde(default)]
    pub workload: Workload,
    #[serde(default)]
    pub timestamps: Timestamps,
    #[serde(default)]
    pub admin: Admin,
    #[serde(default)]
    pub dma: Dma,
}

#[derive(Debug, Clone, Deserialize)]
pub struct General {
    #[serde(default = "default_duration", with = "humantime_serde")]
    pub duration: Duration,
    #[serde(default = "default_warmup", with = "humantime_serde")]
    pub warmup: Duration,
    #[serde(default = "default_threads")]
    pub threads: usize,
    /// CPU list for pinning worker threads (Linux style: "0-3,8-11,13")
    #[serde(default)]
    pub cpu_list: Option<String>,
    /// Print ringline's per-worker event-loop diagnostics (`[ringline diag]`
    /// and `[ringline stall]`) to stderr at shutdown. io_uring only.
    #[serde(default)]
    pub ringline_diag: bool,
    /// Buffers in the per-worker provided recv ring. Must be a power of two.
    /// Unset uses ringline's default of 256.
    ///
    /// The ring is shared by every connection on a worker. A buffer is held
    /// only between a completion and the client draining it -- not for the life
    /// of a connection -- which is why a ring this size serves thousands of
    /// connections without running dry. Measured: 10,000 connections at 20,000
    /// req/s of 56 KiB values, four buffers per response, `parks=0` on every
    /// worker.
    ///
    /// Exposed for the case where that does not hold (ringline's own server-side
    /// sweeps reach millions of `ENOBUFS` parks at small ring depths), not
    /// because cachecannon has a reason to move it.
    #[serde(default)]
    pub recv_ring_size: Option<u16>,
    /// Bytes per buffer in the per-worker provided recv ring. Unset uses
    /// ringline's default of 16 KiB.
    ///
    /// A response larger than this spans several buffers and costs one recv CQE
    /// each. That is real but small: a generator at 20,000 req/s was observed
    /// spending ~12M CQEs/s in total, of which request I/O was under 1%, so
    /// sizing the buffer to the response moves ~0.5% of the CQE budget.
    #[serde(default)]
    pub recv_buffer_size: Option<u32>,
    /// Seed for the per-connection RNGs that choose keys and commands. Unset
    /// draws one from OS entropy; either way the runner logs the seed in use,
    /// so a run can be repeated exactly by setting it here.
    ///
    /// Before this existed every connection was seeded from its position alone,
    /// so every process sent the same key sequence. A run that starts several
    /// processes against one prefilled server (a ladder of steps, A/B/A phases)
    /// then had each later process re-request keys an earlier one had missed and
    /// backfilled: they hit, and the miss rate fell phase by phase (measured
    /// 12.3% -> 6.8% -> 4.0% at zipf 0.90) with the backfill write load falling
    /// with it.
    #[serde(default)]
    pub seed: Option<u64>,
}

impl Default for General {
    fn default() -> Self {
        Self {
            duration: default_duration(),
            warmup: default_warmup(),
            threads: default_threads(),
            cpu_list: None,
            ringline_diag: false,
            recv_ring_size: None,
            recv_buffer_size: None,
            seed: None,
        }
    }
}

fn default_duration() -> Duration {
    Duration::from_secs(60)
}

fn default_warmup() -> Duration {
    Duration::from_secs(10)
}

fn default_threads() -> usize {
    num_cpus()
}

fn default_tls_verify() -> bool {
    true
}

fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

#[derive(Debug, Clone, Deserialize)]
pub struct Target {
    #[serde(default)]
    pub endpoints: Vec<SocketAddr>,
    #[serde(default)]
    pub protocol: Protocol,
    #[serde(default)]
    pub tls: bool,
    /// Explicit SNI hostname for TLS connections. When not set, the endpoint
    /// IP address is used. Needed because SocketAddr loses the original hostname
    /// after DNS resolution.
    #[serde(default)]
    pub tls_hostname: Option<String>,
    /// Whether to verify the server's TLS certificate. Default: true.
    /// Set to false for self-signed certificates (e.g., CI testing).
    #[serde(default = "default_tls_verify")]
    pub tls_verify: bool,
    /// PEM file of CA certificates used to verify the server. When set it
    /// *replaces* the public root store rather than adding to it, so only
    /// these CAs are trusted. Needed for a server whose certificate chains to
    /// a private CA, which the public roots cannot verify.
    #[serde(default)]
    pub tls_ca_file: Option<PathBuf>,
    /// PEM client certificate chain, presented for mutual TLS. Required by
    /// servers that verify client certificates. Must be set together with
    /// `tls_key_file`.
    #[serde(default)]
    pub tls_cert_file: Option<PathBuf>,
    /// PEM private key matching `tls_cert_file`. Must be unencrypted; rustls
    /// cannot read password-protected keys.
    #[serde(default)]
    pub tls_key_file: Option<PathBuf>,
    /// Enable Valkey/Redis Cluster mode: discover topology via CLUSTER SLOTS
    /// and route by hash slot instead of ketama consistent hashing.
    #[serde(default)]
    pub cluster: bool,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    #[default]
    Resp,
    /// RESP3 protocol (Valkey / Redis 6+)
    Resp3,
    /// Memcache ASCII text protocol
    Memcache,
    /// Memcache binary protocol
    #[serde(alias = "memcache-binary")]
    MemcacheBinary,
    /// Simple ASCII PING/PONG protocol
    Ping,
    /// Server-initiated RMA against valkey with vdma module. RESP carries the commands, payloads travel
    /// libfabric. Requires the `dma` feature and libfabric.
    Dma,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Connection {
    /// Total number of connections (distributed across threads).
    /// If not specified, defaults to 1 connection total.
    #[serde(default = "default_connections")]
    pub connections: usize,
    /// Legacy alias for connections (deprecated, use 'connections' instead)
    #[serde(default)]
    pub pool_size: Option<usize>,
    #[serde(default = "default_pipeline_depth")]
    pub pipeline_depth: usize,
    /// Maximum number of fire_* operations to coalesce into a single send.
    /// When unset, defaults to `pipeline_depth`, giving one packet per
    /// pipeline's worth of requests. Set to 1 to disable coalescing.
    /// Clamped to `[1, pipeline_depth]` at use.
    #[serde(default)]
    pub batch_size: Option<usize>,
    #[serde(default = "default_connect_timeout", with = "humantime_serde")]
    pub connect_timeout: Duration,
    #[serde(default = "default_request_timeout", with = "humantime_serde")]
    pub request_timeout: Duration,
    /// How to distribute requests across connections.
    #[serde(default)]
    pub request_distribution: RequestDistribution,
    /// How connections are assigned to endpoints. See [`EndpointAllocation`].
    #[serde(default)]
    pub endpoint_allocation: EndpointAllocation,
}

/// How connections are assigned to endpoints when there are several.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EndpointAllocation {
    /// Connection `i` serves endpoint `i % endpoints` (default). Every endpoint
    /// gets the same number of connections, and so the same request share in a
    /// rate-limited run, whatever share of the key distribution it owns.
    #[default]
    Even,
    /// Each endpoint gets connections in proportion to the share of zipf
    /// probability its keys carry: at least one if it owns keys, none if it
    /// owns none. When every connection gets the same share of rate-limit
    /// tokens, each endpoint then receives its zipf share of requests, so the
    /// servers together see the global zipf distribution. That needs a
    /// rate-limited run in which no connection is held back by its pipeline:
    /// tokens go first come, first served among a worker's waiting
    /// connections, so a connection with a full pipeline takes fewer (DMA, one
    /// transfer per connection, at MiB values). In a closed-loop run an
    /// endpoint's share also depends on its latency. Requires
    /// `keyspace.distribution = "zipf"`; validation counts the endpoints in
    /// `[target]`, which in cluster mode are the seeds. The memcache protocols
    /// still reject-route zipf keys rather than using the per-endpoint alias
    /// tables; the allocation applies to them all the same.
    KeyShare,
}

/// How requests are distributed across connections.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RequestDistribution {
    /// Round-robin: send one request to each connection in turn (default).
    #[default]
    RoundRobin,
    /// Greedy: fill the first connection's pipeline before moving to the next.
    Greedy,
}

impl Connection {
    /// Get the effective number of connections, preferring legacy 'pool_size' over 'connections' when set.
    pub fn total_connections(&self) -> usize {
        self.pool_size.unwrap_or(self.connections)
    }

    /// Effective fire_* coalesce threshold. Defaults to `pipeline_depth` when
    /// `batch_size` is unset, and is clamped to `[1, pipeline_depth]`.
    pub fn effective_batch_size(&self) -> usize {
        self.batch_size
            .unwrap_or(self.pipeline_depth)
            .clamp(1, self.pipeline_depth.max(1))
    }
}

impl Default for Connection {
    fn default() -> Self {
        Self {
            connections: default_connections(),
            pool_size: None,
            pipeline_depth: default_pipeline_depth(),
            batch_size: None,
            connect_timeout: default_connect_timeout(),
            request_timeout: default_request_timeout(),
            request_distribution: RequestDistribution::default(),
            endpoint_allocation: EndpointAllocation::default(),
        }
    }
}

fn default_connections() -> usize {
    1
}

fn default_pipeline_depth() -> usize {
    1
}

fn default_connect_timeout() -> Duration {
    Duration::from_secs(5)
}

fn default_request_timeout() -> Duration {
    Duration::from_secs(1)
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Workload {
    #[serde(default)]
    pub rate_limit: Option<u64>,
    /// Requests per second during prefill, across all connections. Omit or set
    /// to 0 for an unpaced prefill, which writes as fast as the connections
    /// allow. All workers draw on one limiter, which starts empty when prefill
    /// begins and holds at most 10 ms of the rate or one batch, whichever is
    /// more, plus one batch, so prefill does not open with a burst.
    /// `keyspace.count / prefill_rate_limit` must fit in `prefill_timeout`.
    #[serde(default)]
    pub prefill_rate_limit: Option<u64>,
    /// Prefill the cache with all keys before starting the benchmark.
    /// When enabled, each key in the keyspace is written exactly once
    /// before the warmup phase begins.
    #[serde(default)]
    pub prefill: bool,
    /// Maximum time to wait for prefill to complete before aborting.
    /// Set to "0s" to disable the timeout. Default: 300s.
    #[serde(default = "default_prefill_timeout", with = "humantime_serde")]
    pub prefill_timeout: Duration,
    /// Extra discarded warmup added after prefill, before measurement begins.
    ///
    /// Prefill leaves the SERVER's page cache full of dirty pages, and Linux
    /// does not write those back promptly: `dirty_expire_centisecs` defaults to
    /// 3000 -- thirty seconds -- after which the flusher evicts everything past
    /// expiry in one burst. Measured against a disk-backed cache: prefill wrote
    /// 57 GB at 218-428 MB/s, and forty seconds after it ended the kernel
    /// dumped ~4.8 GB at 483 MB/s in a single ten-second burst. That burst
    /// landed inside the first measurement window and took its p99 to 151ms.
    /// A saturation search reads that as the knee -- it fixed the bisection
    /// ceiling there and reported 4.8K req/s on hardware that sustains 55-60K.
    ///
    /// `warmup` alone cannot cover this: it defaults to 10s, so the discard
    /// window is guaranteed to close before the burst it exists to absorb.
    ///
    /// cachecannon cannot fix this at the source -- the dirty pages are on the
    /// server and this is the client, so there is no `sync()` to call. Waiting
    /// past the expiry deadline is the portable remedy. Traffic keeps flowing
    /// throughout, since this extends warmup rather than sleeping, so
    /// connections stay open and the server's caches stay warm.
    ///
    /// Applied ONLY when `prefill` is true, so a run against a memory-only
    /// server pays nothing. Set to "0s" to disable.
    #[serde(default = "default_prefill_settle", with = "humantime_serde")]
    pub prefill_settle: Duration,
    /// On GET miss, automatically SET the key to backfill the cache (cache-aside pattern).
    #[serde(default, alias = "set_on_miss")]
    pub backfill_on_miss: bool,
    /// Read every GET hit's value bytes: compute a CRC-32c over each value
    /// as the protocol client returns it. Off by default.
    ///
    /// By default the RESP client drains a value without assembling it, so the
    /// generator's CPU per request is lower than a client that uses the value
    /// would spend. With this on, RESP receives through ringline-redis `recv`,
    /// which holds the whole reply in contiguous memory, and the CRC is computed
    /// over that. On io_uring that adds one copy out of the provided buffers;
    /// on mio the reply is copied into the same buffer either way, so only the
    /// CRC is added. Memcache `recv` already assembles replies; it gets the
    /// CRC. Bytes read are counted in `values_read_bytes`.
    /// See `docs/reference.md` "Reading values" for memory, timing and
    /// error-handling differences.
    #[serde(default)]
    pub read_values: bool,
    #[serde(default)]
    pub keyspace: Keyspace,
    #[serde(default)]
    pub commands: Commands,
    #[serde(default)]
    pub values: Values,
    #[serde(default)]
    pub saturation_search: Option<SaturationSearch>,
    /// Append stream: new keys written in batches on a cadence by dedicated
    /// writer connections. Requires `keyspace.distribution = "recency"`.
    #[serde(default)]
    pub append: Option<Append>,
    /// KV-cache prefix replay: replay a request-level KV-cache trace instead
    /// of the synthetic keyspace. See `crate::kvreplay`.
    #[serde(default)]
    pub kv_replay: Option<KvReplay>,
}

/// `[workload.kv_replay]`: replay a kvtrace request trace against the server
/// as a KV-cache prefix store, and report cached and new prompt tokens.
///
/// The model geometry comes either from a Hugging Face `config.json`
/// (`model_config`) or from `kv_bytes_per_token` directly; exactly one must
/// be set.
#[derive(Debug, Clone, Deserialize)]
pub struct KvReplay {
    /// kvtrace request trace (Parquet).
    pub trace: PathBuf,
    /// Trace time is divided by this: 10 replays ten times faster than
    /// recorded. 0 replays closed-loop, each connection starting its next
    /// request as soon as the previous one finished. Default 1.
    #[serde(default = "default_kv_speedup")]
    pub speedup: f64,
    /// Hugging Face `config.json` the object sizes are derived from.
    #[serde(default)]
    pub model_config: Option<PathBuf>,
    /// KV cache dtype overriding the config's (`model_config` only), e.g.
    /// `"fp8"`.
    #[serde(default)]
    pub kv_dtype: Option<String>,
    /// KV bytes per token over all layers and ranks, instead of
    /// `model_config`.
    #[serde(default)]
    pub kv_bytes_per_token: Option<u64>,
    /// With `kv_bytes_per_token`: the model uses multi-head latent attention,
    /// so only rank 0 stores, at full size. A `model_config` says this itself.
    #[serde(default)]
    pub mla: bool,
    /// Tensor-parallel degree. Must be 1 (cachecannon#214). Default 1.
    #[serde(default = "default_kv_tp")]
    pub tp: u64,
    /// Which objects a request writes after its lookup. Default
    /// `"set-from-miss"`.
    #[serde(default)]
    pub write_rule: KvWriteRule,
    /// Prepended to every object key. Default `"kv:"`.
    #[serde(default = "default_kv_key_prefix")]
    pub key_prefix: String,
    /// How the object id is written after the prefix. Default `"hex"`.
    #[serde(default)]
    pub key_format: KvKeyFormat,
    /// Write one CSV row per completed trace request to this file.
    #[serde(default)]
    pub requests_out: Option<PathBuf>,
}

/// Which objects a KV replay request SETs after its lookup.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum KvWriteRule {
    /// Every object from the end of the lookup's prefix hit to the last full
    /// chunk, whether or not the store holds it (LMCache's in-process remote
    /// backends, on the default non-layerwise store path).
    #[default]
    SetFromMiss,
    /// Every object whose EXISTS returned 0 (LMCache's multiprocess store).
    InsertIfAbsent,
}

impl KvWriteRule {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SetFromMiss => "set-from-miss",
            Self::InsertIfAbsent => "insert-if-absent",
        }
    }
}

/// How a KV replay object id is written into its key.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum KvKeyFormat {
    /// 16 lowercase hex digits, zero-padded.
    #[default]
    Hex,
    /// Decimal, no padding.
    Decimal,
}

fn default_kv_speedup() -> f64 {
    1.0
}

fn default_kv_tp() -> u64 {
    1
}

fn default_kv_key_prefix() -> String {
    "kv:".to_string()
}

/// Configuration for saturation search mode.
///
/// When enabled, the benchmark will start at `start_rate` and geometrically
/// increase the rate by `step_multiplier` after each `sample_window`. The
/// search stops when the SLO is violated for `stop_after_failures` consecutive
/// steps, and reports the last compliant rate.
#[derive(Debug, Clone, Deserialize)]
pub struct SaturationSearch {
    /// SLO thresholds that must be met.
    pub slo: SloThresholds,
    /// Starting request rate (requests per second).
    #[serde(default = "default_start_rate")]
    pub start_rate: u64,
    /// Multiplier for each rate step (e.g., 1.05 = 5% increase).
    #[serde(default = "default_step_multiplier")]
    pub step_multiplier: f64,
    /// Duration to sample at each rate level.
    #[serde(default = "default_sample_window", with = "humantime_serde")]
    pub sample_window: Duration,
    /// Duration to wait after a rate change before sampling begins.
    ///
    /// When the rate advances, requests issued at the previous rate are still
    /// in flight (up to `pipeline_depth` per connection). Their responses
    /// would otherwise land in the new step's window and bias early
    /// measurements. The drain window discards that period before sampling.
    #[serde(default = "default_drain_window", with = "humantime_serde")]
    pub drain_window: Duration,
    /// Number of consecutive SLO failures before stopping.
    #[serde(default = "default_stop_after_failures")]
    pub stop_after_failures: u32,
    /// Maximum rate to try (absolute ceiling).
    #[serde(default = "default_max_rate")]
    pub max_rate: u64,
    /// Minimum ratio of achieved/target throughput (0.0-1.0).
    ///
    /// When the achieved throughput falls below this fraction of the target,
    /// the step is considered a failure regardless of latency. This detects
    /// saturation where the system cannot keep up with the target rate.
    /// Default is 0.9 (90%).
    #[serde(default = "default_min_throughput_ratio")]
    pub min_throughput_ratio: f64,
    /// Relative interval width at which bisection stops (0.0-1.0).
    /// Extra consecutive measurements required to confirm an SLO failure
    /// before the search acts on it.
    ///
    /// The search is otherwise anchored by its first failing sample: a failure
    /// during the climb sets the bisection ceiling permanently, and nothing
    /// above that rate is ever retried. A single transient -- a post-prefill
    /// writeback burst, a step-onset thundering herd, a noisy neighbour -- can
    /// therefore cap a whole run far below the real knee, and the result looks
    /// like a clean convergence rather than an error.
    ///
    /// With the default of 1 a rate must fail twice in a row to count as
    /// failed; if the retry passes, the rate is treated as passing and the
    /// search continues. Costs one extra sample window per failure. Set to 0
    /// for the previous accept-first-failure behavior.
    #[serde(default = "default_confirm_failures")]
    pub confirm_failures: u32,
    /// Minimum fractional latency improvement expected when the search halves
    /// the rate, before it concludes the target has a floor above the SLO.
    ///
    /// When no rate has passed yet, the search bisects downward looking for one.
    /// That is only productive if latency actually responds to offered rate. If
    /// it does not, the target has a structural floor above the SLO -- a device
    /// read, a fixed round trip -- and no rate will ever pass, so every further
    /// halving is wasted.
    ///
    /// Measured against a cache whose working set was 227% of RAM: the search
    /// ran from 5000 req/s down to 19, a 263x reduction, and p50 went from
    /// 668us to 889us. It got WORSE. Sixteen measurement windows, about
    /// nineteen minutes, to establish what the third window already showed.
    ///
    /// With the default of 0.10, a halving that improves the SLO percentile by
    /// less than 10% ends the search with a diagnosis rather than a bare
    /// "SLO never met". Set to 0 to disable and always bisect to the step limit.
    #[serde(default = "default_floor_improvement_ratio")]
    pub floor_improvement_ratio: f64,
    #[serde(default = "default_bisect_tolerance")]
    pub bisect_tolerance: f64,
    /// Hard cap on the number of bisection probes.
    #[serde(default = "default_max_bisect_steps")]
    pub max_bisect_steps: u32,
}

/// SLO thresholds for latency percentiles.
///
/// At least one threshold must be specified. All specified thresholds
/// must be met for the SLO to pass.
#[derive(Debug, Clone, Deserialize)]
pub struct SloThresholds {
    /// Maximum acceptable p50 latency.
    #[serde(default, with = "humantime_serde_option")]
    pub p50: Option<Duration>,
    /// Maximum acceptable p99 latency.
    #[serde(default, with = "humantime_serde_option")]
    pub p99: Option<Duration>,
    /// Maximum acceptable p99.9 latency.
    #[serde(default, with = "humantime_serde_option")]
    pub p999: Option<Duration>,
}

fn default_start_rate() -> u64 {
    1000
}

fn default_step_multiplier() -> f64 {
    1.05
}

fn default_sample_window() -> Duration {
    Duration::from_secs(5)
}

fn default_drain_window() -> Duration {
    Duration::from_millis(500)
}

pub(crate) fn default_stop_after_failures() -> u32 {
    3
}

fn default_max_rate() -> u64 {
    100_000_000
}

fn default_min_throughput_ratio() -> f64 {
    0.9
}

pub fn default_confirm_failures() -> u32 {
    1
}

pub fn default_floor_improvement_ratio() -> f64 {
    0.10
}

pub(crate) fn default_bisect_tolerance() -> f64 {
    0.05
}

pub(crate) fn default_max_bisect_steps() -> u32 {
    8
}

fn default_prefill_timeout() -> Duration {
    Duration::from_secs(300)
}

/// 60s: past Linux's 30s `dirty_expire_centisecs`, with headroom for the flush
/// itself. See `Workload::prefill_settle`.
fn default_prefill_settle() -> Duration {
    Duration::from_secs(60)
}

#[derive(Debug, Clone, Deserialize)]
pub struct Keyspace {
    #[serde(default = "default_key_length")]
    pub length: usize,
    #[serde(default = "default_key_count")]
    pub count: usize,
    #[serde(default)]
    pub distribution: Distribution,
    /// YCSB skew parameter for `distribution = "zipf"`; ignored for uniform.
    /// Larger is more skewed. Must be in (0, 10]; 0.99 is the YCSB default.
    /// The upper bound keeps every key's weight `(id + 1)^-theta` above f64
    /// underflow for any keyspace below 1e30 keys, which the per-endpoint alias
    /// tables need (an endpoint whose weights all underflow has no sampler).
    #[serde(default = "default_zipf_theta")]
    pub zipf_theta: f64,
    /// How key ids are rendered into key bytes (`hex` default, or `uuid`).
    #[serde(default)]
    pub format: KeyFormat,
    /// `distribution = "recency"` only: how many of the newest append batches
    /// form the hot window. Default 8.
    #[serde(default = "default_hot_generations")]
    pub hot_generations: usize,
    /// `distribution = "recency"` only: weight of each older hot generation
    /// relative to the one before it, in (0, 1]. 1.0 is flat across the hot
    /// window. Default 0.5.
    #[serde(default = "default_hot_decay")]
    pub hot_decay: f64,
    /// `distribution = "recency"` only: fraction of reads that land in the hot
    /// window; the rest go uniformly to the body below it. Default 0.8.
    #[serde(default = "default_hot_fraction")]
    pub hot_fraction: f64,
}

impl Default for Keyspace {
    fn default() -> Self {
        Self {
            length: default_key_length(),
            count: default_key_count(),
            distribution: Distribution::default(),
            zipf_theta: default_zipf_theta(),
            format: KeyFormat::default(),
            hot_generations: default_hot_generations(),
            hot_decay: default_hot_decay(),
            hot_fraction: default_hot_fraction(),
        }
    }
}

fn default_hot_generations() -> usize {
    8
}

fn default_hot_decay() -> f64 {
    0.5
}

fn default_hot_fraction() -> f64 {
    0.8
}

/// A single append stream: a batch of new keys written on a fixed cadence by
/// dedicated writer connections, growing the keyspace over the run.
///
/// The writer is its own role with its own connections so that a batch never
/// queues reads behind SETs inside this client: read latency then reflects
/// the server under write load, not cachecannon's own pipeline. Batches are
/// drained through the same per-endpoint queue path as prefill (routing,
/// confirm-on-response, retry), and the keyspace head advances only once a
/// whole batch is confirmed, so reads never target an unwritten key.
#[derive(Debug, Clone, Deserialize)]
pub struct Append {
    /// Keys per batch.
    pub batch: usize,
    /// Interval between batch starts, measured from the start of warmup.
    #[serde(with = "humantime_serde")]
    pub every: Duration,
    /// `burst` fires the batch as fast as the writer pipelines allow (default);
    /// `spread` paces it evenly across the interval.
    #[serde(default)]
    pub pace: AppendPace,
    /// Writer connections, distributed across threads like `[connection]`.
    /// Cluster mode needs at least one per node. Default 1.
    #[serde(default = "default_append_connections")]
    pub connections: usize,
    /// In-flight SETs per writer connection. Default 32.
    #[serde(default = "default_append_pipeline_depth")]
    pub pipeline_depth: usize,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AppendPace {
    #[default]
    Burst,
    Spread,
}

fn default_append_connections() -> usize {
    1
}

fn default_append_pipeline_depth() -> usize {
    32
}

impl Append {
    /// Writer-side SET rate for `pace = "spread"`: the batch spread evenly
    /// over the interval, at least 1/s.
    pub fn spread_rate(&self) -> u64 {
        let secs = self.every.as_secs_f64().max(1e-9);
        ((self.batch as f64 / secs).ceil() as u64).max(1)
    }
}

fn default_key_length() -> usize {
    16
}

fn default_key_count() -> usize {
    1_000_000
}

/// Upper bound on `keyspace.zipf_theta`; see that field.
pub const MAX_ZIPF_THETA: f64 = 10.0;

fn default_zipf_theta() -> f64 {
    0.99
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Distribution {
    #[default]
    Uniform,
    Zipf,
    /// Newest keys are hot, in units of append batches. Requires
    /// `[workload.append]`, which supplies the batch size and grows the
    /// keyspace over the run. See `keydist::Recency`.
    Recency,
}

/// How a key id is rendered into the on-wire key bytes.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[repr(u8)]
pub enum KeyFormat {
    /// Right-aligned lowercase hex of the id, zero-padded to `length` (default,
    /// the historical behavior).
    #[default]
    Hex = 0,
    /// Canonical 8-4-4-4-12 dashed UUID derived from the id. Requires
    /// `length = 36`. Needed by servers that key by UUID and drop the
    /// connection on a non-UUID key.
    Uuid = 1,
}

impl KeyFormat {
    /// Recover a `KeyFormat` from its `repr(u8)` discriminant (for the
    /// run-scoped atomic that carries it to `write_key`).
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => KeyFormat::Uuid,
            _ => KeyFormat::Hex,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Commands {
    #[serde(default = "default_get_ratio")]
    pub get: u8,
    #[serde(default = "default_set_ratio")]
    pub set: u8,
    #[serde(default = "default_delete_ratio")]
    pub delete: u8,
}

impl Default for Commands {
    fn default() -> Self {
        Self {
            get: default_get_ratio(),
            set: default_set_ratio(),
            delete: default_delete_ratio(),
        }
    }
}

fn default_get_ratio() -> u8 {
    80
}

fn default_set_ratio() -> u8 {
    20
}

fn default_delete_ratio() -> u8 {
    0
}

#[derive(Debug, Clone, Deserialize)]
pub struct Values {
    #[serde(default = "default_value_length")]
    pub length: usize,
}

impl Default for Values {
    fn default() -> Self {
        Self {
            length: default_value_length(),
        }
    }
}

fn default_value_length() -> usize {
    64
}

#[derive(Debug, Clone, Deserialize)]
pub struct Timestamps {
    #[serde(default = "default_timestamps_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub mode: TimestampMode,
}

impl Default for Timestamps {
    fn default() -> Self {
        Self {
            enabled: default_timestamps_enabled(),
            mode: TimestampMode::default(),
        }
    }
}

fn default_timestamps_enabled() -> bool {
    true
}

/// Fabric settings for `protocol = "dma"`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Dma {
    /// Which server module is on the other end. This selects the RESP
    /// vocabulary the transfers are issued in.
    #[serde(default)]
    pub module: DmaModule,
    /// `efa-direct` for real dma, `tcp` for a development host.
    #[serde(default)]
    pub provider: DmaProvider,
    /// Fabric domains to spread workers across, one endpoint per worker. Empty discovers all
    /// domains.
    #[serde(default)]
    pub interfaces: Vec<String>,
    /// Registered buffer size per connection. Defaults to the workload's value length. Registered
    /// memory multiplies with connection count, against `RLIMIT_MEMLOCK`.
    #[serde(default)]
    pub buffer_capacity: Option<usize>,
    /// Verify the CRC-32c the server returns with a read, and, under vdma, send one with each write.
    /// Off by default. The default module returns one with every read and takes none on a write.
    #[serde(default)]
    pub checksum: bool,
}

impl Dma {
    /// Whether a write carries a CRC-32c for the server to verify. Only vdma takes one: the default
    /// module's write command takes no checksum argument.
    pub fn checksums_writes(&self) -> bool {
        self.checksum && self.module == DmaModule::Vdma
    }
}

/// The server module a DMA run talks to.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DmaModule {
    /// vdma: `DMA.HELLO`/`DMA.GET`/`DMA.SET`, values in the ordinary keyspace.
    Vdma,
    /// valkey-large-object: `BLOB.RDMA_HELLO`/`BLOB.RDMA_GET`/`BLOB.RDMA_SET`, a module type of
    /// its own. The client sends no write checksum; a read returns one unasked.
    #[default]
    LargeObj,
}

/// Which libfabric provider a DMA run opens.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum DmaProvider {
    /// EFA's `efa-direct` fabric — the hardware path.
    #[default]
    EfaDirect,
    /// Software transport, for a development host with no RDMA hardware.
    Tcp,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TimestampMode {
    #[default]
    Userspace,
    Software,
}

/// Admin/metrics configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct Admin {
    /// Listen address for Prometheus metrics endpoint.
    #[serde(default)]
    pub listen: Option<SocketAddr>,
    /// Path to write Parquet output file.
    #[serde(default)]
    pub parquet: Option<PathBuf>,
    /// Interval for Parquet snapshots.
    #[serde(default = "default_parquet_interval", with = "humantime_serde")]
    pub parquet_interval: Duration,
    /// Output format (clean, json, verbose, quiet).
    #[serde(default, with = "output_format_serde")]
    pub format: OutputFormat,
    /// Color mode (auto, always, never).
    #[serde(default, with = "color_mode_serde")]
    pub color: ColorMode,
}

impl Default for Admin {
    fn default() -> Self {
        Self {
            listen: None,
            parquet: None,
            parquet_interval: default_parquet_interval(),
            format: OutputFormat::default(),
            color: ColorMode::default(),
        }
    }
}

fn default_parquet_interval() -> Duration {
    Duration::from_secs(1)
}

impl Config {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self, ConfigError> {
        let content =
            std::fs::read_to_string(path.as_ref()).map_err(|e| ConfigError::Io(e.to_string()))?;
        let config: Self =
            toml::from_str(&content).map_err(|e| ConfigError::Parse(e.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    /// A paced prefill takes about keys / rate seconds; one that cannot finish
    /// inside `prefill_timeout` would fail after the full timeout.
    fn validate_prefill_pace(&self) -> Result<(), ConfigError> {
        if self.workload.prefill
            && let Some(rate) = self.workload.prefill_rate_limit.filter(|&r| r > 0)
            && !self.workload.prefill_timeout.is_zero()
        {
            let needed = self.workload.keyspace.count as f64 / rate as f64;
            let timeout = self.workload.prefill_timeout.as_secs_f64();
            if needed > timeout {
                return Err(ConfigError::Validation(format!(
                    "prefill of {} keys at prefill_rate_limit = {rate} takes about \
                     {needed:.0}s, longer than prefill_timeout ({timeout:.0}s); raise \
                     prefill_timeout or set it to \"0s\"",
                    self.workload.keyspace.count
                )));
            }
        }
        Ok(())
    }

    /// Validate config invariants that can't be expressed via serde alone.
    /// [`load`](Self::load) calls it; a caller that changes a loaded or
    /// default config (valkey-lab's CLI flags) must call it again.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.target.endpoints.is_empty() {
            return Err(ConfigError::Validation(
                "endpoints must be specified".to_string(),
            ));
        }

        if self.general.threads == 0 {
            return Err(ConfigError::Validation("threads must be >= 1".to_string()));
        }

        self.validate_prefill_pace()?;

        // io_uring's provided-buffer ring is indexed by a mask, so ringline
        // rejects a non-power-of-two. Catching it here names the config key
        // that is wrong instead of failing later as a ring-setup error.
        if let Some(ring_size) = self.general.recv_ring_size {
            if ring_size == 0 {
                return Err(ConfigError::Validation(
                    "recv_ring_size must be >= 1".to_string(),
                ));
            }
            if !ring_size.is_power_of_two() {
                return Err(ConfigError::Validation(format!(
                    "recv_ring_size must be a power of two (got {ring_size})"
                )));
            }
        }

        if self.general.recv_buffer_size == Some(0) {
            return Err(ConfigError::Validation(
                "recv_buffer_size must be >= 1".to_string(),
            ));
        }

        // A client certificate is useless without its key and vice versa, and
        // rustls takes them as a pair. Catch a half-configured client identity
        // rather than at connect time, where it would surface as a handshake
        // failure against every endpoint.
        match (&self.target.tls_cert_file, &self.target.tls_key_file) {
            (Some(_), None) => {
                return Err(ConfigError::Validation(
                    "tls_cert_file requires tls_key_file (the client key for mutual TLS)"
                        .to_string(),
                ));
            }
            (None, Some(_)) => {
                return Err(ConfigError::Validation(
                    "tls_key_file requires tls_cert_file (the client certificate chain)"
                        .to_string(),
                ));
            }
            _ => {}
        }

        // Certificate options only take effect on a TLS connection. Silently
        // ignoring them would look like the CA file was honored.
        if !self.target.tls
            && (self.target.tls_ca_file.is_some()
                || self.target.tls_cert_file.is_some()
                || self.target.tls_key_file.is_some())
        {
            return Err(ConfigError::Validation(
                "tls_ca_file / tls_cert_file / tls_key_file require tls = true".to_string(),
            ));
        }

        if self.target.protocol == Protocol::Dma {
            #[cfg(not(feature = "dma"))]
            return Err(ConfigError::Validation(
                "protocol = \"dma\" requires a build with --features dma (needs libfabric)"
                    .to_string(),
            ));

            #[cfg(feature = "dma")]
            if self.connection.pipeline_depth != 1 {
                return Err(ConfigError::Validation(format!(
                    "protocol = \"dma\" currently runs one transfer per connection; set \
                     connection.pipeline_depth = 1 (got {}) and raise connections for concurrency",
                    self.connection.pipeline_depth,
                )));
            }

            #[cfg(feature = "dma")]
            if self.workload.append.is_some() {
                return Err(ConfigError::Validation(
                    "protocol = \"dma\" does not support [workload.append]".to_string(),
                ));
            }

            #[cfg(feature = "dma")]
            if let Some(capacity) = self.dma.buffer_capacity
                && capacity < self.workload.values.length
            {
                return Err(ConfigError::Validation(format!(
                    "dma.buffer_capacity ({capacity}) must hold workload.values.length ({})",
                    self.workload.values.length,
                )));
            }
        }

        if self.connection.total_connections() == 0 {
            return Err(ConfigError::Validation(
                "connections must be >= 1".to_string(),
            ));
        }

        if self.workload.values.length == 0 {
            return Err(ConfigError::Validation(
                "workload.values.length must be >= 1".to_string(),
            ));
        }

        if self.workload.keyspace.format == KeyFormat::Uuid && self.workload.keyspace.length != 36 {
            return Err(ConfigError::Validation(
                "keyspace.format = uuid requires keyspace.length = 36".to_string(),
            ));
        }

        if matches!(self.workload.keyspace.distribution, Distribution::Zipf) {
            let theta = self.workload.keyspace.zipf_theta;
            // is_finite() first so NaN is caught before the ordered comparison.
            // rand_distr's rejection-inversion handles theta == 1, so unlike a
            // zeta-based generator there is no singularity to exclude here.
            if !theta.is_finite() || theta <= 0.0 || theta > MAX_ZIPF_THETA {
                return Err(ConfigError::Validation(format!(
                    "keyspace.zipf_theta must be in (0, {MAX_ZIPF_THETA}] (got {theta})"
                )));
            }
        }

        if self.connection.endpoint_allocation == EndpointAllocation::KeyShare {
            if self.workload.keyspace.distribution != Distribution::Zipf {
                return Err(ConfigError::Validation(
                    "connection.endpoint_allocation = \"key_share\" requires \
                     keyspace.distribution = \"zipf\""
                        .to_string(),
                ));
            }
            let endpoints = self.target.endpoints.len();
            if endpoints > 1 && self.connection.total_connections() < endpoints {
                return Err(ConfigError::Validation(format!(
                    "connection.endpoint_allocation = \"key_share\" requires connections >= \
                     {endpoints}, the number of endpoints (validation runs before keys are \
                     assigned, so it counts endpoints that may own none)"
                )));
            }
        }

        let is_recency = self.workload.keyspace.distribution == Distribution::Recency;
        match (&self.workload.append, is_recency) {
            (None, true) => {
                return Err(ConfigError::Validation(
                    "keyspace.distribution = \"recency\" requires a [workload.append] section \
                     (it supplies the batch size the hot window is measured in)"
                        .to_string(),
                ));
            }
            (Some(_), false) => {
                return Err(ConfigError::Validation(
                    "[workload.append] requires keyspace.distribution = \"recency\" \
                     (the stationary distributions cannot follow a growing keyspace)"
                        .to_string(),
                ));
            }
            _ => {}
        }
        if is_recency {
            let ks = &self.workload.keyspace;
            if ks.hot_generations == 0 {
                return Err(ConfigError::Validation(
                    "keyspace.hot_generations must be >= 1".to_string(),
                ));
            }
            if !ks.hot_decay.is_finite() || ks.hot_decay <= 0.0 || ks.hot_decay > 1.0 {
                return Err(ConfigError::Validation(format!(
                    "keyspace.hot_decay must be in (0, 1] (got {})",
                    ks.hot_decay
                )));
            }
            if !ks.hot_fraction.is_finite() || !(0.0..=1.0).contains(&ks.hot_fraction) {
                return Err(ConfigError::Validation(format!(
                    "keyspace.hot_fraction must be in [0, 1] (got {})",
                    ks.hot_fraction
                )));
            }
            if ks.count == 0 {
                return Err(ConfigError::Validation(
                    "keyspace.count must be >= 1 with distribution = \"recency\"".to_string(),
                ));
            }
        }
        if let Some(ref append) = self.workload.append {
            if append.batch == 0 {
                return Err(ConfigError::Validation(
                    "append.batch must be >= 1".to_string(),
                ));
            }
            if append.every.is_zero() {
                return Err(ConfigError::Validation(
                    "append.every must be > 0".to_string(),
                ));
            }
            if append.connections == 0 {
                return Err(ConfigError::Validation(
                    "append.connections must be >= 1".to_string(),
                ));
            }
            if append.pipeline_depth == 0 {
                return Err(ConfigError::Validation(
                    "append.pipeline_depth must be >= 1".to_string(),
                ));
            }
            if matches!(self.target.protocol, Protocol::Ping) {
                return Err(ConfigError::Validation(
                    "[workload.append] needs a protocol with SET (resp, resp3, memcache, \
                     memcache-binary); ping has none"
                        .to_string(),
                ));
            }
            if self.target.endpoints.len() > 1 && append.connections < self.target.endpoints.len() {
                return Err(ConfigError::Validation(format!(
                    "append.connections ({}) must be >= the number of endpoints ({}) so every \
                     node's append queue has a writer draining it",
                    append.connections,
                    self.target.endpoints.len()
                )));
            }
        }

        if self.workload.read_values && matches!(self.target.protocol, Protocol::Ping) {
            return Err(ConfigError::Validation(
                "workload.read_values needs a protocol with GET (resp, resp3, memcache, \
                 memcache-binary); ping has none"
                    .to_string(),
            ));
        }

        // The DMA path never reads values through the RESP receive path that option changes.
        if self.workload.read_values && self.target.protocol == Protocol::Dma {
            return Err(ConfigError::Validation(
                "workload.read_values does not apply to protocol = \"dma\"; use \
                 dma.checksum = true to checksum each value a GET returns"
                    .to_string(),
            ));
        }

        if self.workload.values.length > crate::runner::VALUE_POOL_SIZE {
            return Err(ConfigError::Validation(format!(
                "workload.values.length ({}) exceeds the value pool size ({})",
                self.workload.values.length,
                crate::runner::VALUE_POOL_SIZE,
            )));
        }

        #[cfg(not(target_os = "linux"))]
        if matches!(self.timestamps.mode, TimestampMode::Software) {
            return Err(ConfigError::Validation(
                "timestamps.mode = \"software\" (kernel SO_TIMESTAMPING) is only supported on Linux"
                    .to_string(),
            ));
        }

        let cmds = &self.workload.commands;
        if cmds.get == 0 && cmds.set == 0 && cmds.delete == 0 {
            return Err(ConfigError::Validation(
                "at least one command ratio (get, set, delete) must be > 0".to_string(),
            ));
        }

        if let Some(ref kv) = self.workload.kv_replay {
            self.validate_kv_replay(kv)?;
        }

        if let Some(ref sat) = self.workload.saturation_search {
            if sat.start_rate == 0 {
                return Err(ConfigError::Validation(
                    "saturation_search.start_rate must be > 0".to_string(),
                ));
            }

            if sat.step_multiplier <= 1.0 {
                return Err(ConfigError::Validation(
                    "saturation_search.step_multiplier must be > 1.0".to_string(),
                ));
            }

            if !(0.0..=1.0).contains(&sat.min_throughput_ratio) {
                return Err(ConfigError::Validation(
                    "saturation_search.min_throughput_ratio must be between 0.0 and 1.0"
                        .to_string(),
                ));
            }

            if sat.slo.p50.is_none() && sat.slo.p99.is_none() && sat.slo.p999.is_none() {
                return Err(ConfigError::Validation(
                    "saturation_search.slo must have at least one threshold (p50, p99, or p999)"
                        .to_string(),
                ));
            }
        }

        Ok(())
    }
}

impl Config {
    /// `[workload.kv_replay]` checks that need no file access. The trace and
    /// the model config are read when the run starts
    /// (`crate::kvreplay::KvReplay::load`).
    fn validate_kv_replay(&self, kv: &KvReplay) -> Result<(), ConfigError> {
        let bad = |msg: &str| {
            Err(ConfigError::Validation(format!(
                "workload.kv_replay: {msg}"
            )))
        };
        match (&kv.model_config, kv.kv_bytes_per_token) {
            (Some(_), Some(_)) => return bad("set model_config or kv_bytes_per_token, not both"),
            (None, None) => return bad("one of model_config or kv_bytes_per_token is required"),
            (None, Some(0)) => return bad("kv_bytes_per_token must be >= 1"),
            _ => {}
        }
        if kv.kv_dtype.is_some() && kv.model_config.is_none() {
            return bad("kv_dtype applies only with model_config");
        }
        if kv.mla && kv.model_config.is_some() {
            return bad("mla applies only with kv_bytes_per_token; a model_config says it itself");
        }
        if !kv.speedup.is_finite() || kv.speedup < 0.0 {
            return bad(&format!(
                "speedup must be finite and >= 0 (0 = closed loop), got {}",
                kv.speedup
            ));
        }
        // With tp > 1 every rank's operations would share the request's one
        // connection; an engine's ranks each have their own (#214).
        if kv.tp != 1 {
            return bad(&format!(
                "tp must be 1, got {}: tensor parallelism needs one connection per rank, \
                 which kv_replay does not do yet (cachecannon#214)",
                kv.tp
            ));
        }
        // A request's GETs and SETs are sent at most pipeline_depth at a time;
        // with 0 none would be sent and the replay would wait for a reply to
        // nothing, with no request in flight to time out.
        if self.connection.pipeline_depth == 0 {
            return bad("needs connection.pipeline_depth >= 1");
        }
        // The lookup is one pipeline of EXISTS per connection, so every key a
        // request touches must live on the server that connection reaches.
        if !matches!(self.target.protocol, Protocol::Resp | Protocol::Resp3) {
            return bad("needs protocol resp or resp3 (it issues EXISTS, GET and SET)");
        }
        if self.target.cluster || self.target.endpoints.len() > 1 {
            return bad(
                "needs a single endpoint; cluster and multi-endpoint routing are not supported",
            );
        }
        // The trace sets the request sequence and its timing, so the options
        // that generate or pace synthetic requests do not apply.
        let w = &self.workload;
        for (set, key) in [
            (w.rate_limit.is_some_and(|r| r > 0), "rate_limit"),
            (
                w.saturation_search.is_some(),
                "[workload.saturation_search]",
            ),
            (w.prefill, "prefill"),
            (w.backfill_on_miss, "backfill_on_miss"),
            (w.read_values, "read_values"),
            (w.append.is_some(), "[workload.append]"),
        ] {
            if set {
                return bad(&format!("cannot be combined with {key}"));
            }
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config: {0}")]
    Io(String),
    #[error("failed to parse config: {0}")]
    Parse(String),
    #[error("invalid config: {0}")]
    Validation(String),
}

/// Parse a Linux-style CPU list string into a vector of CPU IDs.
///
/// Examples:
/// - "0-3" -> [0, 1, 2, 3]
/// - "0,2,4" -> [0, 2, 4]
/// - "0-3,8-11,13" -> [0, 1, 2, 3, 8, 9, 10, 11, 13]
pub fn parse_cpu_list(s: &str) -> Result<Vec<usize>, String> {
    let mut cpus = Vec::new();

    for part in s.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }

        if let Some((start, end)) = part.split_once('-') {
            let start: usize = start
                .trim()
                .parse()
                .map_err(|_| format!("invalid CPU number: {}", start))?;
            let end: usize = end
                .trim()
                .parse()
                .map_err(|_| format!("invalid CPU number: {}", end))?;

            if start > end {
                return Err(format!("invalid range: {} > {}", start, end));
            }

            for cpu in start..=end {
                cpus.push(cpu);
            }
        } else {
            let cpu: usize = part
                .parse()
                .map_err(|_| format!("invalid CPU number: {}", part))?;
            cpus.push(cpu);
        }
    }

    // Remove duplicates and sort
    cpus.sort_unstable();
    cpus.dedup();

    Ok(cpus)
}

#[cfg(test)]
mod cpu_list_tests {
    use super::*;

    #[test]
    fn test_single_cpu() {
        assert_eq!(parse_cpu_list("0").unwrap(), vec![0]);
        assert_eq!(parse_cpu_list("5").unwrap(), vec![5]);
    }

    #[test]
    fn test_range() {
        assert_eq!(parse_cpu_list("0-3").unwrap(), vec![0, 1, 2, 3]);
        assert_eq!(parse_cpu_list("8-11").unwrap(), vec![8, 9, 10, 11]);
    }

    #[test]
    fn test_list() {
        assert_eq!(parse_cpu_list("0,2,4").unwrap(), vec![0, 2, 4]);
    }

    #[test]
    fn test_mixed() {
        assert_eq!(
            parse_cpu_list("0-3,8-11,13").unwrap(),
            vec![0, 1, 2, 3, 8, 9, 10, 11, 13]
        );
    }

    #[test]
    fn test_with_spaces() {
        assert_eq!(
            parse_cpu_list("0-3, 8-11, 13").unwrap(),
            vec![0, 1, 2, 3, 8, 9, 10, 11, 13]
        );
    }

    #[test]
    fn test_duplicates_removed() {
        assert_eq!(parse_cpu_list("0,0,1,1").unwrap(), vec![0, 1]);
    }

    #[test]
    fn test_invalid_range() {
        assert!(parse_cpu_list("3-0").is_err());
    }
}

#[cfg(test)]
mod validation_tests {
    use super::*;

    fn parse_config(toml: &str) -> Result<Config, ConfigError> {
        let config: Config = toml::from_str(toml).map_err(|e| ConfigError::Parse(e.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    /// A config with `[workload.kv_replay]` set to `kv` and `extra` appended.
    fn kv_config(kv: &str, extra: &str) -> Result<Config, ConfigError> {
        parse_config(&format!(
            "[target]\nendpoints = [\"127.0.0.1:6379\"]\n{extra}\n\
             [workload.kv_replay]\ntrace = \"t.parquet\"\n{kv}\n"
        ))
    }

    #[test]
    fn kv_replay_defaults() {
        let config = kv_config("kv_bytes_per_token = 4", "").unwrap();
        let kv = config.workload.kv_replay.unwrap();
        assert_eq!(kv.speedup, 1.0);
        assert_eq!(kv.tp, 1);
        assert_eq!(kv.write_rule, KvWriteRule::SetFromMiss);
        assert_eq!(kv.key_prefix, "kv:");
        assert_eq!(kv.key_format, KvKeyFormat::Hex);
        let kv = kv_config(
            "kv_bytes_per_token = 4\nwrite_rule = \"insert-if-absent\"\nkey_format = \"decimal\"",
            "",
        )
        .unwrap()
        .workload
        .kv_replay
        .unwrap();
        assert_eq!(kv.write_rule, KvWriteRule::InsertIfAbsent);
        assert_eq!(kv.key_format, KvKeyFormat::Decimal);
    }

    #[test]
    fn kv_replay_needs_exactly_one_geometry() {
        let err = kv_config("", "").unwrap_err().to_string();
        assert!(
            err.contains("one of model_config or kv_bytes_per_token"),
            "{err}"
        );
        let err = kv_config("kv_bytes_per_token = 4\nmodel_config = \"c.json\"", "")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not both"), "{err}");
        let err = kv_config("kv_bytes_per_token = 4\nkv_dtype = \"fp8\"", "")
            .unwrap_err()
            .to_string();
        assert!(err.contains("kv_dtype"), "{err}");
        let err = kv_config("model_config = \"c.json\"\nmla = true", "")
            .unwrap_err()
            .to_string();
        assert!(err.contains("mla"), "{err}");
    }

    #[test]
    fn kv_replay_rejects_bad_values_and_synthetic_options() {
        for (kv, extra, want) in [
            ("speedup = -1.0", "", "speedup"),
            ("tp = 0", "", "tp must be 1"),
            ("tp = 2", "", "one connection per rank"),
            ("", "[connection]\npipeline_depth = 0", "pipeline_depth"),
            ("", "[workload]\nrate_limit = 1000", "rate_limit"),
            ("", "[workload]\nprefill = true", "prefill"),
            ("", "[workload]\nread_values = true", "read_values"),
            (
                "",
                "[workload]\nbackfill_on_miss = true",
                "backfill_on_miss",
            ),
        ] {
            let err = kv_config(&format!("kv_bytes_per_token = 4\n{kv}"), extra)
                .unwrap_err()
                .to_string();
            assert!(err.contains(want), "{kv} {extra}: {err}");
        }
        // Closed loop.
        kv_config("kv_bytes_per_token = 4\nspeedup = 0", "").unwrap();
        let err = parse_config(
            "[target]\nendpoints = [\"127.0.0.1:11211\"]\nprotocol = \"memcache\"\n\
             [workload.kv_replay]\ntrace = \"t\"\nkv_bytes_per_token = 4\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("resp"), "{err}");
        let err = parse_config(
            "[target]\nendpoints = [\"127.0.0.1:6379\", \"127.0.0.1:6380\"]\n\
             [workload.kv_replay]\ntrace = \"t\"\nkv_bytes_per_token = 4\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("single endpoint"), "{err}");
    }

    #[test]
    fn seed_defaults_unset_and_parses() {
        let config = parse_config("[target]\nendpoints = [\"127.0.0.1:6379\"]\n").unwrap();
        assert_eq!(
            config.general.seed, None,
            "unset means the runner draws one"
        );
        let config =
            parse_config("[general]\nseed = 12345\n[target]\nendpoints = [\"127.0.0.1:6379\"]\n")
                .unwrap();
        assert_eq!(config.general.seed, Some(12345));
    }

    #[test]
    fn ringline_diag_defaults_off_and_parses() {
        let config = parse_config("[target]\nendpoints = [\"127.0.0.1:6379\"]\n").unwrap();
        assert!(!config.general.ringline_diag);
        let config = parse_config(
            "[general]\nringline_diag = true\n[target]\nendpoints = [\"127.0.0.1:6379\"]\n",
        )
        .unwrap();
        assert!(config.general.ringline_diag);
    }

    #[test]
    fn valid_minimal_config() {
        let config = parse_config(
            r#"
            [target]
            endpoints = ["127.0.0.1:6379"]
            "#,
        );
        assert!(config.is_ok());
    }

    #[test]
    fn accepts_memcache_binary() {
        // memcache-binary is a supported protocol (driven via ringline-memcache's
        // BinaryClient), so it must parse and validate like the other protocols.
        let cfg = parse_config(
            r#"
            [target]
            endpoints = ["127.0.0.1:11211"]
            protocol = "memcache-binary"
            "#,
        )
        .expect("memcache-binary config should be valid");
        assert_eq!(cfg.target.protocol, Protocol::MemcacheBinary);
    }

    #[test]
    fn read_values_defaults_off() {
        let parse = |workload: &str| {
            toml::from_str::<Config>(&format!(
                "[target]\nendpoints = [\"127.0.0.1:6379\"]\n[workload]\n{workload}"
            ))
            .expect("parses")
            .workload
            .read_values
        };
        assert!(!parse(""));
        assert!(parse("read_values = true"));
    }

    #[test]
    fn read_values_rejected_for_ping() {
        let err = toml::from_str::<Config>(
            "[target]\nendpoints = [\"127.0.0.1:6379\"]\nprotocol = \"ping\"\n\
             [workload]\nread_values = true",
        )
        .expect("parses")
        .validate()
        .unwrap_err();
        assert!(err.to_string().contains("read_values"), "{err}");
    }

    #[cfg(feature = "dma")]
    #[test]
    fn read_values_rejected_for_dma() {
        let err = toml::from_str::<Config>(
            "[target]\nendpoints = [\"127.0.0.1:6379\"]\nprotocol = \"dma\"\n\
             [workload]\nread_values = true",
        )
        .expect("parses")
        .validate()
        .unwrap_err();
        assert!(err.to_string().contains("dma.checksum"), "{err}");
    }

    #[test]
    fn accepts_zipf_theta_of_one() {
        // rejection-inversion has no singularity at theta == 1, so it is a legal
        // (and commonly quoted) skew rather than something to exclude.
        toml::from_str::<Config>(
            r#"
            [target]
            endpoints = ["127.0.0.1:11211"]
            protocol = "memcache-binary"
            [workload.keyspace]
            distribution = "zipf"
            zipf_theta = 1.0
            "#,
        )
        .expect("parses")
        .validate()
        .expect("theta = 1 must be accepted");
    }

    #[test]
    fn rejects_nonpositive_or_nan_zipf_theta() {
        for bad in ["0.0", "-0.5", "nan", "10.5", "inf"] {
            let err = toml::from_str::<Config>(&format!(
                r#"
                [target]
                endpoints = ["127.0.0.1:11211"]
                protocol = "memcache-binary"
                [workload.keyspace]
                distribution = "zipf"
                zipf_theta = {bad}
                "#
            ))
            .expect("parses")
            .validate()
            .unwrap_err();
            assert!(
                format!("{err:?}").contains("zipf_theta"),
                "theta={bad} gave unexpected error: {err:?}"
            );
        }
    }

    #[test]
    fn accepts_zipf_with_multiple_endpoints() {
        // Skew across a sharded fleet is supported. Each connection samples zipf
        // restricted to its endpoint's keys. Shards see load in proportion to
        // their zipf share only with `connection.endpoint_allocation =
        // "key_share"`; the default gives every endpoint the same connections.
        toml::from_str::<Config>(
            r#"
            [target]
            endpoints = ["127.0.0.1:11211", "127.0.0.1:11212"]
            protocol = "memcache-binary"
            [workload.keyspace]
            distribution = "zipf"
            "#,
        )
        .expect("parses")
        .validate()
        .expect("zipf across shards must be allowed");
    }

    #[test]
    fn endpoint_allocation_defaults_to_even_and_parses_key_share() {
        let base = r#"
            [target]
            endpoints = ["127.0.0.1:6379", "127.0.0.1:6380"]
            protocol = "resp"
            [workload.keyspace]
            distribution = "zipf"
            "#;
        let cfg: Config = toml::from_str(base).expect("parses");
        assert_eq!(cfg.connection.endpoint_allocation, EndpointAllocation::Even);
        let cfg: Config = toml::from_str(&format!(
            "{base}\n[connection]\nconnections = 2\nendpoint_allocation = \"key_share\"\n"
        ))
        .expect("parses");
        assert_eq!(
            cfg.connection.endpoint_allocation,
            EndpointAllocation::KeyShare
        );
        cfg.validate().expect("key_share with zipf is valid");
    }

    #[test]
    fn key_share_requires_zipf_and_a_connection_per_endpoint() {
        let cfg = |dist: &str, conns: usize| {
            toml::from_str::<Config>(&format!(
                r#"
                [target]
                endpoints = ["127.0.0.1:6379", "127.0.0.1:6380", "127.0.0.1:6381"]
                protocol = "resp"
                [connection]
                connections = {conns}
                endpoint_allocation = "key_share"
                [workload.keyspace]
                distribution = "{dist}"
                "#
            ))
            .expect("parses")
            .validate()
        };
        let err = cfg("uniform", 3).unwrap_err();
        assert!(format!("{err:?}").contains("requires"), "{err:?}");
        let err = cfg("zipf", 2).unwrap_err();
        assert!(format!("{err:?}").contains(">= 3"), "{err:?}");
        cfg("zipf", 3).expect("one connection per endpoint is enough");
    }

    #[test]
    fn zipf_theta_defaults_to_ycsb_value() {
        let cfg: Config = toml::from_str(
            r#"
            [target]
            endpoints = ["127.0.0.1:11211"]
            protocol = "memcache-binary"
            [workload.keyspace]
            distribution = "zipf"
            "#,
        )
        .expect("parses");
        cfg.validate().expect("default theta must be valid");
        assert_eq!(cfg.workload.keyspace.zipf_theta, 0.99);
    }

    #[test]
    fn accepts_uuid_keyspace() {
        let cfg = parse_config(
            r#"
            [target]
            endpoints = ["127.0.0.1:11211"]
            [workload.keyspace]
            length = 36
            format = "uuid"
            "#,
        )
        .expect("uuid keyspace with length 36 should be valid");
        assert_eq!(cfg.workload.keyspace.format, KeyFormat::Uuid);
    }

    #[test]
    fn rejects_uuid_keyspace_wrong_length() {
        let err = parse_config(
            r#"
            [target]
            endpoints = ["127.0.0.1:11211"]
            [workload.keyspace]
            length = 16
            format = "uuid"
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("length = 36"));
    }

    #[test]
    fn rejects_a_paced_prefill_longer_than_its_timeout() {
        let cfg = |timeout: &str, prefill: bool| {
            parse_config(&format!(
                r#"
                [target]
                endpoints = ["127.0.0.1:6379"]
                [workload]
                prefill = {prefill}
                prefill_rate_limit = 1000
                prefill_timeout = "{timeout}"
                [workload.keyspace]
                count = 400000
                "#
            ))
        };
        // 400 s of prefill against the default 300 s timeout.
        let err = cfg("300s", true).unwrap_err().to_string();
        assert!(err.contains("prefill_timeout"), "{err}");
        assert!(cfg("500s", true).is_ok());
        assert!(cfg("0s", true).is_ok());
        assert!(cfg("300s", false).is_ok());
    }

    #[test]
    fn rejects_zero_threads() {
        let err = parse_config(
            r#"
            [general]
            threads = 0
            [target]
            endpoints = ["127.0.0.1:6379"]
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("threads"));
    }

    #[test]
    fn rejects_a_non_power_of_two_recv_ring() {
        // io_uring masks the ring index, so ringline rejects this too -- but as
        // a ring-setup error that never names the config key.
        let err = parse_config(
            r#"
            [general]
            recv_ring_size = 300
            [target]
            endpoints = ["127.0.0.1:6379"]
            "#,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("recv_ring_size"), "{msg}");
        assert!(msg.contains("power of two"), "{msg}");
    }

    #[test]
    fn rejects_empty_recv_geometry() {
        for key in ["recv_ring_size", "recv_buffer_size"] {
            let err = parse_config(&format!(
                r#"
                [general]
                {key} = 0
                [target]
                endpoints = ["127.0.0.1:6379"]
                "#
            ))
            .unwrap_err();
            assert!(
                err.to_string().contains(key),
                "{key} not named in the error"
            );
        }
    }

    #[test]
    fn recv_geometry_is_unset_by_default() {
        // Unset is what selects the derived buffer size and ringline's ring
        // depth; a stray default here would silently pin both.
        let config = parse_config(
            r#"
            [target]
            endpoints = ["127.0.0.1:6379"]
            "#,
        )
        .unwrap();
        assert_eq!(config.general.recv_ring_size, None);
        assert_eq!(config.general.recv_buffer_size, None);
    }

    #[test]
    fn recv_geometry_round_trips_from_toml() {
        let config = parse_config(
            r#"
            [general]
            recv_ring_size = 4096
            recv_buffer_size = 65536
            [target]
            endpoints = ["127.0.0.1:6379"]
            "#,
        )
        .unwrap();
        assert_eq!(config.general.recv_ring_size, Some(4096));
        assert_eq!(config.general.recv_buffer_size, Some(65536));
    }

    #[test]
    fn rejects_zero_connections() {
        let err = parse_config(
            r#"
            [target]
            endpoints = ["127.0.0.1:6379"]
            [connection]
            connections = 0
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("connections"));
    }

    #[test]
    fn rejects_all_zero_command_ratios() {
        let err = parse_config(
            r#"
            [target]
            endpoints = ["127.0.0.1:6379"]
            [workload.commands]
            get = 0
            set = 0
            delete = 0
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("command ratio"));
    }

    #[test]
    fn rejects_step_multiplier_at_one() {
        let err = parse_config(
            r#"
            [target]
            endpoints = ["127.0.0.1:6379"]
            [workload.saturation_search]
            step_multiplier = 1.0
            [workload.saturation_search.slo]
            p99 = "1ms"
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("step_multiplier"));
    }

    #[test]
    fn rejects_step_multiplier_below_one() {
        let err = parse_config(
            r#"
            [target]
            endpoints = ["127.0.0.1:6379"]
            [workload.saturation_search]
            step_multiplier = 0.5
            [workload.saturation_search.slo]
            p99 = "1ms"
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("step_multiplier"));
    }

    #[test]
    fn rejects_zero_start_rate() {
        let err = parse_config(
            r#"
            [target]
            endpoints = ["127.0.0.1:6379"]
            [workload.saturation_search]
            start_rate = 0
            [workload.saturation_search.slo]
            p99 = "1ms"
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("start_rate"));
    }

    #[test]
    fn rejects_min_throughput_ratio_out_of_range() {
        let err = parse_config(
            r#"
            [target]
            endpoints = ["127.0.0.1:6379"]
            [workload.saturation_search]
            min_throughput_ratio = 1.5
            [workload.saturation_search.slo]
            p99 = "1ms"
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("min_throughput_ratio"));
    }

    #[test]
    fn rejects_saturation_search_without_slo_thresholds() {
        let err = parse_config(
            r#"
            [target]
            endpoints = ["127.0.0.1:6379"]
            [workload.saturation_search]
            [workload.saturation_search.slo]
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("at least one threshold"));
    }

    #[test]
    fn accepts_valid_saturation_search() {
        let config = parse_config(
            r#"
            [target]
            endpoints = ["127.0.0.1:6379"]
            [workload.saturation_search]
            step_multiplier = 1.05
            min_throughput_ratio = 0.9
            [workload.saturation_search.slo]
            p99 = "1ms"
            "#,
        );
        assert!(config.is_ok());
    }

    #[test]
    fn rejects_zero_value_length() {
        let err = parse_config(
            r#"
            [target]
            endpoints = ["127.0.0.1:6379"]
            [workload.values]
            length = 0
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("values.length"));
    }

    #[test]
    fn rejects_value_length_exceeding_pool() {
        let err = parse_config(
            r#"
            [target]
            endpoints = ["127.0.0.1:6379"]
            [workload.values]
            length = 2_000_000_000
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("value pool size"));
    }
}

mod humantime_serde {
    use serde::{Deserialize, Deserializer};
    use std::time::Duration;

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        humantime_parse(&s).map_err(serde::de::Error::custom)
    }

    fn humantime_parse(s: &str) -> Result<Duration, String> {
        // Simple parser for durations like "100ns", "500us", "1ms", "60s", "10m", "1h", or bare seconds
        let s = s.trim();
        if s.is_empty() {
            return Err("empty duration".to_string());
        }

        let (num, suffix) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));

        let value: u64 = num.parse().map_err(|e| format!("invalid number: {e}"))?;

        let multiplier = match suffix.trim() {
            "s" | "sec" | "secs" => 1,
            "m" | "min" | "mins" => 60,
            "h" | "hr" | "hrs" | "hour" | "hours" => 3600,
            "ms" => return Ok(Duration::from_millis(value)),
            "us" => return Ok(Duration::from_micros(value)),
            "ns" => return Ok(Duration::from_nanos(value)),
            "" => 1, // default to seconds
            other => return Err(format!("unknown time unit: {other}")),
        };

        Ok(Duration::from_secs(value * multiplier))
    }
}

mod humantime_serde_option {
    use serde::{Deserialize, Deserializer};
    use std::time::Duration;

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Duration>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s: Option<String> = Option::deserialize(deserializer)?;
        match s {
            Some(s) => humantime_parse(&s)
                .map(Some)
                .map_err(serde::de::Error::custom),
            None => Ok(None),
        }
    }

    fn humantime_parse(s: &str) -> Result<Duration, String> {
        let s = s.trim();
        if s.is_empty() {
            return Err("empty duration".to_string());
        }

        let (num, suffix) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));

        let value: u64 = num.parse().map_err(|e| format!("invalid number: {e}"))?;

        let multiplier = match suffix.trim() {
            "s" | "sec" | "secs" => 1,
            "m" | "min" | "mins" => 60,
            "h" | "hr" | "hrs" | "hour" | "hours" => 3600,
            "ms" => return Ok(Duration::from_millis(value)),
            "us" => return Ok(Duration::from_micros(value)),
            "ns" => return Ok(Duration::from_nanos(value)),
            "" => 1, // default to seconds
            other => return Err(format!("unknown time unit: {other}")),
        };

        Ok(Duration::from_secs(value * multiplier))
    }
}

mod output_format_serde {
    use crate::output::OutputFormat;
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D>(deserializer: D) -> Result<OutputFormat, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

mod color_mode_serde {
    use crate::output::ColorMode;
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D>(deserializer: D) -> Result<ColorMode, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tls_config_tests {
    use super::Config;

    /// Minimal valid document; individual tests append `[target]` keys.
    fn config_with_target(extra: &str) -> Result<Config, super::ConfigError> {
        let doc = format!(
            r#"
[target]
endpoints = ["127.0.0.1:11211"]
{extra}

[connection]
connections = 1
"#
        );
        let config: Config = toml::from_str(&doc).expect("test config should parse");
        config.validate().map(|()| config)
    }

    #[test]
    fn client_cert_without_key_is_rejected() {
        let err = config_with_target("tls = true\ntls_cert_file = \"client.crt\"")
            .expect_err("a client cert without its key must not validate");
        assert!(
            format!("{err:?}").contains("tls_key_file"),
            "error should name the missing option: {err:?}"
        );
    }

    #[test]
    fn client_key_without_cert_is_rejected() {
        let err = config_with_target("tls = true\ntls_key_file = \"client.key\"")
            .expect_err("a client key without its cert must not validate");
        assert!(
            format!("{err:?}").contains("tls_cert_file"),
            "error should name the missing option: {err:?}"
        );
    }

    #[test]
    fn cert_options_without_tls_are_rejected() {
        // Silently ignoring these would look like the CA file was honored.
        let err = config_with_target("tls_ca_file = \"ca.pem\"")
            .expect_err("cert options without tls = true must not validate");
        assert!(
            format!("{err:?}").contains("tls = true"),
            "error should say TLS must be enabled: {err:?}"
        );
    }

    #[test]
    fn matched_cert_and_key_with_tls_validates() {
        config_with_target(
            "tls = true\ntls_ca_file = \"ca.pem\"\ntls_cert_file = \"c.crt\"\ntls_key_file = \"c.key\"",
        )
        .expect("a complete client-auth config should validate");
    }

    #[test]
    fn tls_options_remain_optional() {
        let config = config_with_target("").expect("plain config should validate");
        assert!(config.target.tls_ca_file.is_none());
        assert!(config.target.tls_cert_file.is_none());
        assert!(config.target.tls_key_file.is_none());
    }
}

#[cfg(test)]
mod append_config_tests {
    use super::*;
    use std::time::Duration;

    fn parse_config(toml: &str) -> Result<Config, ConfigError> {
        let config: Config = toml::from_str(toml).map_err(|e| ConfigError::Parse(e.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    fn append_config(keyspace: &str, append: &str) -> Result<Config, ConfigError> {
        parse_config(&format!(
            r#"
            [target]
            endpoints = ["127.0.0.1:6379"]
            protocol = "resp"
            [workload.keyspace]
            count = 10000
            {keyspace}
            {append}
            "#
        ))
    }

    #[test]
    fn append_with_recency_validates_and_applies_defaults() {
        let cfg = append_config(
            "distribution = \"recency\"",
            "[workload.append]\nbatch = 100\nevery = \"1s\"",
        )
        .expect("recency + append should validate");
        let a = cfg.workload.append.expect("append section parsed");
        assert_eq!(a.batch, 100);
        assert_eq!(a.every, Duration::from_secs(1));
        assert_eq!(a.pace, AppendPace::Burst);
        assert_eq!(a.connections, 1);
        assert_eq!(a.pipeline_depth, 32);
        assert_eq!(cfg.workload.keyspace.hot_generations, 8);
        assert!((cfg.workload.keyspace.hot_decay - 0.5).abs() < 1e-12);
        assert!((cfg.workload.keyspace.hot_fraction - 0.8).abs() < 1e-12);
    }

    #[test]
    fn recency_without_append_is_rejected() {
        let err = append_config("distribution = \"recency\"", "")
            .expect_err("recency needs the batch size from [workload.append]");
        assert!(format!("{err:?}").contains("workload.append"), "{err:?}");
    }

    #[test]
    fn append_without_recency_is_rejected() {
        let err = append_config("", "[workload.append]\nbatch = 100\nevery = \"1s\"")
            .expect_err("a stationary distribution cannot follow a growing keyspace");
        assert!(format!("{err:?}").contains("recency"), "{err:?}");
    }

    #[test]
    fn append_rejects_degenerate_values() {
        for (append, needle) in [
            (
                "[workload.append]\nbatch = 0\nevery = \"1s\"",
                "append.batch",
            ),
            (
                "[workload.append]\nbatch = 1\nevery = \"0s\"",
                "append.every",
            ),
            (
                "[workload.append]\nbatch = 1\nevery = \"1s\"\nconnections = 0",
                "append.connections",
            ),
            (
                "[workload.append]\nbatch = 1\nevery = \"1s\"\npipeline_depth = 0",
                "append.pipeline_depth",
            ),
        ] {
            let err = append_config("distribution = \"recency\"", append)
                .expect_err("degenerate append value must not validate");
            assert!(format!("{err:?}").contains(needle), "{needle}: {err:?}");
        }
    }

    #[test]
    fn recency_rejects_degenerate_hot_window() {
        let append = "[workload.append]\nbatch = 1\nevery = \"1s\"";
        for (ks, needle) in [
            (
                "distribution = \"recency\"\nhot_generations = 0",
                "hot_generations",
            ),
            ("distribution = \"recency\"\nhot_decay = 0.0", "hot_decay"),
            ("distribution = \"recency\"\nhot_decay = 1.5", "hot_decay"),
            (
                "distribution = \"recency\"\nhot_fraction = 1.5",
                "hot_fraction",
            ),
        ] {
            let err = append_config(ks, append).expect_err("bad hot window must not validate");
            assert!(format!("{err:?}").contains(needle), "{needle}: {err:?}");
        }
    }

    #[test]
    fn append_needs_a_protocol_with_set() {
        let err = parse_config(
            r#"
            [target]
            endpoints = ["127.0.0.1:6379"]
            protocol = "ping"
            [workload.keyspace]
            distribution = "recency"
            [workload.append]
            batch = 10
            every = "1s"
            "#,
        )
        .expect_err("ping has no SET");
        assert!(format!("{err:?}").contains("ping"), "{err:?}");
    }

    #[test]
    fn append_cluster_needs_a_writer_per_node() {
        let err = parse_config(
            r#"
            [target]
            endpoints = ["127.0.0.1:6379", "127.0.0.1:6380"]
            protocol = "resp"
            [workload.keyspace]
            distribution = "recency"
            [workload.append]
            batch = 10
            every = "1s"
            connections = 1
            "#,
        )
        .expect_err("one writer cannot drain two nodes' queues");
        assert!(format!("{err:?}").contains("append.connections"), "{err:?}");
    }

    #[test]
    fn spread_rate_is_batch_over_interval_rounded_up() {
        let a = Append {
            batch: 1000,
            every: Duration::from_secs(30),
            pace: AppendPace::Spread,
            connections: 1,
            pipeline_depth: 32,
        };
        assert_eq!(a.spread_rate(), 34);
        let slow = Append {
            batch: 1,
            every: Duration::from_secs(3600),
            ..a
        };
        assert_eq!(slow.spread_rate(), 1);
    }

    #[cfg(feature = "dma")]
    #[test]
    fn shipped_dma_configs_validate() {
        let vdma = Config::load("config/dma.toml").expect("config/dma.toml");
        assert_eq!(vdma.dma.module, DmaModule::Vdma);
        let largeobj = Config::load("config/largeobj.toml").expect("config/largeobj.toml");
        assert_eq!(largeobj.dma.module, DmaModule::LargeObj);
    }

    #[cfg(feature = "dma")]
    fn dma_config(dma: &str) -> Result<Config, ConfigError> {
        parse_config(&format!(
            r#"
            [target]
            endpoints = ["127.0.0.1:6379"]
            protocol = "dma"
            [workload.values]
            length = 4096
            [dma]
            {dma}
            "#
        ))
    }

    #[cfg(feature = "dma")]
    #[test]
    fn dma_module_defaults_to_largeobj() {
        assert_eq!(dma_config("").unwrap().dma.module, DmaModule::LargeObj);
        assert_eq!(
            dma_config("module = \"vdma\"").unwrap().dma.module,
            DmaModule::Vdma
        );
    }

    #[cfg(feature = "dma")]
    #[test]
    fn checksum_is_accepted_for_both_modules() {
        // The default module takes no write checksum but returns one with every read, so the
        // client checks reads only.
        let vdma = dma_config("module = \"vdma\"\nchecksum = true").unwrap();
        assert!(vdma.dma.checksums_writes());
        let default = dma_config("checksum = true").unwrap();
        assert!(default.dma.checksum);
        assert!(!default.dma.checksums_writes());
        assert!(
            !dma_config("module = \"vdma\"")
                .unwrap()
                .dma
                .checksums_writes()
        );
    }

    #[cfg(feature = "dma")]
    #[test]
    fn buffer_capacity_must_hold_a_value() {
        assert!(dma_config("buffer_capacity = 4096").is_ok());
        let error = dma_config("buffer_capacity = 4095").unwrap_err();
        assert!(error.to_string().contains("buffer_capacity"), "{error}");
    }
}
