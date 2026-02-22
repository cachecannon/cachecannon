//! JSON (NDJSON) formatter for machine-readable output.

use super::{OutputFormatter, PrefillDiagnostics, Results, Sample};
use crate::config::Config;
use serde::Serialize;
use std::time::Duration;

/// JSON formatter outputting NDJSON (newline-delimited JSON).
pub struct JsonFormatter;

impl JsonFormatter {
    pub fn new() -> Self {
        Self
    }
}

impl Default for JsonFormatter {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Serialize)]
struct ConfigOutput {
    #[serde(rename = "type")]
    msg_type: &'static str,
    target: String,
    protocol: String,
    tls: bool,
    threads: usize,
    conns: usize,
    pipeline: usize,
    keyspace: u64,
    key_size: usize,
    value_size: usize,
    engine: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    ratelimit: Option<u64>,
    warmup_secs: u64,
    duration_secs: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    cpu_list: Option<String>,
}

#[derive(Serialize)]
struct SampleOutput {
    #[serde(rename = "type")]
    msg_type: &'static str,
    ts: String,
    req_s: u64,
    err_s: f64,
    hit_pct: f64,
    p50_us: u64,
    p90_us: u64,
    p99_us: u64,
    p999_us: u64,
    p9999_us: u64,
    max_us: u64,
}

#[derive(Serialize)]
struct LatencyOutput {
    count: u64,
    p50_us: u64,
    p90_us: u64,
    p99_us: u64,
    p999_us: u64,
    p9999_us: u64,
    max_us: u64,
}

#[derive(Serialize)]
struct ResultsOutput {
    #[serde(rename = "type")]
    msg_type: &'static str,
    duration_secs: f64,
    requests: u64,
    responses: u64,
    errors: u64,
    err_pct: f64,
    hits: u64,
    misses: u64,
    hit_pct: f64,
    throughput: u64,
    rx_bytes: u64,
    tx_bytes: u64,
    rx_bps: u64,
    tx_bps: u64,
    get: LatencyOutput,
    #[serde(skip_serializing_if = "Option::is_none")]
    get_ttfb: Option<LatencyOutput>,
    set: LatencyOutput,
    #[serde(skip_serializing_if = "Option::is_none")]
    backfill_set: Option<LatencyOutput>,
    conns_active: i64,
    conns_failed: u64,
}

impl OutputFormatter for JsonFormatter {
    fn print_config(&self, config: &Config) {
        let endpoints: Vec<_> = config
            .target
            .endpoints
            .iter()
            .map(|e| e.to_string())
            .collect();

        let output = ConfigOutput {
            msg_type: "config",
            target: endpoints.join(","),
            protocol: format!("{:?}", config.target.protocol),
            tls: config.target.tls,
            threads: config.general.threads,
            conns: config.connection.total_connections(),
            pipeline: config.connection.pipeline_depth,
            keyspace: config.workload.keyspace.count as u64,
            key_size: config.workload.keyspace.length,
            value_size: config.workload.values.length,
            engine: "io_uring".to_string(),
            ratelimit: config.workload.rate_limit,
            warmup_secs: config.general.warmup.as_secs(),
            duration_secs: config.general.duration.as_secs(),
            cpu_list: config.general.cpu_list.clone(),
        };

        if let Ok(json) = serde_json::to_string(&output) {
            println!("{}", json);
        }
    }

    fn print_warmup(&self, _duration: Duration) {
        // JSON format doesn't print warmup indicator
    }

    fn print_running(&self, _duration: Duration) {
        // JSON format doesn't print running indicator
    }

    fn print_header(&self) {
        // JSON format doesn't have a header
    }

    fn print_sample(&self, sample: &Sample) {
        let output = SampleOutput {
            msg_type: "sample",
            ts: sample.timestamp.to_rfc3339(),
            req_s: sample.req_per_sec as u64,
            err_s: sample.err_per_sec,
            hit_pct: sample.hit_pct,
            p50_us: sample.p50_us as u64,
            p90_us: sample.p90_us as u64,
            p99_us: sample.p99_us as u64,
            p999_us: sample.p999_us as u64,
            p9999_us: sample.p9999_us as u64,
            max_us: sample.max_us as u64,
        };

        if let Ok(json) = serde_json::to_string(&output) {
            println!("{}", json);
        }
    }

    fn print_prefill_progress(&self, confirmed: usize, total: usize, elapsed: std::time::Duration) {
        #[derive(Serialize)]
        struct PrefillProgress {
            #[serde(rename = "type")]
            msg_type: &'static str,
            confirmed: usize,
            total: usize,
            pct: f64,
            rate: f64,
            elapsed_secs: f64,
        }

        let pct = if total > 0 {
            (confirmed as f64 / total as f64) * 100.0
        } else {
            0.0
        };
        let secs = elapsed.as_secs_f64();
        let rate = if secs > 0.0 {
            confirmed as f64 / secs
        } else {
            0.0
        };

        let output = PrefillProgress {
            msg_type: "prefill_progress",
            confirmed,
            total,
            pct,
            rate,
            elapsed_secs: secs,
        };

        if let Ok(json) = serde_json::to_string(&output) {
            println!("{}", json);
        }
    }

    fn print_prefill_timeout(&self, diag: &PrefillDiagnostics) {
        #[derive(Serialize)]
        struct PrefillTimeout {
            #[serde(rename = "type")]
            msg_type: &'static str,
            cause: String,
            keys_confirmed: usize,
            keys_total: usize,
            workers_complete: usize,
            workers_total: usize,
            elapsed_secs: f64,
            conns_active: i64,
            conns_failed: u64,
            bytes_rx: u64,
            requests_sent: u64,
        }

        let output = PrefillTimeout {
            msg_type: "prefill_timeout",
            cause: format!("{}", diag.likely_cause),
            keys_confirmed: diag.keys_confirmed,
            keys_total: diag.keys_total,
            workers_complete: diag.workers_complete,
            workers_total: diag.workers_total,
            elapsed_secs: diag.elapsed.as_secs_f64(),
            conns_active: diag.conns_active,
            conns_failed: diag.conns_failed,
            bytes_rx: diag.bytes_rx,
            requests_sent: diag.requests_sent,
        };

        if let Ok(json) = serde_json::to_string(&output) {
            println!("{}", json);
        }
    }

    fn print_results(&self, results: &Results) {
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

        // Guard against division by zero
        let throughput = if results.duration_secs > 0.0 {
            (results.responses as f64 / results.duration_secs) as u64
        } else {
            0
        };
        let rx_bps = if results.duration_secs > 0.0 {
            ((results.bytes_rx as f64 / results.duration_secs) * 8.0) as u64
        } else {
            0
        };
        let tx_bps = if results.duration_secs > 0.0 {
            ((results.bytes_tx as f64 / results.duration_secs) * 8.0) as u64
        } else {
            0
        };

        let output = ResultsOutput {
            msg_type: "result",
            duration_secs: results.duration_secs,
            requests: results.requests,
            responses: results.responses,
            errors: results.errors,
            err_pct,
            hits: results.hits,
            misses: results.misses,
            hit_pct,
            throughput,
            rx_bytes: results.bytes_rx,
            tx_bytes: results.bytes_tx,
            rx_bps,
            tx_bps,
            get: LatencyOutput {
                count: results.get_count,
                p50_us: results.get_latencies.p50_us as u64,
                p90_us: results.get_latencies.p90_us as u64,
                p99_us: results.get_latencies.p99_us as u64,
                p999_us: results.get_latencies.p999_us as u64,
                p9999_us: results.get_latencies.p9999_us as u64,
                max_us: results.get_latencies.max_us as u64,
            },
            get_ttfb: if results.get_ttfb.p50_us > 0.0 {
                Some(LatencyOutput {
                    count: results.get_count,
                    p50_us: results.get_ttfb.p50_us as u64,
                    p90_us: results.get_ttfb.p90_us as u64,
                    p99_us: results.get_ttfb.p99_us as u64,
                    p999_us: results.get_ttfb.p999_us as u64,
                    p9999_us: results.get_ttfb.p9999_us as u64,
                    max_us: results.get_ttfb.max_us as u64,
                })
            } else {
                None
            },
            set: LatencyOutput {
                count: results.set_count,
                p50_us: results.set_latencies.p50_us as u64,
                p90_us: results.set_latencies.p90_us as u64,
                p99_us: results.set_latencies.p99_us as u64,
                p999_us: results.set_latencies.p999_us as u64,
                p9999_us: results.set_latencies.p9999_us as u64,
                max_us: results.set_latencies.max_us as u64,
            },
            backfill_set: if results.backfill_set_count > 0 {
                Some(LatencyOutput {
                    count: results.backfill_set_count,
                    p50_us: results.backfill_set_latencies.p50_us as u64,
                    p90_us: results.backfill_set_latencies.p90_us as u64,
                    p99_us: results.backfill_set_latencies.p99_us as u64,
                    p999_us: results.backfill_set_latencies.p999_us as u64,
                    p9999_us: results.backfill_set_latencies.p9999_us as u64,
                    max_us: results.backfill_set_latencies.max_us as u64,
                })
            } else {
                None
            },
            conns_active: results.conns_active,
            conns_failed: results.conns_failed,
        };

        if let Ok(json) = serde_json::to_string(&output) {
            println!("{}", json);
        }
    }
}
