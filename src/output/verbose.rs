//! Verbose tracing-style formatter.

use super::format::{format_bandwidth_bps, format_pct};
use super::{OutputFormatter, Results, Sample};
use crate::config::Config;
use std::time::Duration;

/// Verbose formatter using tracing-style output.
pub struct VerboseFormatter;

impl VerboseFormatter {
    pub fn new() -> Self {
        Self
    }
}

impl Default for VerboseFormatter {
    fn default() -> Self {
        Self::new()
    }
}

impl OutputFormatter for VerboseFormatter {
    fn print_config(&self, config: &Config) {
        tracing::info!("starting cachecannon");
        tracing::info!("endpoints: {:?}", config.target.endpoints);
        tracing::info!("protocol: {:?}", config.target.protocol);
        tracing::info!("tls: {}", config.target.tls);
        tracing::info!("threads: {}", config.general.threads);
        tracing::info!(
            "connections: {} (total)",
            config.connection.total_connections()
        );
        tracing::info!("pipeline_depth: {}", config.connection.pipeline_depth);
        tracing::info!(
            "keyspace: {} keys, {} bytes each",
            config.workload.keyspace.count,
            config.workload.keyspace.length
        );
        tracing::info!("value_size: {} bytes", config.workload.values.length);
        tracing::info!("io_engine: ringline (io_uring)");
        if let Some(ref cpu_list) = config.general.cpu_list {
            tracing::info!("cpu_list: {}", cpu_list);
        }
        if let Some(rate) = config.workload.rate_limit {
            tracing::info!("rate_limit: {} req/s", rate);
        }
    }

    fn print_warmup(&self, duration: Duration) {
        tracing::info!("warming up for {}s...", duration.as_secs());
    }

    fn print_running(&self, duration: Duration) {
        tracing::info!("warmup complete, running for {}s", duration.as_secs());
    }

    fn print_header(&self) {
        // Verbose format doesn't use a table header
    }

    fn print_sample(&self, sample: &Sample) {
        tracing::info!(
            "rate={:.0}/s err={:.0}/s hit%={} p50={:.0}us p90={:.0}us p99={:.0}us p99.9={:.0}us p99.99={:.0}us max={:.0}us",
            sample.req_per_sec,
            sample.err_per_sec,
            format_pct(sample.hit_pct),
            sample.p50_us,
            sample.p90_us,
            sample.p99_us,
            sample.p999_us,
            sample.p9999_us,
            sample.max_us,
        );
    }

    fn print_results(&self, results: &Results) {
        // Guard against division by zero
        let throughput = if results.duration_secs > 0.0 {
            results.responses as f64 / results.duration_secs
        } else {
            0.0
        };
        let err_pct = if results.responses > 0 {
            (results.errors as f64 / results.responses as f64) * 100.0
        } else {
            0.0
        };
        let hit_pct = if results.hits + results.misses > 0 {
            (results.hits as f64 / (results.hits + results.misses) as f64) * 100.0
        } else {
            0.0
        };
        let rx_bps = if results.duration_secs > 0.0 {
            (results.bytes_rx as f64 / results.duration_secs) * 8.0
        } else {
            0.0
        };
        let tx_bps = if results.duration_secs > 0.0 {
            (results.bytes_tx as f64 / results.duration_secs) * 8.0
        } else {
            0.0
        };

        tracing::info!("=== Final Results ===");
        tracing::info!("duration: {:.0}s", results.duration_secs);
        tracing::info!("requests: {}", results.requests);
        tracing::info!("responses: {}", results.responses);
        tracing::info!("errors: {} ({}%)", results.errors, format_pct(err_pct));
        tracing::info!("throughput: {:.2} req/s", throughput);
        tracing::info!(
            "bandwidth: {} RX, {} TX",
            format_bandwidth_bps(rx_bps),
            format_bandwidth_bps(tx_bps)
        );
        tracing::info!(
            "hit_rate: {}% ({} hits, {} misses)",
            format_pct(hit_pct),
            results.hits,
            results.misses
        );
        tracing::info!(
            "GET latency: p50={:.0}us p90={:.0}us p99={:.0}us p99.9={:.0}us p99.99={:.0}us max={:.0}us",
            results.get_latencies.p50_us,
            results.get_latencies.p90_us,
            results.get_latencies.p99_us,
            results.get_latencies.p999_us,
            results.get_latencies.p9999_us,
            results.get_latencies.max_us,
        );
        if results.get_ttfb.p50_us > 0.0 {
            tracing::info!(
                "GET TTFB:    p50={:.0}us p90={:.0}us p99={:.0}us p99.9={:.0}us p99.99={:.0}us max={:.0}us",
                results.get_ttfb.p50_us,
                results.get_ttfb.p90_us,
                results.get_ttfb.p99_us,
                results.get_ttfb.p999_us,
                results.get_ttfb.p9999_us,
                results.get_ttfb.max_us,
            );
        }
        tracing::info!(
            "SET latency: p50={:.0}us p90={:.0}us p99={:.0}us p99.9={:.0}us p99.99={:.0}us max={:.0}us",
            results.set_latencies.p50_us,
            results.set_latencies.p90_us,
            results.set_latencies.p99_us,
            results.set_latencies.p999_us,
            results.set_latencies.p9999_us,
            results.set_latencies.max_us,
        );
        tracing::info!(
            "connections: {} active, {} failed",
            results.conns_active,
            results.conns_failed
        );
    }
}
