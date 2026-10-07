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
    /// Injected disconnects per second across the whole run, spread over the
    /// workers in proportion to their connections. Each injected disconnect
    /// closes a connection that has requests in flight without waiting for
    /// the replies, then reconnects it, to exercise a server's cleanup of work
    /// for a connection that goes away mid-request. `0` (the default)
    /// disables it.
    #[serde(default)]
    pub disconnect_rate: f64,
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
            disconnect_rate: 0.0,
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
    /// Larger is more skewed. Must be > 0 and != 1 (the generator divides by
    /// `1 - theta`). 0.99 is the YCSB default.
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

    /// Validate config invariants that can't be expressed via serde alone.
    fn validate(&self) -> Result<(), ConfigError> {
        if self.target.endpoints.is_empty() {
            return Err(ConfigError::Validation(
                "endpoints must be specified".to_string(),
            ));
        }

        if self.general.threads == 0 {
            return Err(ConfigError::Validation("threads must be >= 1".to_string()));
        }

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

        if self.connection.total_connections() == 0 {
            return Err(ConfigError::Validation(
                "connections must be >= 1".to_string(),
            ));
        }

        let disconnect_rate = self.connection.disconnect_rate;
        if !disconnect_rate.is_finite() || disconnect_rate < 0.0 {
            return Err(ConfigError::Validation(format!(
                "connection.disconnect_rate must be a finite number >= 0 (got {disconnect_rate})"
            )));
        }
        // The ping client sends and awaits each request in one call, so there
        // is no point at which a request is in flight and the task could close
        // the connection.
        if disconnect_rate > 0.0 && self.target.protocol == Protocol::Ping {
            return Err(ConfigError::Validation(
                "connection.disconnect_rate is not supported with protocol = \"ping\"".to_string(),
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
            if !theta.is_finite() || theta <= 0.0 {
                return Err(ConfigError::Validation(format!(
                    "keyspace.zipf_theta must be finite and > 0 (got {theta})"
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
    fn disconnect_rate_defaults_off_and_parses() {
        let config = parse_config("[target]\nendpoints = [\"127.0.0.1:6379\"]\n").unwrap();
        assert_eq!(config.connection.disconnect_rate, 0.0, "unset means off");
        // An integer is accepted, as a user would write it.
        let config = parse_config(
            "[target]\nendpoints = [\"127.0.0.1:6379\"]\n[connection]\ndisconnect_rate = 50\n",
        )
        .unwrap();
        assert_eq!(config.connection.disconnect_rate, 50.0);
        let config = parse_config(
            "[target]\nendpoints = [\"127.0.0.1:6379\"]\n[connection]\ndisconnect_rate = 0.5\n",
        )
        .unwrap();
        assert_eq!(config.connection.disconnect_rate, 0.5);
    }

    #[test]
    fn disconnect_rate_rejects_negative_nan_and_inf() {
        for bad in ["-1", "nan", "inf"] {
            let err = parse_config(&format!(
                "[target]\nendpoints = [\"127.0.0.1:6379\"]\n[connection]\ndisconnect_rate = {bad}\n"
            ))
            .err()
            .unwrap_or_else(|| panic!("disconnect_rate = {bad} should be rejected"));
            assert!(
                err.to_string().contains("disconnect_rate"),
                "{bad}: error should name the key, got {err}"
            );
        }
    }

    #[test]
    fn disconnect_rate_rejected_for_ping() {
        let err = parse_config(
            "[target]\nendpoints = [\"127.0.0.1:6379\"]\nprotocol = \"ping\"\n[connection]\ndisconnect_rate = 1\n",
        )
        .expect_err("ping cannot inject disconnects");
        assert!(err.to_string().contains("ping"), "got {err}");
        // Off is fine with ping.
        parse_config("[target]\nendpoints = [\"127.0.0.1:6379\"]\nprotocol = \"ping\"\n").unwrap();
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
        for bad in ["0.0", "-0.5", "nan"] {
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
        // Skew across a sharded fleet is realistic and supported: hot keys hash
        // to particular shards and those shards see disproportionate load, which
        // is the imbalance the setup exists to measure. The worker draws from the
        // global distribution and reject-routes rather than using the uniform
        // per-endpoint fast path.
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
}
