//! Quiet formatter with minimal single-line output.

use super::format::{format_count, format_latency_us, format_pct};
use super::{OutputFormatter, Results, Sample};
use crate::config::Config;
use std::time::Duration;

/// Quiet formatter that only outputs final results on a single line.
pub struct QuietFormatter;

impl QuietFormatter {
    pub fn new() -> Self {
        Self
    }
}

impl Default for QuietFormatter {
    fn default() -> Self {
        Self::new()
    }
}

impl OutputFormatter for QuietFormatter {
    fn print_config(&self, _config: &Config) {
        // Quiet mode doesn't print config
    }

    fn print_warmup(&self, _duration: Duration) {
        // Quiet mode doesn't print warmup
    }

    fn print_running(&self, _duration: Duration) {
        // Quiet mode doesn't print running indicator
    }

    fn print_header(&self) {
        // Quiet mode doesn't print header
    }

    fn print_sample(&self, _sample: &Sample) {
        // Quiet mode doesn't print samples
    }

    fn print_results(&self, results: &Results) {
        // Guard against division by zero
        let throughput = if results.duration_secs > 0.0 {
            results.responses as f64 / results.duration_secs
        } else {
            0.0
        };
        let hit_pct = if results.hits + results.misses > 0 {
            (results.hits as f64 / (results.hits + results.misses) as f64) * 100.0
        } else {
            0.0
        };

        // Single line: throughput, hit rate, key latencies
        println!(
            "{} req/s  {}% hit  p50={} p99={} p99.9={} max={}",
            format_count(throughput as u64),
            format_pct(hit_pct),
            format_latency_us(results.get_latencies.p50_us),
            format_latency_us(results.get_latencies.p99_us),
            format_latency_us(results.get_latencies.p999_us),
            format_latency_us(results.get_latencies.max_us),
        );
    }
}
