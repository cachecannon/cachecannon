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
        if let Some(ref t) = config.workload.trace_replay {
            tracing::info!("workload: {}", super::trace_workload_summary(t));
        } else if let Some(ref kv) = config.workload.kv_replay {
            tracing::info!("workload: {}", super::kv_workload_summary(kv));
        } else {
            tracing::info!(
                "keyspace: {} keys, {} bytes each",
                config.workload.keyspace.count,
                config.workload.keyspace.length
            );
            tracing::info!("value_size: {} bytes", config.workload.values.length);
        }
        tracing::info!("io_engine: ringline ({})", crate::output::IO_ENGINE);
        // The geometry that decides whether a response lands in one buffer or
        // several, and how many can land at once per worker. Printed because it
        // is derived rather than configured by default, and because it is the
        // first thing to check when reported latency outruns the target's own.
        let (ring_size, buffer_size) = crate::runner::resolved_recv_geometry(config);
        if config.workload.kv_replay.is_some() || config.workload.trace_replay.is_some() {
            // Object sizes come from the trace, read after this.
            tracing::info!(
                "recv_buffer: {} x {} bytes per worker",
                ring_size,
                buffer_size
            );
        } else {
            let per_response = (config.workload.values.length as u32)
                .div_ceil(buffer_size)
                .max(1);
            tracing::info!(
                "recv_buffer: {} x {} bytes per worker ({} buffer{} per response)",
                ring_size,
                buffer_size,
                per_response,
                if per_response == 1 { "" } else { "s" }
            );
        }
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
        let throughput = results.throughput();
        let err_pct = results.err_pct();
        let hit_pct = results.hit_pct();
        let rx_bps = results.rx_bps();
        let tx_bps = results.tx_bps();

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
        // CO-honest sections — suppressed when there is no rate limit (slip ≡ 0)
        if results.schedule_slip.p99_us > 0.0 || results.requests_dropped > 0 {
            tracing::info!(
                "schedule_slip: p50={:.0}us p90={:.0}us p99={:.0}us p99.9={:.0}us p99.99={:.0}us max={:.0}us",
                results.schedule_slip.p50_us,
                results.schedule_slip.p90_us,
                results.schedule_slip.p99_us,
                results.schedule_slip.p999_us,
                results.schedule_slip.p9999_us,
                results.schedule_slip.max_us,
            );
            tracing::info!(
                "perceived_latency: p50={:.0}us p90={:.0}us p99={:.0}us p99.9={:.0}us p99.99={:.0}us max={:.0}us",
                results.perceived_latency.p50_us,
                results.perceived_latency.p90_us,
                results.perceived_latency.p99_us,
                results.perceived_latency.p999_us,
                results.perceived_latency.p9999_us,
                results.perceived_latency.max_us,
            );
            if results.requests_dropped > 0 {
                tracing::info!(
                    "overload: {} of {} offered dropped ({:.1}%)",
                    results.requests_dropped,
                    results.offered(),
                    results.overload_pct()
                );
            }
        }
        if let Some(ref t) = results.trace_replay {
            tracing::info!(
                "trace_replay: {} rows={}/{} failed={} skipped={} gets={} sets={} dels={} \
                 read_waits={} finished={}",
                t.trace,
                t.rows,
                t.trace_rows,
                t.rows_failed,
                t.rows_skipped,
                t.gets,
                t.sets,
                t.dels,
                t.read_waits,
                t.finished
            );
        }
        if let Some(ref kv) = results.kv_replay {
            tracing::info!(
                "kv_replay: {} speedup={} write_rule={} requests={}/{} failed={}",
                kv.timing(),
                kv.speedup,
                kv.write_rule,
                kv.requests,
                kv.trace_requests,
                kv.requests_failed
            );
            tracing::info!(
                "kv_replay tokens: cached={} ({}%) new={} input={} full_hits={} \
                 misses_at_chunk_0={} get_nil={}",
                kv.cached_tokens,
                format_pct(kv.cached_fraction() * 100.0),
                kv.new_tokens,
                kv.input_tokens,
                kv.full_hits,
                kv.chunk0_misses,
                kv.get_nil
            );
            let mut rows = vec![
                ("lookup", &kv.lookup_time),
                ("load", &kv.load_time),
                ("load_per_1k_cached_tokens", &kv.load_time_per_1k_tokens),
                ("store", &kv.store_time),
            ];
            if !kv.closed_loop {
                rows.push(("schedule_lag", &kv.schedule_lag));
            }
            for (name, s) in rows {
                tracing::info!(
                    "kv_replay {name}: p50={:.0}us p90={:.0}us p99={:.0}us p99.9={:.0}us \
                     p99.99={:.0}us max={:.0}us",
                    s.p50_us,
                    s.p90_us,
                    s.p99_us,
                    s.p999_us,
                    s.p9999_us,
                    s.max_us,
                );
            }
        }
        if results.routed_queue_dropped > 0 {
            tracing::warn!(
                "routed: {} of {} offered dropped at a full node queue (a node did not keep up)",
                results.routed_queue_dropped,
                results.offered()
            );
        }
        if results.requests_send_failed > 0 {
            tracing::warn!(
                "{} requests not sent because a send failed (usually a full send pool \
                 or zero-copy slab): some connections sent less than the pipeline depth, \
                 and latency covers only sent requests",
                results.requests_send_failed
            );
        }
        tracing::info!(
            "connections: {} active, {} failed",
            results.conns_active,
            results.conns_failed
        );
    }
}
