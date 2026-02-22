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
    pub momento: Momento,
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
}

impl Default for General {
    fn default() -> Self {
        Self {
            duration: default_duration(),
            warmup: default_warmup(),
            threads: default_threads(),
            cpu_list: None,
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

fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

#[derive(Debug, Clone, Deserialize)]
pub struct Target {
    pub endpoints: Vec<SocketAddr>,
    #[serde(default)]
    pub protocol: Protocol,
    #[serde(default)]
    pub tls: bool,
    /// Enable Redis Cluster mode: discover topology via CLUSTER SLOTS and route
    /// by hash slot instead of ketama consistent hashing.
    #[serde(default)]
    pub cluster: bool,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    #[default]
    Resp,
    /// RESP3 protocol (Redis 6+)
    Resp3,
    /// Memcache ASCII text protocol
    Memcache,
    /// Memcache binary protocol
    #[serde(alias = "memcache-binary")]
    MemcacheBinary,
    Momento,
    /// Simple ASCII PING/PONG protocol
    Ping,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Connection {
    /// Total number of connections (distributed across threads).
    /// If not specified, defaults to 1 connection per endpoint.
    #[serde(default = "default_connections")]
    pub connections: usize,
    /// Legacy alias for connections (deprecated, use 'connections' instead)
    #[serde(default)]
    pub pool_size: Option<usize>,
    #[serde(default = "default_pipeline_depth")]
    pub pipeline_depth: usize,
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
    /// Get the effective number of connections, preferring 'connections' over legacy 'pool_size'.
    pub fn total_connections(&self) -> usize {
        self.pool_size.unwrap_or(self.connections)
    }
}

impl Default for Connection {
    fn default() -> Self {
        Self {
            connections: default_connections(),
            pool_size: None,
            pipeline_depth: default_pipeline_depth(),
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

fn default_stop_after_failures() -> u32 {
    3
}

fn default_max_rate() -> u64 {
    100_000_000
}

fn default_min_throughput_ratio() -> f64 {
    0.9
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
}

impl Default for Keyspace {
    fn default() -> Self {
        Self {
            length: default_key_length(),
            count: default_key_count(),
            distribution: Distribution::default(),
        }
    }
}

fn default_key_length() -> usize {
    16
}

fn default_key_count() -> usize {
    1_000_000
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Distribution {
    #[default]
    Uniform,
    Zipf,
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

/// Momento-specific configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct Momento {
    /// Cache name to use for operations.
    #[serde(default = "default_cache_name")]
    pub cache_name: String,
    /// Explicit endpoint (overrides MOMENTO_ENDPOINT env var).
    #[serde(default)]
    pub endpoint: Option<String>,
    /// Default TTL for SET operations in seconds.
    #[serde(default = "default_momento_ttl")]
    pub ttl_seconds: u64,
}

impl Default for Momento {
    fn default() -> Self {
        Self {
            cache_name: default_cache_name(),
            endpoint: None,
            ttl_seconds: default_momento_ttl(),
        }
    }
}

fn default_cache_name() -> String {
    "default-cache".to_string()
}

fn default_momento_ttl() -> u64 {
    3600 // 1 hour
}

impl Config {
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self, ConfigError> {
        let content =
            std::fs::read_to_string(path.as_ref()).map_err(|e| ConfigError::Io(e.to_string()))?;
        toml::from_str(&content).map_err(|e| ConfigError::Parse(e.to_string()))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config: {0}")]
    Io(String),
    #[error("failed to parse config: {0}")]
    Parse(String),
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
        // Simple parser for durations like "60s", "10m", "1h"
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
