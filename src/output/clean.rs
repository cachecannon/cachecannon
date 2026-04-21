//! Clean table formatter with optional color output.

use super::format::{
    format_bandwidth_bps, format_count, format_latency_padded, format_pct, format_rate_padded,
};
use super::{
    ColorMode, LatencyStats, OutputFormatter, PrefillDiagnostics, PrefillSample, PrefillStallCause,
    Results, Sample, SaturationResults, SaturationStep,
};
use crate::config::{Config, Protocol};
use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Number of sample rows between header reprints.
const HEADER_REPEAT_INTERVAL: u64 = 25;

/// ANSI escape codes for colors.
mod ansi {
    pub const RED: &str = "\x1b[31m";
    pub const GREEN: &str = "\x1b[32m";
    pub const YELLOW: &str = "\x1b[33m";
    pub const CYAN: &str = "\x1b[36m";
    pub const DIM: &str = "\x1b[2m";
    pub const BOLD: &str = "\x1b[1m";
    pub const RESET: &str = "\x1b[0m";
}

/// Clean table formatter with optional color support.
pub struct CleanFormatter {
    use_color: bool,
    sample_count: AtomicU64,
    prefill_sample_count: AtomicU64,
    results_step_count: AtomicU64,
    banner: String,
}

impl CleanFormatter {
    pub fn new(color_mode: ColorMode) -> Self {
        Self::with_banner(color_mode, "cachecannon".to_string())
    }

    pub fn with_banner(color_mode: ColorMode, banner: String) -> Self {
        let use_color = match color_mode {
            ColorMode::Always => true,
            ColorMode::Never => false,
            ColorMode::Auto => {
                // Check if stdout is a TTY and NO_COLOR is not set
                io::stdout().is_terminal() && std::env::var("NO_COLOR").is_err()
            }
        };
        Self {
            use_color,
            sample_count: AtomicU64::new(0),
            prefill_sample_count: AtomicU64::new(0),
            results_step_count: AtomicU64::new(0),
            banner,
        }
    }

    fn red(&self, s: &str) -> String {
        if self.use_color {
            format!("{}{}{}", ansi::RED, s, ansi::RESET)
        } else {
            s.to_string()
        }
    }

    fn green(&self, s: &str) -> String {
        if self.use_color {
            format!("{}{}{}", ansi::GREEN, s, ansi::RESET)
        } else {
            s.to_string()
        }
    }

    fn yellow(&self, s: &str) -> String {
        if self.use_color {
            format!("{}{}{}", ansi::YELLOW, s, ansi::RESET)
        } else {
            s.to_string()
        }
    }

    fn dim(&self, s: &str) -> String {
        if self.use_color {
            format!("{}{}{}", ansi::DIM, s, ansi::RESET)
        } else {
            s.to_string()
        }
    }

    fn bold(&self, s: &str) -> String {
        if self.use_color {
            format!("{}{}{}", ansi::BOLD, s, ansi::RESET)
        } else {
            s.to_string()
        }
    }

    fn cyan(&self, s: &str) -> String {
        if self.use_color {
            format!("{}{}{}", ansi::CYAN, s, ansi::RESET)
        } else {
            s.to_string()
        }
    }

    fn maybe_red(&self, s: &str, condition: bool) -> String {
        if condition {
            self.red(s)
        } else {
            s.to_string()
        }
    }
}

impl OutputFormatter for CleanFormatter {
    fn print_config(&self, config: &Config) {
        println!("{}", self.bold(&self.cyan(&self.banner)));
        println!("{}", self.dim("──────────────────"));

        // Target line
        let protocol = format!("{:?}", config.target.protocol);
        let tls_suffix = if config.target.tls { ", TLS" } else { "" };
        if config.target.protocol == Protocol::Momento {
            let endpoint_display = crate::client::MomentoSetup::resolve_endpoint_display(config)
                .unwrap_or_else(|| "<MOMENTO_API_KEY not set>".to_string());
            println!(
                "{}     {} ({})",
                self.cyan("target"),
                endpoint_display,
                protocol
            );
        } else {
            let endpoints: Vec<_> = config
                .target
                .endpoints
                .iter()
                .map(|e| e.to_string())
                .collect();
            println!(
                "{}     {}{}:{} ({}{})",
                self.cyan("target"),
                config
                    .target
                    .endpoints
                    .first()
                    .map(|e| e.ip())
                    .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
                if endpoints.len() > 1 {
                    format!(" (+{})", endpoints.len() - 1)
                } else {
                    String::new()
                },
                config
                    .target
                    .endpoints
                    .first()
                    .map(|e| e.port())
                    .unwrap_or(6379),
                protocol,
                tls_suffix
            );
        }

        // Workload line
        let keyspace_count = format_count(config.workload.keyspace.count as u64);
        println!(
            "{}   {} keys, {}B key, {}B value, {}:{} GET:SET",
            self.cyan("workload"),
            keyspace_count,
            config.workload.keyspace.length,
            config.workload.values.length,
            config.workload.commands.get,
            config.workload.commands.set
        );

        // Threads line
        let threads_str = if let Some(ref cpu_list) = config.general.cpu_list {
            format!("{}, pinned {}", config.general.threads, cpu_list)
        } else {
            format!("{}", config.general.threads)
        };
        println!("{}    {}", self.cyan("threads"), threads_str);

        // Connections line
        println!(
            "{}      {}, pipeline {}",
            self.cyan("conns"),
            config.connection.total_connections(),
            config.connection.pipeline_depth
        );

        // Rate limit line (optional)
        if let Some(rate) = config.workload.rate_limit {
            println!("{}  {} req/s", self.cyan("ratelimit"), format_count(rate));
        }

        println!();
    }

    fn print_precheck(&self) {
        println!("[precheck]");
        let _ = io::stdout().flush();
    }

    fn print_precheck_ok(&self, elapsed: Duration) {
        println!("[precheck ok {}ms]", elapsed.as_millis());
        let _ = io::stdout().flush();
    }

    fn print_precheck_failed(&self, elapsed: Duration, conns_failed: u64, protocol: Protocol) {
        println!();
        println!(
            "{}",
            self.red("────────────────────────────────────────────────────────────────────────")
        );
        println!("{}", self.red("PRECHECK FAILED"));
        println!(
            "{}",
            self.red("────────────────────────────────────────────────────────────────────────")
        );
        println!(
            "{}",
            self.red(&format!(
                "no connectivity after {}ms ({} connection attempts failed)",
                elapsed.as_millis(),
                conns_failed
            ))
        );
        println!();
        if protocol == Protocol::Momento {
            println!("hint: check MOMENTO_API_KEY environment variable");
            println!("hint: check momento.endpoint or MOMENTO_ENDPOINT configuration");
        } else {
            println!("hint: check that the server is running and reachable");
            println!("hint: check firewall rules and target address in config");
        }
        let _ = io::stdout().flush();
    }

    fn print_prefill(&self, key_count: usize) {
        use super::format::format_count;
        println!("[prefill {} keys]", format_count(key_count as u64));
        println!();
    }

    fn print_warmup(&self, duration: Duration) {
        println!("[warmup {}s]", duration.as_secs());
    }

    fn print_running(&self, duration: Duration) {
        println!("[running {}s]", duration.as_secs());
        println!();
    }

    fn print_header(&self) {
        println!(
            "{}",
            self.cyan(
                "time UTC  req/s      p50      p90      p99     p999    p9999      max  err/s  hit%"
            )
        );
        println!(
            "{}",
            self.dim(
                "────────  ─────  ───────  ───────  ───────  ───────  ───────  ───────  ─────  ────"
            )
        );
        let _ = io::stdout().flush();
    }

    fn print_sample(&self, sample: &Sample) {
        // Reprint header periodically for readability
        let count = self.sample_count.fetch_add(1, Ordering::Relaxed);
        if count > 0 && count.is_multiple_of(HEADER_REPEAT_INTERVAL) {
            println!();
            self.print_header();
        }

        let time = sample.timestamp.format("%H:%M:%S");
        let rate = self.bold(&format_rate_padded(sample.req_per_sec, 5));
        let err = format_rate_padded(sample.err_per_sec, 5);
        let hit = format_pct(sample.hit_pct);

        // Color err/s red if > 0, otherwise dim
        let err_display = if sample.err_per_sec > 0.0 {
            self.red(&err)
        } else {
            self.dim(&err)
        };

        let p50 = self.dim(&format_latency_padded(sample.p50_us, 7));
        let p90 = self.dim(&format_latency_padded(sample.p90_us, 7));
        let p99 = self.dim(&format_latency_padded(sample.p99_us, 7));
        let p999 = self.bold(&format_latency_padded(sample.p999_us, 7));
        let p9999 = self.dim(&format_latency_padded(sample.p9999_us, 7));
        let max = self.dim(&format_latency_padded(sample.max_us, 7));

        println!(
            "{}  {}  {}  {}  {}  {}  {}  {}  {}  {:>4}",
            self.dim(&format!("{}", time)),
            rate,
            p50,
            p90,
            p99,
            p999,
            p9999,
            max,
            err_display,
            self.dim(&hit)
        );
        let _ = io::stdout().flush();
    }

    fn print_results(&self, results: &Results) {
        println!();
        println!(
            "{}",
            self.dim("─────────────────────────────────────────────────────────────────────────────────────")
        );
        println!(
            "{}",
            self.bold(&format!("RESULTS ({:.0}s)", results.duration_secs))
        );
        println!(
            "{}",
            self.dim("─────────────────────────────────────────────────────────────────────────────────────")
        );

        // Throughput line
        let throughput = results.throughput();
        let err_pct = results.err_pct();
        let err_str = format!("{}% errors", format_pct(err_pct));
        let err_colored = self.maybe_red(&err_str, err_pct > 0.0);
        println!(
            "{}   {} req/s, {}",
            self.cyan("throughput"),
            format_count(throughput as u64),
            err_colored
        );

        // Bandwidth line
        if results.bytes_rx > 0 || results.bytes_tx > 0 {
            println!(
                "{}    {} RX, {} TX",
                self.cyan("bandwidth"),
                format_bandwidth_bps(results.rx_bps()),
                format_bandwidth_bps(results.tx_bps())
            );
        }

        println!();

        // Hit rate line
        let hit_pct = results.hit_pct();
        println!(
            "{}     {}% ({} hit, {} miss)",
            self.cyan("hit rate"),
            format_pct(hit_pct),
            format_count(results.hits),
            format_count(results.misses)
        );

        println!();

        // Latency table - pad labels before coloring to avoid ANSI alignment issues
        let pad_name = |name: &str| -> String { format!("{:<10}", name) };
        let rpad = |s: &str| -> String { format!("{:>8}", s) };
        println!(
            "{}  {}  {}  {}  {}  {}  {}",
            self.cyan(&pad_name("latency")),
            self.cyan(&rpad("p50")),
            self.cyan(&rpad("p90")),
            self.cyan(&rpad("p99")),
            self.cyan(&rpad("p999")),
            self.cyan(&rpad("p9999")),
            self.cyan(&rpad("max"))
        );

        let format_latency_row = |label: &str, stats: &LatencyStats| -> String {
            format!(
                "{}  {:>8}  {:>8}  {:>8}  {:>8}  {:>8}  {:>8}",
                label,
                format_latency_padded(stats.p50_us, 8),
                format_latency_padded(stats.p90_us, 8),
                format_latency_padded(stats.p99_us, 8),
                format_latency_padded(stats.p999_us, 8),
                format_latency_padded(stats.p9999_us, 8),
                format_latency_padded(stats.max_us, 8),
            )
        };

        if results.get_count > 0 {
            println!(
                "{}",
                format_latency_row(&self.cyan(&pad_name("GET")), &results.get_latencies)
            );
            if results.get_ttfb.p50_us > 0.0 {
                println!(
                    "{}",
                    format_latency_row(&self.cyan(&pad_name("GET TTFB")), &results.get_ttfb)
                );
            }
        }
        if results.set_count > 0 {
            println!(
                "{}",
                format_latency_row(&self.cyan(&pad_name("SET")), &results.set_latencies)
            );
            if results.backfill_set_count > 0 {
                println!(
                    "{}",
                    format_latency_row(
                        &self.cyan(&pad_name("  backfill")),
                        &results.backfill_set_latencies
                    )
                );
            }
        }

        println!();

        // Connections line
        let conn_str = if results.conns_failed > 0 {
            self.red(&format!(
                "{} active, {} failed",
                results.conns_active, results.conns_failed
            ))
        } else {
            format!("{} active, 0 failed", results.conns_active)
        };
        println!("{}  {}", self.cyan("connections"), conn_str);
    }

    fn print_prefill_header(&self) {
        println!(
            "{}",
            self.cyan("elapsed    set/s   err/s  conns  reconn  progress")
        );
        println!(
            "{}",
            self.dim("───────  ───────  ──────  ─────  ──────  ────────────────")
        );
        let _ = io::stdout().flush();
    }

    fn print_prefill_sample(&self, sample: &PrefillSample) {
        use super::format::{format_count, format_pct, format_rate_padded};

        let count = self.prefill_sample_count.fetch_add(1, Ordering::Relaxed);
        if count > 0 && count.is_multiple_of(HEADER_REPEAT_INTERVAL) {
            println!();
            self.print_prefill_header();
        }

        let secs = sample.elapsed.as_secs();
        let elapsed = if secs < 60 {
            format!("{:>6}s", secs)
        } else {
            format!("{:>3}m{:02}s", secs / 60, secs % 60)
        };

        let set_rate = self.bold(&format_rate_padded(sample.set_per_sec, 7));

        let err_str = format_rate_padded(sample.err_per_sec, 6);
        let err_display = if sample.err_per_sec > 0.0 {
            self.red(&err_str)
        } else {
            self.dim(&err_str)
        };

        let conns_str = format!("{:>5}", sample.conns_active);

        let reconn_str = format!("{:>6}", sample.reconnects);
        let reconn_display = if sample.reconnects > 0 {
            self.yellow(&reconn_str)
        } else {
            self.dim(&reconn_str)
        };

        let pct = if sample.total > 0 {
            (sample.confirmed as f64 / sample.total as f64) * 100.0
        } else {
            0.0
        };
        let progress = format!(
            "{}/{}  {:>5}%",
            format_count(sample.confirmed as u64),
            format_count(sample.total as u64),
            format_pct(pct),
        );

        println!(
            "{}  {}  {}  {}  {}  {}",
            self.dim(&elapsed),
            set_rate,
            err_display,
            conns_str,
            reconn_display,
            self.dim(&progress),
        );
        let _ = io::stdout().flush();
    }

    fn print_prefill_timeout(&self, diag: &PrefillDiagnostics) {
        use super::format::format_count;

        println!();
        println!(
            "{}",
            self.red("────────────────────────────────────────────────────────────────────────")
        );
        println!("{}", self.red("PREFILL TIMEOUT"));
        println!(
            "{}",
            self.red("────────────────────────────────────────────────────────────────────────")
        );

        let cause_str = match &diag.likely_cause {
            PrefillStallCause::NoConnections => "no connections established",
            PrefillStallCause::NoResponses => "connections up but no responses received",
            PrefillStallCause::Stalled => "progress stalled (no new confirmations for 30s)",
            PrefillStallCause::TooSlow { .. } => "progressing too slowly to finish in time",
            PrefillStallCause::Unknown => "unknown",
        };
        println!("cause        {}", self.red(cause_str));
        println!(
            "progress     {}/{} keys ({:.1}%)",
            format_count(diag.keys_confirmed as u64),
            format_count(diag.keys_total as u64),
            if diag.keys_total > 0 {
                (diag.keys_confirmed as f64 / diag.keys_total as f64) * 100.0
            } else {
                0.0
            }
        );
        println!(
            "workers      {}/{} complete",
            diag.workers_complete, diag.workers_total,
        );
        println!("elapsed      {:.0}s", diag.elapsed.as_secs_f64());
        println!(
            "connections  {} active, {} failed",
            diag.conns_active, diag.conns_failed,
        );
        println!(
            "requests     {} sent, {} bytes rx",
            format_count(diag.requests_sent),
            format_count(diag.bytes_rx),
        );

        println!();
        match &diag.likely_cause {
            PrefillStallCause::NoConnections => {
                println!("hint: check that the server is running and reachable");
                println!("hint: check firewall rules and target address in config");
            }
            PrefillStallCause::NoResponses => {
                println!("hint: server may be overloaded or not processing commands");
                println!("hint: check server logs for errors");
            }
            PrefillStallCause::Stalled => {
                println!("hint: server may have stopped responding");
                println!("hint: check server logs and connection state");
            }
            PrefillStallCause::TooSlow {
                estimated_remaining,
            } => {
                println!(
                    "hint: estimated {:.0}s remaining at current rate",
                    estimated_remaining.as_secs_f64()
                );
                println!("hint: increase prefill_timeout or reduce keyspace size");
            }
            PrefillStallCause::Unknown => {
                println!("hint: check server and network connectivity");
            }
        }
        let _ = io::stdout().flush();
    }

    fn print_saturation_header(&self) {
        // No separate header - each step is self-describing
    }

    fn print_saturation_step(&self, step: &SaturationStep) {
        use super::format::{format_latency_us, format_rate};

        let step_num = self.results_step_count.fetch_add(1, Ordering::Relaxed) + 1;

        let target = format_rate(step.target_rate as f64);
        let achieved_str = format_rate(step.achieved_rate);
        let slo_p_str = format_latency_us(step.slo_percentile_us);

        let bar = self.dim("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");

        // Build verdict header with cause
        let is_throughput_fail = !step.slo_passed && step.fail_reason.starts_with("Throughput");
        let is_latency_fail = !step.slo_passed && step.fail_reason.starts_with("Latency");

        let verdict = if step.slo_passed {
            self.green(&format!("STEP {} \u{2014} PASS", step_num))
        } else if is_throughput_fail {
            self.red(&format!(
                "STEP {} \u{2014} FAIL \u{2014} Throughput Limited",
                step_num
            ))
        } else if is_latency_fail {
            self.red(&format!(
                "STEP {} \u{2014} FAIL \u{2014} Latency Exceeded",
                step_num
            ))
        } else {
            self.red(&format!("STEP {} \u{2014} FAIL", step_num))
        };

        let slo_display = if step.slo_display.is_empty() {
            String::new()
        } else {
            format!(" @ {}", step.slo_display)
        };

        println!();
        println!("{}", bar);
        println!("{}", verdict);
        println!();
        println!("SLO:    {}{}", target, slo_display);
        println!(
            "Result: {} @ {}={}",
            self.bold(&achieved_str),
            step.slo_percentile_label,
            self.bold(&slo_p_str)
        );

        // Fail reason: yellow for throughput, red for latency
        if !step.slo_passed && !step.fail_reason.is_empty() && is_throughput_fail {
            println!("{}", self.yellow(&step.fail_reason));
        } else if !step.slo_passed && !step.fail_reason.is_empty() {
            println!("{}", self.red(&step.fail_reason));
        }

        // Headroom on PASS
        if step.slo_passed
            && let Some(threshold_us) = step.slo_threshold_us
            && threshold_us > 0.0
        {
            let headroom = ((threshold_us - step.slo_percentile_us) / threshold_us) * 100.0;
            println!(
                "{}",
                self.dim(&format!("Headroom: {:.0}%", headroom.max(0.0)))
            );
        }

        println!("{}", bar);
        println!();

        // Reset sample count and reprint header for the next step's rows
        self.sample_count.store(0, Ordering::Relaxed);
        self.print_header();
        let _ = io::stdout().flush();
    }

    fn print_saturation_results(&self, results: &SaturationResults) {
        println!();
        println!(
            "{}",
            self.dim("────────────────────────────────────────────────────────")
        );

        match results.max_compliant_rate {
            Some(rate) => {
                println!(
                    "{}",
                    self.green(&format!(
                        "MAX COMPLIANT THROUGHPUT: {} req/s",
                        format_count(rate)
                    ))
                );
            }
            None => {
                println!(
                    "{}",
                    self.red("MAX COMPLIANT THROUGHPUT: none (SLO never met)")
                );
            }
        }
    }
}
