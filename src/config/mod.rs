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
}

impl Default for General {
    fn default() -> Self {
        Self {
            duration: default_duration(),
            warmup: default_warmup(),
            threads: default_threads(),
            cpu_list: None,
            ringline_diag: false,
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

pub(crate) fn default_bisect_tolerance() -> f64 {
    0.05
}

pub(crate) fn default_max_bisect_steps() -> u32 {
    8
}

fn default_prefill_timeout() -> Duration {
    Duration::from_secs(300)
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
}

impl Default for Keyspace {
    fn default() -> Self {
        Self {
            length: default_key_length(),
            count: default_key_count(),
            distribution: Distribution::default(),
            zipf_theta: default_zipf_theta(),
            format: KeyFormat::default(),
        }
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

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Distribution {
    #[default]
    Uniform,
    Zipf,
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
