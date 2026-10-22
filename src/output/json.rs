//! JSON (NDJSON) formatter for machine-readable output.

use super::{
    KvReplayResults, LatencyStats, OutputFormatter, PrefillDiagnostics, PrefillSample, Results,
    Sample, TraceReplayResults,
};
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

/// `[workload.kv_replay]` in the config line.
#[derive(Serialize)]
struct KvConfigOutput {
    trace: String,
    /// 0 for closed loop.
    speedup: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    model_config: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    kv_dtype: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    kv_bytes_per_token: Option<u64>,
    mla: bool,
    write_rule: &'static str,
}

/// `[workload.trace_replay]` in the config line.
#[derive(Serialize)]
struct TraceConfigOutput {
    trace: String,
    key_prefix: String,
    key_format: &'static str,
}

/// `[workload.trace_replay]` totals in the result line.
#[derive(Serialize)]
struct TraceReplayOutput {
    trace_rows: u64,
    rows: u64,
    rows_failed: u64,
    rows_skipped: u64,
    gets: u64,
    sets: u64,
    dels: u64,
    read_waits: u64,
    finished: bool,
}

impl TraceReplayOutput {
    fn new(t: &TraceReplayResults) -> Self {
        Self {
            trace_rows: t.trace_rows,
            rows: t.rows,
            rows_failed: t.rows_failed,
            rows_skipped: t.rows_skipped,
            gets: t.gets,
            sets: t.sets,
            dels: t.dels,
            read_waits: t.read_waits,
            finished: t.finished,
        }
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
    /// The synthetic keyspace; absent for a KV or trace replay, which ignore
    /// it.
    #[serde(skip_serializing_if = "Option::is_none")]
    keyspace: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    key_size: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    value_size: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    kv_replay: Option<KvConfigOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    trace_replay: Option<TraceConfigOutput>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    append_set: Option<LatencyOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    append_batches: Option<u64>,
    conns_active: i64,
    conns_failed: u64,
    /// Requests abandoned at `connection.request_timeout` (included in errors).
    timeouts: u64,
    requests_dropped: u64,
    requests_send_failed: u64,
    routed_queue_dropped: u64,
    offered: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    schedule_slip: Option<LatencyOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    perceived_latency: Option<LatencyOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    kv_replay: Option<KvReplayOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    trace_replay: Option<TraceReplayOutput>,
}

/// `[workload.kv_replay]` totals in the result line. Times are per trace
/// request, in microseconds; `count` is the requests sampled.
#[derive(Serialize)]
struct KvReplayOutput {
    /// "open-loop" or "closed-loop".
    timing: &'static str,
    speedup: f64,
    write_rule: &'static str,
    trace_requests: u64,
    requests: u64,
    requests_failed: u64,
    input_tokens: u64,
    cached_tokens: u64,
    new_tokens: u64,
    cached_fraction: f64,
    full_hits: u64,
    misses_at_chunk_0: u64,
    no_full_chunk: u64,
    get_nil: u64,
    exists: u64,
    lookup: LatencyOutput,
    load: LatencyOutput,
    store: LatencyOutput,
    load_per_1k_cached_tokens: LatencyOutput,
    /// Open loop only.
    #[serde(skip_serializing_if = "Option::is_none")]
    schedule_lag: Option<LatencyOutput>,
}

impl LatencyOutput {
    fn from_stats(count: u64, s: &LatencyStats) -> Self {
        Self {
            count,
            p50_us: s.p50_us as u64,
            p90_us: s.p90_us as u64,
            p99_us: s.p99_us as u64,
            p999_us: s.p999_us as u64,
            p9999_us: s.p9999_us as u64,
            max_us: s.max_us as u64,
        }
    }
}

impl KvReplayOutput {
    fn new(kv: &KvReplayResults) -> Self {
        Self {
            timing: kv.timing(),
            speedup: kv.speedup,
            write_rule: kv.write_rule,
            trace_requests: kv.trace_requests,
            requests: kv.requests,
            requests_failed: kv.requests_failed,
            input_tokens: kv.input_tokens,
            cached_tokens: kv.cached_tokens,
            new_tokens: kv.new_tokens,
            cached_fraction: kv.cached_fraction(),
            full_hits: kv.full_hits,
            misses_at_chunk_0: kv.chunk0_misses,
            no_full_chunk: kv.no_full_chunk,
            get_nil: kv.get_nil,
            exists: kv.exists,
            lookup: LatencyOutput::from_stats(kv.lookups, &kv.lookup_time),
            load: LatencyOutput::from_stats(kv.loads, &kv.load_time),
            store: LatencyOutput::from_stats(kv.stores, &kv.store_time),
            load_per_1k_cached_tokens: LatencyOutput::from_stats(
                kv.loads_with_cached_tokens,
                &kv.load_time_per_1k_tokens,
            ),
            schedule_lag: (!kv.closed_loop)
                .then(|| LatencyOutput::from_stats(kv.requests, &kv.schedule_lag)),
        }
    }
}

fn config_output(config: &Config) -> ConfigOutput {
    let target = config
        .target
        .endpoints
        .iter()
        .map(|e| e.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let kv = config.workload.kv_replay.as_ref();
    let trace = config.workload.trace_replay.as_ref();
    let synthetic = kv.is_none() && trace.is_none();
    ConfigOutput {
        msg_type: "config",
        target,
        protocol: format!("{:?}", config.target.protocol),
        tls: config.target.tls,
        threads: config.general.threads,
        conns: config.connection.total_connections(),
        pipeline: config.connection.pipeline_depth,
        keyspace: synthetic.then_some(config.workload.keyspace.count as u64),
        key_size: synthetic.then_some(config.workload.keyspace.length),
        value_size: synthetic.then_some(config.workload.values.length),
        kv_replay: kv.map(|kv| KvConfigOutput {
            trace: kv.trace.display().to_string(),
            speedup: kv.speedup,
            model_config: kv.model_config.as_ref().map(|p| p.display().to_string()),
            kv_dtype: kv.kv_dtype.clone(),
            kv_bytes_per_token: kv.kv_bytes_per_token,
            mla: kv.mla,
            write_rule: kv.write_rule.as_str(),
        }),
        trace_replay: trace.map(|t| TraceConfigOutput {
            trace: t.trace.display().to_string(),
            key_prefix: t.key_prefix.clone(),
            key_format: match t.key_format {
                crate::config::KvKeyFormat::Hex => "hex",
                crate::config::KvKeyFormat::Decimal => "decimal",
            },
        }),
        engine: crate::output::IO_ENGINE.to_string(),
        ratelimit: config.workload.rate_limit,
        warmup_secs: config.general.warmup.as_secs(),
        duration_secs: config.general.duration.as_secs(),
        cpu_list: config.general.cpu_list.clone(),
    }
}

impl OutputFormatter for JsonFormatter {
    fn print_config(&self, config: &Config) {
        match serde_json::to_string(&config_output(config)) {
            Ok(json) => println!("{}", json),
            Err(e) => tracing::warn!("failed to serialize JSON output: {e}"),
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

        match serde_json::to_string(&output) {
            Ok(json) => println!("{}", json),
            Err(e) => tracing::warn!("failed to serialize JSON output: {e}"),
        }
    }

    fn print_prefill_sample(&self, sample: &PrefillSample) {
        #[derive(Serialize)]
        struct PrefillProgress {
            #[serde(rename = "type")]
            msg_type: &'static str,
            confirmed: usize,
            total: usize,
            pct: f64,
            set_per_sec: f64,
            err_per_sec: f64,
            conns_active: i64,
            reconnects: u64,
            elapsed_secs: f64,
        }

        let pct = if sample.total > 0 {
            (sample.confirmed as f64 / sample.total as f64) * 100.0
        } else {
            0.0
        };

        let output = PrefillProgress {
            msg_type: "prefill_progress",
            confirmed: sample.confirmed,
            total: sample.total,
            pct,
            set_per_sec: sample.set_per_sec,
            err_per_sec: sample.err_per_sec,
            conns_active: sample.conns_active,
            reconnects: sample.reconnects,
            elapsed_secs: sample.elapsed.as_secs_f64(),
        };

        match serde_json::to_string(&output) {
            Ok(json) => println!("{}", json),
            Err(e) => tracing::warn!("failed to serialize JSON output: {e}"),
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

        match serde_json::to_string(&output) {
            Ok(json) => println!("{}", json),
            Err(e) => tracing::warn!("failed to serialize JSON output: {e}"),
        }
    }

    fn print_results(&self, results: &Results) {
        let err_pct = results.err_pct();
        let hit_pct = results.hit_pct();
        let throughput = results.throughput() as u64;
        let rx_bps = results.rx_bps() as u64;
        let tx_bps = results.tx_bps() as u64;

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
            append_set: if results.append_set_count > 0 {
                Some(LatencyOutput {
                    count: results.append_set_count,
                    p50_us: results.append_set_latencies.p50_us as u64,
                    p90_us: results.append_set_latencies.p90_us as u64,
                    p99_us: results.append_set_latencies.p99_us as u64,
                    p999_us: results.append_set_latencies.p999_us as u64,
                    p9999_us: results.append_set_latencies.p9999_us as u64,
                    max_us: results.append_set_latencies.max_us as u64,
                })
            } else {
                None
            },
            append_batches: if results.append_set_count > 0 {
                Some(results.append_batches)
            } else {
                None
            },
            conns_active: results.conns_active,
            conns_failed: results.conns_failed,
            timeouts: results.timeouts,
            requests_dropped: results.requests_dropped,
            requests_send_failed: results.requests_send_failed,
            routed_queue_dropped: results.routed_queue_dropped,
            offered: results.offered(),
            schedule_slip: if results.schedule_slip.p99_us > 0.0 || results.requests_dropped > 0 {
                Some(LatencyOutput {
                    count: 0,
                    p50_us: results.schedule_slip.p50_us as u64,
                    p90_us: results.schedule_slip.p90_us as u64,
                    p99_us: results.schedule_slip.p99_us as u64,
                    p999_us: results.schedule_slip.p999_us as u64,
                    p9999_us: results.schedule_slip.p9999_us as u64,
                    max_us: results.schedule_slip.max_us as u64,
                })
            } else {
                None
            },
            perceived_latency: if results.schedule_slip.p99_us > 0.0 || results.requests_dropped > 0
            {
                Some(LatencyOutput {
                    count: 0,
                    p50_us: results.perceived_latency.p50_us as u64,
                    p90_us: results.perceived_latency.p90_us as u64,
                    p99_us: results.perceived_latency.p99_us as u64,
                    p999_us: results.perceived_latency.p999_us as u64,
                    p9999_us: results.perceived_latency.p9999_us as u64,
                    max_us: results.perceived_latency.max_us as u64,
                })
            } else {
                None
            },
            kv_replay: results.kv_replay.as_ref().map(KvReplayOutput::new),
            trace_replay: results.trace_replay.as_ref().map(TraceReplayOutput::new),
        };

        match serde_json::to_string(&output) {
            Ok(json) => println!("{}", json),
            Err(e) => tracing::warn!("failed to serialize JSON output: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml: &str) -> Config {
        toml::from_str(toml).unwrap()
    }

    /// A KV replay's config line describes the replay, not the synthetic
    /// keyspace it ignores; any other run keeps the keyspace fields.
    #[test]
    fn config_line_describes_kv_replay_instead_of_the_keyspace() {
        let synthetic = serde_json::to_value(config_output(&parse(
            "[target]\nendpoints = [\"127.0.0.1:6379\"]\n",
        )))
        .unwrap();
        assert!(synthetic["keyspace"].is_u64(), "{synthetic}");
        assert!(synthetic["value_size"].is_u64(), "{synthetic}");
        assert!(synthetic.get("kv_replay").is_none(), "{synthetic}");

        let kv = serde_json::to_value(config_output(&parse(
            "[target]\nendpoints = [\"127.0.0.1:6379\"]\n\
             [workload.kv_replay]\ntrace = \"r.parquet\"\nspeedup = 10.0\n\
             kv_bytes_per_token = 256\n",
        )))
        .unwrap();
        for absent in ["keyspace", "key_size", "value_size"] {
            assert!(kv.get(absent).is_none(), "{absent}: {kv}");
        }
        let r = &kv["kv_replay"];
        assert_eq!(r["trace"], "r.parquet");
        assert_eq!(r["speedup"], 10.0);
        assert_eq!(r["kv_bytes_per_token"], 256);
        assert_eq!(r["write_rule"], "set-from-miss");
        assert!(r.get("model_config").is_none(), "{kv}");
    }

    /// Each distribution's `count` is the number of samples it holds. A load
    /// whose first GET returned nil has a load time but no time per cached
    /// token.
    #[test]
    fn kv_distribution_counts_are_their_sample_counts() {
        let kv = KvReplayResults {
            closed_loop: true,
            speedup: 0.0,
            write_rule: "set-from-miss",
            trace_requests: 3,
            requests: 3,
            requests_failed: 0,
            input_tokens: 0,
            cached_tokens: 0,
            new_tokens: 0,
            full_hits: 0,
            chunk0_misses: 0,
            no_full_chunk: 0,
            get_nil: 1,
            exists: 0,
            lookups: 3,
            loads: 2,
            stores: 1,
            loads_with_cached_tokens: 1,
            lookup_time: LatencyStats::default(),
            load_time: LatencyStats::default(),
            store_time: LatencyStats::default(),
            load_time_per_1k_tokens: LatencyStats::default(),
            schedule_lag: LatencyStats::default(),
        };
        let out = KvReplayOutput::new(&kv);
        assert_eq!(
            (
                out.lookup.count,
                out.load.count,
                out.store.count,
                out.load_per_1k_cached_tokens.count
            ),
            (3, 2, 1, 1)
        );
        assert!(out.schedule_lag.is_none(), "closed loop has no lag");
    }
}
