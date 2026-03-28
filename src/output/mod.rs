//! Output formatting for benchmark results.
//!
//! Provides multiple output formats:
//! - Clean: Human-readable table format with colors
//! - Json: NDJSON format for machine parsing
//! - Verbose: Tracing-style output with timestamps
//! - Quiet: Minimal single-line output

mod clean;
pub mod format;
mod json;
mod quiet;
mod verbose;

pub use clean::CleanFormatter;
pub use json::JsonFormatter;
pub use quiet::QuietFormatter;
pub use verbose::VerboseFormatter;

use crate::config::{Config, Protocol};
use chrono::{DateTime, Utc};
use std::fmt;
use std::time::Duration;

/// Output format selection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OutputFormat {
    /// Clean table format with colors (default).
    #[default]
    Clean,
    /// NDJSON format for machine parsing.
    Json,
    /// Verbose tracing-style output.
    Verbose,
    /// Minimal single-line output.
    Quiet,
}

impl std::str::FromStr for OutputFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "clean" => Ok(OutputFormat::Clean),
            "json" => Ok(OutputFormat::Json),
            "verbose" => Ok(OutputFormat::Verbose),
            "quiet" => Ok(OutputFormat::Quiet),
            _ => Err(format!(
                "invalid format '{}', expected: clean, json, verbose, quiet",
                s
            )),
        }
    }
}

/// Color mode selection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ColorMode {
    /// Auto-detect based on TTY and NO_COLOR env var (default).
    #[default]
    Auto,
    /// Always use colors.
    Always,
    /// Never use colors.
    Never,
}

impl std::str::FromStr for ColorMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "auto" => Ok(ColorMode::Auto),
            "always" => Ok(ColorMode::Always),
            "never" => Ok(ColorMode::Never),
            _ => Err(format!(
                "invalid color mode '{}', expected: auto, always, never",
                s
            )),
        }
    }
}

/// A periodic sample of prefill metrics.
#[derive(Debug, Clone)]
pub struct PrefillSample {
    pub elapsed: Duration,
    pub confirmed: usize,
    pub total: usize,
    pub set_per_sec: f64,
    pub err_per_sec: f64,
    pub conns_active: i64,
    pub reconnects: u64,
}

/// A periodic sample of benchmark metrics.
#[derive(Debug, Clone)]
pub struct Sample {
    pub timestamp: DateTime<Utc>,
    pub req_per_sec: f64,
    pub err_per_sec: f64,
    pub hit_pct: f64,
    pub p50_us: f64,
    pub p90_us: f64,
    pub p99_us: f64,
    pub p999_us: f64,
    pub p9999_us: f64,
    pub max_us: f64,
}

/// Latency statistics for a specific operation type.
#[derive(Debug, Clone, Default)]
pub struct LatencyStats {
    pub p50_us: f64,
    pub p90_us: f64,
    pub p99_us: f64,
    pub p999_us: f64,
    pub p9999_us: f64,
    pub max_us: f64,
}

/// Final benchmark results.
#[derive(Debug, Clone)]
pub struct Results {
    pub duration_secs: f64,
    pub requests: u64,
    pub responses: u64,
    pub errors: u64,
    pub hits: u64,
    pub misses: u64,
    pub bytes_tx: u64,
    pub bytes_rx: u64,
    pub get_count: u64,
    pub set_count: u64,
    pub get_latencies: LatencyStats,
    pub get_ttfb: LatencyStats,
    pub set_latencies: LatencyStats,
    pub backfill_set_count: u64,
    pub backfill_set_latencies: LatencyStats,
    pub conns_active: i64,
    pub conns_failed: u64,
    pub conns_total: u64,
}

impl Results {
    /// Throughput in responses per second.
    pub fn throughput(&self) -> f64 {
        if self.duration_secs > 0.0 {
            self.responses as f64 / self.duration_secs
        } else {
            0.0
        }
    }

    /// Error percentage (0.0-100.0).
    pub fn err_pct(&self) -> f64 {
        if self.responses > 0 {
            (self.errors as f64 / self.responses as f64) * 100.0
        } else {
            0.0
        }
    }

    /// Cache hit percentage (0.0-100.0).
    pub fn hit_pct(&self) -> f64 {
        let total = self.hits + self.misses;
        if total > 0 {
            (self.hits as f64 / total as f64) * 100.0
        } else {
            0.0
        }
    }

    /// Receive bandwidth in bits per second.
    pub fn rx_bps(&self) -> f64 {
        if self.duration_secs > 0.0 {
            (self.bytes_rx as f64 / self.duration_secs) * 8.0
        } else {
            0.0
        }
    }

    /// Transmit bandwidth in bits per second.
    pub fn tx_bps(&self) -> f64 {
        if self.duration_secs > 0.0 {
            (self.bytes_tx as f64 / self.duration_secs) * 8.0
        } else {
            0.0
        }
    }
}

/// A single step in saturation search.
#[derive(Debug, Clone)]
pub struct SaturationStep {
    /// Target request rate for this step.
    pub target_rate: u64,
    /// Actual achieved request rate.
    pub achieved_rate: f64,
    /// p50 latency in microseconds.
    pub p50_us: f64,
    /// p99 latency in microseconds.
    pub p99_us: f64,
    /// p99.9 latency in microseconds.
    pub p999_us: f64,
    /// Whether SLO was met for this step.
    pub slo_passed: bool,
    /// Reason for failure (empty if passed).
    pub fail_reason: String,
    /// SLO threshold display string (e.g. "p999=1ms").
    pub slo_display: String,
    /// SLO threshold in microseconds (for percentage calculation).
    pub slo_threshold_us: Option<f64>,
}

/// Results from saturation search.
#[derive(Debug, Clone)]
pub struct SaturationResults {
    /// Maximum rate that met SLO, if any.
    pub max_compliant_rate: Option<u64>,
    /// All steps taken during the search.
    pub steps: Vec<SaturationStep>,
}

/// Reason why prefill stalled or timed out.
#[derive(Debug, Clone)]
pub enum PrefillStallCause {
    /// No connections were established.
    NoConnections,
    /// Connections established but no responses received.
    NoResponses,
    /// Progress was being made but stalled for an extended period.
    Stalled,
    /// Progress is still being made but won't finish in time.
    TooSlow {
        /// Estimated time remaining based on current rate.
        estimated_remaining: Duration,
    },
    /// Unable to determine the cause.
    Unknown,
}

impl fmt::Display for PrefillStallCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PrefillStallCause::NoConnections => write!(f, "no connections established"),
            PrefillStallCause::NoResponses => {
                write!(f, "connections established but no responses received")
            }
            PrefillStallCause::Stalled => write!(f, "progress stalled"),
            PrefillStallCause::TooSlow {
                estimated_remaining,
            } => write!(
                f,
                "too slow (estimated {:.0}s remaining)",
                estimated_remaining.as_secs_f64()
            ),
            PrefillStallCause::Unknown => write!(f, "unknown"),
        }
    }
}

/// Diagnostics collected when prefill times out or stalls.
#[derive(Debug, Clone)]
pub struct PrefillDiagnostics {
    /// Number of workers that completed prefill.
    pub workers_complete: usize,
    /// Total number of workers.
    pub workers_total: usize,
    /// Number of keys confirmed across all workers.
    pub keys_confirmed: usize,
    /// Total number of keys to prefill.
    pub keys_total: usize,
    /// Time elapsed since prefill started.
    pub elapsed: Duration,
    /// Number of active connections.
    pub conns_active: i64,
    /// Number of failed connections.
    pub conns_failed: u64,
    /// Total bytes received.
    pub bytes_rx: u64,
    /// Total requests sent.
    pub requests_sent: u64,
    /// Likely cause of the stall/timeout.
    pub likely_cause: PrefillStallCause,
}

/// Trait for output formatters.
pub trait OutputFormatter: Send + Sync {
    /// Print the configuration summary at startup.
    fn print_config(&self, config: &Config);

    /// Print the precheck phase indicator.
    fn print_precheck(&self) {
        println!("[precheck]");
    }

    /// Print precheck success.
    fn print_precheck_ok(&self, elapsed: Duration) {
        println!("[precheck ok {}ms]", elapsed.as_millis());
    }

    /// Print precheck failure.
    fn print_precheck_failed(&self, elapsed: Duration, conns_failed: u64, protocol: Protocol) {
        let _ = protocol;
        println!(
            "[precheck failed] no connectivity after {}ms ({} connection attempts failed)",
            elapsed.as_millis(),
            conns_failed
        );
    }

    /// Print the prefill phase indicator.
    fn print_prefill(&self, key_count: usize) {
        println!("PREFILL: writing {} keys...", key_count);
    }

    /// Print the warmup phase indicator.
    fn print_warmup(&self, duration: Duration);

    /// Print the running phase indicator.
    fn print_running(&self, duration: Duration);

    /// Print the table header (for formats that use one).
    fn print_header(&self);

    /// Print a periodic sample.
    fn print_sample(&self, sample: &Sample);

    /// Print the final results.
    fn print_results(&self, results: &Results);

    /// Print the prefill table header (for formats that use one).
    fn print_prefill_header(&self) {}

    /// Print a periodic prefill sample.
    fn print_prefill_sample(&self, sample: &PrefillSample) {
        let pct = if sample.total > 0 {
            (sample.confirmed as f64 / sample.total as f64) * 100.0
        } else {
            0.0
        };
        println!(
            "[prefill] {}/{} ({:.1}%) @ {:.0} SET/s  err/s={:.0}  conns={}  reconn={}",
            sample.confirmed,
            sample.total,
            pct,
            sample.set_per_sec,
            sample.err_per_sec,
            sample.conns_active,
            sample.reconnects,
        );
    }

    /// Print prefill timeout/stall diagnostics.
    fn print_prefill_timeout(&self, diag: &PrefillDiagnostics) {
        println!("PREFILL FAILED: {}", diag.likely_cause);
        println!(
            "  keys: {}/{}, workers: {}/{}, elapsed: {:.0}s",
            diag.keys_confirmed,
            diag.keys_total,
            diag.workers_complete,
            diag.workers_total,
            diag.elapsed.as_secs_f64()
        );
    }

    /// Print the saturation search header (for formats that use one).
    fn print_saturation_header(&self) {}

    /// Print a saturation search step result.
    fn print_saturation_step(&self, _step: &SaturationStep) {}

    /// Print the final saturation search results.
    fn print_saturation_results(&self, _results: &SaturationResults) {}
}

/// Create a formatter based on the output format and color mode.
pub fn create_formatter(format: OutputFormat, color: ColorMode) -> Box<dyn OutputFormatter> {
    create_formatter_with_banner(format, color, "cachecannon".to_string())
}

/// Create a formatter with a custom banner name.
pub fn create_formatter_with_banner(
    format: OutputFormat,
    color: ColorMode,
    banner: String,
) -> Box<dyn OutputFormatter> {
    match format {
        OutputFormat::Clean => Box::new(CleanFormatter::with_banner(color, banner)),
        OutputFormat::Json => Box::new(JsonFormatter::new()),
        OutputFormat::Verbose => Box::new(VerboseFormatter::new()),
        OutputFormat::Quiet => Box::new(QuietFormatter::new()),
    }
}
