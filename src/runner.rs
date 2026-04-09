use crate::config::{Config, Protocol as CacheProtocol, TimestampMode};
use crate::metrics;
use crate::output::{PrefillDiagnostics, PrefillSample, PrefillStallCause};
use crate::saturation::SaturationSearchState;
use crate::worker::{BenchWorkerConfig, Phase, init_config_channel};
use crate::{
    AdminServer, LatencyStats, OutputFormatter, Results, Sample, SharedState, create_formatter,
    parse_cpu_list,
};
use ratelimit::Ratelimiter;

use chrono::Utc;
use metriken::{AtomicHistogram, histogram::Histogram};
use rand::RngCore;
use rand_xoshiro::Xoshiro256PlusPlus;
use rand_xoshiro::rand_core::SeedableRng;
use ringline::RinglineBuilder;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Timeout for waiting on worker threads to finish during shutdown.
const WORKER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Size of the shared random-byte value pool (1 GiB).
///
/// Workers pick random offsets into this pool for SET values.  The configured
/// `workload.values.length` must not exceed this size.
pub const VALUE_POOL_SIZE: usize = 1024 * 1024 * 1024;

/// Convenience entry point that takes only a [`Config`].
///
/// Parses `cpu_list`, creates the output formatter, installs a Ctrl-C handler,
/// prints the config summary, and delegates to [`run_benchmark_full`].
pub fn run_benchmark(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    let cpu_ids = if let Some(ref cpu_list) = config.general.cpu_list {
        match parse_cpu_list(cpu_list) {
            Ok(ids) => Some(ids),
            Err(e) => {
                tracing::error!("invalid cpu_list '{}': {}", cpu_list, e);
                return Err(e.into());
            }
        }
    } else {
        None
    };

    let formatter = create_formatter(config.admin.format, config.admin.color);
    formatter.print_config(&config);

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    })
    .expect("Error setting signal handler");

    run_benchmark_full(config, cpu_ids, formatter, running)
}

/// Run the full benchmark with the given configuration and pre-built
/// components.
///
/// This is the shared core used by both the `cachecannon` and `valkey-lab`
/// binaries.  It handles cluster discovery, worker spawning, the
/// prefill/warmup/run lifecycle, periodic reporting, saturation search, and
/// final results output.
pub fn run_benchmark_full(
    mut config: Config,
    cpu_ids: Option<Vec<usize>>,
    formatter: Box<dyn OutputFormatter>,
    running: Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Cluster mode: discover topology and replace endpoints with primaries
    let slot_table = if config.target.cluster {
        match config.target.protocol {
            CacheProtocol::Resp | CacheProtocol::Resp3 => {}
            other => {
                return Err(format!("cluster mode requires resp protocol, got {:?}", other).into());
            }
        }
        let (endpoints, table) = crate::cluster::discover_topology(&config.target.endpoints)?;
        config.target.endpoints = endpoints;
        Some(table)
    } else {
        None
    };

    let num_threads = config.general.threads;
    let warmup = config.general.warmup;
    let duration = config.general.duration;
    let total_connections = config.connection.total_connections();

    // Shared state
    let shared = Arc::new(SharedState::new());

    // Create shared rate limiter
    let initial_rate = if let Some(ref sat) = config.workload.saturation_search {
        sat.start_rate
    } else {
        config.workload.rate_limit.unwrap_or(0)
    };

    let ratelimiter = if initial_rate > 0 || config.workload.saturation_search.is_some() {
        Some(Arc::new(
            Ratelimiter::builder(initial_rate)
                .initial_available(initial_rate)
                .build()
                .expect("failed to build ratelimiter"),
        ))
    } else {
        None
    };

    // Start admin server if configured
    let _admin_handle = if config.admin.listen.is_some() || config.admin.parquet.is_some() {
        let admin = AdminServer::new(
            config.admin.listen,
            config.admin.parquet.clone(),
            config.admin.parquet_interval,
            Arc::clone(&shared),
        );
        Some(admin.run())
    } else {
        None
    };

    // Calculate prefill ranges for each worker.
    // Only distribute keys to workers that will have at least one connection,
    // using the same distribution formula as worker.rs create_for_worker().
    let prefill_enabled = config.workload.prefill;
    let key_count = config.workload.keyspace.count;
    let prefill_ranges: Vec<Option<std::ops::Range<usize>>> = if prefill_enabled {
        let base_conns = total_connections / num_threads;
        let conn_remainder = total_connections % num_threads;
        let workers_with_conns = if base_conns > 0 {
            num_threads
        } else {
            conn_remainder.min(num_threads)
        };

        let keys_per_worker = key_count / workers_with_conns;
        let key_remainder = key_count % workers_with_conns;

        // Track which effective worker index we're at (among workers with conns)
        let mut effective_id = 0usize;
        (0..num_threads)
            .map(|id| {
                let has_conns = id < conn_remainder || base_conns > 0;
                if !has_conns {
                    return None;
                }
                let eid = effective_id;
                effective_id += 1;
                let start = if eid < key_remainder {
                    eid * (keys_per_worker + 1)
                } else {
                    key_remainder * (keys_per_worker + 1) + (eid - key_remainder) * keys_per_worker
                };
                let count = if eid < key_remainder {
                    keys_per_worker + 1
                } else {
                    keys_per_worker
                };
                Some(start..start + count)
            })
            .collect()
    } else {
        vec![None; num_threads]
    };

    // Allocate shared value pool: 1GB of random bytes shared across all workers.
    // Workers pick random offsets into this pool for SET values, avoiding per-worker
    // copies. The pool is seeded deterministically for reproducibility.
    let value_pool = {
        let mut pool = vec![0u8; VALUE_POOL_SIZE];
        let mut rng = Xoshiro256PlusPlus::seed_from_u64(0xdeadbeef);
        // Fill in 8KB chunks for efficiency
        const CHUNK: usize = 8192;
        for chunk in pool.chunks_mut(CHUNK) {
            rng.fill_bytes(chunk);
        }
        Arc::new(pool)
    };

    // Set up config channel for ringline workers
    let (config_tx, config_rx) = crossbeam_channel::bounded::<BenchWorkerConfig>(num_threads);
    #[allow(clippy::needless_range_loop)]
    for id in 0..num_threads {
        config_tx
            .send(BenchWorkerConfig {
                id,
                config: config.clone(),
                shared: Arc::clone(&shared),
                ratelimiter: ratelimiter.clone(),
                recording: false,
                prefill_range: prefill_ranges[id].clone(),
                cpu_ids: cpu_ids.clone(),
                value_pool: Arc::clone(&value_pool),
                slot_table: slot_table.clone(),
            })
            .expect("failed to queue worker config");
    }
    init_config_channel(config_rx);

    // Build ringline config (client-only, no bind).
    // With guard-based sends for SET values, the copy pool only holds small
    // protocol framing data, so the default 16KB slot size is sufficient.
    // Momento always needs TLS (ringline-momento::Client::connect() uses ringline::connect_tls)
    let needs_tls = config.target.tls || config.target.protocol == CacheProtocol::Momento;
    let tls_client = if needs_tls {
        let mut root_store = rustls::RootCertStore::empty();
        root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

        let tls_config = if config.target.tls_verify {
            rustls::ClientConfig::builder()
                .with_root_certificates(root_store)
                .with_no_client_auth()
        } else {
            rustls::ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(std::sync::Arc::new(NoCertificateVerification))
                .with_no_client_auth()
        };

        Some(ringline::TlsClientConfig {
            client_config: std::sync::Arc::new(tls_config),
        })
    } else {
        None
    };

    let ringline_config = ringline::Config {
        worker: ringline::WorkerConfig {
            threads: num_threads,
            pin_to_core: false, // We pin in create_for_worker instead
            core_offset: 0,
        },
        tcp_nodelay: true,
        tls_client,
        timestamps: matches!(config.timestamps.mode, TimestampMode::Software),
        ..Default::default()
    };

    // Launch ringline workers (client-only, no bind)
    tracing::debug!(num_threads, "launching ringline workers");
    let (shutdown_handle, handles) =
        RinglineBuilder::new(ringline_config).launch::<crate::worker::BenchHandler>()?;
    tracing::debug!(workers = handles.len(), "ringline workers launched");

    // Start in precheck phase
    shared.set_phase(Phase::Precheck);
    formatter.print_precheck();

    // Early liveness check: give workers time to complete EventLoop::new(),
    // then verify at least one is still alive. This catches setup failures
    // (e.g., RLIMIT_NOFILE too low) before entering the reporting loop.
    std::thread::sleep(Duration::from_millis(200));
    if handles.iter().all(|h| h.is_finished()) {
        shutdown_handle.shutdown();
        let mut errors = Vec::new();
        for (i, handle) in handles.into_iter().enumerate() {
            match handle.join() {
                Ok(Err(e)) => errors.push(format!("worker {i}: {e}")),
                Err(e) => errors.push(format!("worker {i} panicked: {e:?}")),
                Ok(Ok(())) => {}
            }
        }
        if errors.is_empty() {
            return Err("all worker threads exited immediately with no error".into());
        }
        return Err(format!(
            "all workers failed during startup:\n  {}",
            errors.join("\n  ")
        )
        .into());
    }

    // Main thread: reporting loop
    let start = Instant::now();
    let report_interval = Duration::from_secs(1);
    let mut last_report = Instant::now();
    let mut last_responses = 0u64;
    let mut last_errors = 0u64;
    let mut last_conn_failures = 0u64;
    let mut last_hits = 0u64;
    let mut last_misses = 0u64;
    let mut last_histogram: Option<Histogram> = None;
    let mut baseline_bytes_tx = 0u64;
    let mut baseline_bytes_rx = 0u64;
    let mut baseline_requests = 0u64;
    let mut baseline_responses = 0u64;
    let mut baseline_errors = 0u64;
    let mut baseline_hits = 0u64;
    let mut baseline_misses = 0u64;
    let mut baseline_get_count = 0u64;
    let mut baseline_set_count = 0u64;
    let mut baseline_backfill_set_count = 0u64;
    let mut baseline_get_latency: Option<Histogram> = None;
    let mut baseline_get_ttfb: Option<Histogram> = None;
    let mut baseline_set_latency: Option<Histogram> = None;
    let mut baseline_backfill_set_latency: Option<Histogram> = None;
    let mut current_phase = Phase::Precheck;

    let mut actual_duration = duration;

    // Saturation search state (initialized after warmup if configured)
    let mut saturation_state: Option<SaturationSearchState> = None;

    // Track when warmup actually starts (after prefill completes)
    let mut warmup_start: Option<Instant> = None;
    // Track when the running phase started (for saturation search duration)
    let mut running_start: Option<Instant> = None;

    // Precheck tracking.
    // The precheck timer does not start until all workers have finished
    // initializing their event loops (io_uring setup, on_start).  This
    // prevents false timeouts on loaded systems where worker startup
    // takes longer than the connect_timeout window.
    let mut precheck_start: Option<Instant> = None;
    let precheck_timeout = config.connection.connect_timeout;

    // Prefill progress tracking
    let mut prefill_start = Instant::now();
    let mut last_prefill_confirmed: usize = 0;
    let mut last_prefill_progress_time = Instant::now();
    let mut last_prefill_progress_report = Instant::now();
    let prefill_timeout = config.workload.prefill_timeout;
    let prefill_stall_threshold = Duration::from_secs(30);
    let prefill_progress_interval = Duration::from_secs(1);
    let mut prefill_timeout_diag: Option<PrefillDiagnostics> = None;
    // Delta tracking for prefill sample output
    let mut last_prefill_confirmed_snapshot: usize = 0;
    let mut last_prefill_errors: u64 = 0;
    let mut last_prefill_conn_failures: u64 = 0;

    loop {
        std::thread::sleep(Duration::from_millis(100));

        // Check for signal
        if !running.load(Ordering::SeqCst) {
            shared.set_phase(Phase::Stop);
            if let Some(rs) = running_start {
                actual_duration = rs.elapsed();
            } else {
                actual_duration = Duration::ZERO;
            }
            break;
        }

        // Handle precheck -> prefill/warmup transition
        if current_phase == Phase::Precheck {
            let precheck_complete = shared.precheck_complete_count();
            if precheck_complete >= num_threads {
                let elapsed = precheck_start.map_or(Duration::ZERO, |t| t.elapsed());
                formatter.print_precheck_ok(elapsed);
                if prefill_enabled {
                    shared.set_phase(Phase::Prefill);
                    current_phase = Phase::Prefill;
                    prefill_start = Instant::now();
                    last_prefill_progress_time = Instant::now();
                    last_prefill_confirmed_snapshot = 0;
                    last_prefill_errors = metrics::REQUEST_ERRORS.value();
                    last_prefill_conn_failures = metrics::CONNECTIONS_FAILED.value();
                    formatter.print_prefill(key_count);
                    formatter.print_prefill_header();
                } else {
                    shared.set_phase(Phase::Warmup);
                    current_phase = Phase::Warmup;
                    warmup_start = Some(Instant::now());
                    formatter.print_warmup(warmup);
                }
                continue;
            }

            // Start the precheck timer once all workers have initialized
            // their event loops.  Until then, workers are still setting up
            // io_uring and haven't attempted any connections yet.
            if precheck_start.is_none() {
                if shared.workers_started() >= num_threads {
                    precheck_start = Some(Instant::now());
                } else if handles.iter().all(|h| h.is_finished()) {
                    // All workers exited before starting — fall through to
                    // timeout path which will report the failure.
                    precheck_start = Some(Instant::now() - precheck_timeout);
                } else {
                    continue;
                }
            }

            // Timeout detection
            let elapsed = precheck_start.unwrap().elapsed();
            if elapsed >= precheck_timeout {
                let conns_failed = metrics::CONNECTIONS_FAILED.value();
                formatter.print_precheck_failed(elapsed, conns_failed, config.target.protocol);
                shared.set_phase(Phase::Stop);

                // Shutdown workers cleanly
                shutdown_handle.shutdown();
                let shutdown_start = Instant::now();
                for (i, handle) in handles.into_iter().enumerate() {
                    let remaining =
                        WORKER_SHUTDOWN_TIMEOUT.saturating_sub(shutdown_start.elapsed());
                    if remaining.is_zero() {
                        tracing::warn!("shutdown timeout: some workers did not finish");
                        break;
                    }
                    if let Err(e) = handle.join() {
                        tracing::error!("worker {i} panicked during precheck shutdown: {e:?}");
                    }
                }

                return Err("precheck failed: no connectivity".into());
            }

            continue;
        }

        // Handle prefill -> warmup transition
        if current_phase == Phase::Prefill {
            let prefill_complete = shared.prefill_complete_count();
            if prefill_complete >= num_threads {
                shared.set_phase(Phase::Warmup);
                current_phase = Phase::Warmup;
                warmup_start = Some(Instant::now());
                formatter.print_warmup(warmup);
                continue;
            }

            let confirmed = shared.prefill_keys_confirmed();
            let total = shared.prefill_keys_total();
            let elapsed = prefill_start.elapsed();

            // Progress reporting
            if last_prefill_progress_report.elapsed() >= prefill_progress_interval && total > 0 {
                let report_secs = last_prefill_progress_report.elapsed().as_secs_f64();

                let delta_confirmed = confirmed - last_prefill_confirmed_snapshot;
                let set_per_sec = delta_confirmed as f64 / report_secs;
                last_prefill_confirmed_snapshot = confirmed;

                let errors = metrics::REQUEST_ERRORS.value();
                let delta_errors = errors - last_prefill_errors;
                let err_per_sec = delta_errors as f64 / report_secs;
                last_prefill_errors = errors;

                let conn_failures = metrics::CONNECTIONS_FAILED.value();
                let reconnects = conn_failures - last_prefill_conn_failures;
                last_prefill_conn_failures = conn_failures;

                let sample = PrefillSample {
                    elapsed,
                    confirmed,
                    total,
                    set_per_sec,
                    err_per_sec,
                    conns_active: metrics::CONNECTIONS_ACTIVE.value(),
                    reconnects,
                };
                formatter.print_prefill_sample(&sample);
                last_prefill_progress_report = Instant::now();
            }

            // Track progress for stall detection
            if confirmed > last_prefill_confirmed {
                last_prefill_confirmed = confirmed;
                last_prefill_progress_time = Instant::now();
            }

            // Stall detection: no progress for 30s since prefill started
            let stalled = last_prefill_progress_time.elapsed() >= prefill_stall_threshold;

            // Timeout detection (skip if timeout is zero = disabled)
            let timed_out = !prefill_timeout.is_zero() && elapsed >= prefill_timeout;

            if stalled || timed_out {
                let conns_active = metrics::CONNECTIONS_ACTIVE.value();
                let bytes_rx = metrics::BYTES_RX.value();
                let requests_sent = metrics::REQUESTS_SENT.value();

                let likely_cause = if conns_active == 0 {
                    PrefillStallCause::NoConnections
                } else if bytes_rx == 0 {
                    PrefillStallCause::NoResponses
                } else if stalled {
                    PrefillStallCause::Stalled
                } else if confirmed > 0 && total > confirmed {
                    // Still progressing but hit timeout - estimate remaining time
                    let rate = confirmed as f64 / elapsed.as_secs_f64();
                    let remaining_keys = (total - confirmed) as f64;
                    let estimated_remaining = if rate > 0.0 {
                        Duration::from_secs_f64(remaining_keys / rate)
                    } else {
                        Duration::from_secs(0)
                    };
                    PrefillStallCause::TooSlow {
                        estimated_remaining,
                    }
                } else {
                    PrefillStallCause::Unknown
                };

                let conns_failed = metrics::CONNECTIONS_FAILED.value();
                prefill_timeout_diag = Some(PrefillDiagnostics {
                    workers_complete: prefill_complete,
                    workers_total: num_threads,
                    keys_confirmed: confirmed,
                    keys_total: total,
                    elapsed,
                    conns_active,
                    conns_failed,
                    bytes_rx,
                    requests_sent,
                    likely_cause,
                });
                break;
            }

            continue;
        }

        // Calculate elapsed time since warmup started
        let warmup_start_time = warmup_start.unwrap_or(start);
        let elapsed = warmup_start_time.elapsed();

        // Check if we're done.
        // When saturation search is configured, the search controls its own
        // termination (stop_after_failures, max_rate), so we only stop on
        // the duration timer if no saturation search is configured.
        let saturation_done = saturation_state.as_ref().is_some_and(|s| s.is_completed());
        let has_saturation = config.workload.saturation_search.is_some();
        let time_done = elapsed >= warmup + duration;
        if saturation_done || (time_done && !has_saturation) {
            if let Some(rs) = running_start {
                actual_duration = rs.elapsed();
            }
            shared.set_phase(Phase::Stop);
            break;
        }

        // Transition from warmup to running
        if current_phase == Phase::Warmup && elapsed >= warmup {
            shared.set_phase(Phase::Running);
            current_phase = Phase::Running;
            running_start = Some(Instant::now());
            formatter.print_running(duration);
            formatter.print_header();
            last_report = Instant::now();

            // Capture baselines for all counters at the start of the recording phase.
            // Counters are now always incremented (including during prefill/warmup),
            // so we subtract these baselines in the final results.
            baseline_requests = metrics::REQUESTS_SENT.value();
            baseline_responses = metrics::RESPONSES_RECEIVED.value();
            baseline_errors = metrics::REQUEST_ERRORS.value();
            baseline_hits = metrics::CACHE_HITS.value();
            baseline_misses = metrics::CACHE_MISSES.value();
            baseline_get_count = metrics::GET_COUNT.value();
            baseline_set_count = metrics::SET_COUNT.value();
            baseline_backfill_set_count = metrics::BACKFILL_SET_COUNT.value();
            baseline_bytes_tx = metrics::BYTES_TX.value();
            baseline_bytes_rx = metrics::BYTES_RX.value();
            baseline_get_latency = metrics::GET_LATENCY.load();
            baseline_get_ttfb = metrics::GET_TTFB.load();
            baseline_set_latency = metrics::SET_LATENCY.load();
            baseline_backfill_set_latency = metrics::BACKFILL_SET_LATENCY.load();

            last_responses = baseline_responses;
            last_errors = baseline_errors;
            last_conn_failures = metrics::CONNECTIONS_FAILED.value();
            last_hits = baseline_hits;
            last_misses = baseline_misses;
            last_histogram = metrics::RESPONSE_LATENCY.load();

            if let Some(ref sat_config) = config.workload.saturation_search
                && let Some(ref rl) = ratelimiter
            {
                saturation_state = Some(SaturationSearchState::new(
                    sat_config.clone(),
                    Arc::clone(rl),
                ));
            }
        }

        // Skip reporting during warmup
        if current_phase != Phase::Running {
            continue;
        }

        // Periodic reporting
        if last_report.elapsed() >= report_interval {
            let responses = metrics::RESPONSES_RECEIVED.value();
            let errors = metrics::REQUEST_ERRORS.value();
            let conn_failures = metrics::CONNECTIONS_FAILED.value();
            let hits = metrics::CACHE_HITS.value();
            let misses = metrics::CACHE_MISSES.value();

            let elapsed_secs = last_report.elapsed().as_secs_f64();

            let delta_responses = responses - last_responses;
            let rate = delta_responses as f64 / elapsed_secs;
            last_responses = responses;

            let delta_errors = errors - last_errors;
            let delta_conn_failures = conn_failures - last_conn_failures;
            let err_rate = (delta_errors + delta_conn_failures) as f64 / elapsed_secs;
            last_errors = errors;
            last_conn_failures = conn_failures;

            let delta_hits = hits - last_hits;
            let delta_misses = misses - last_misses;
            let delta_gets = delta_hits + delta_misses;
            let hit_pct = if delta_gets > 0 {
                (delta_hits as f64 / delta_gets as f64) * 100.0
            } else {
                0.0
            };
            last_hits = hits;
            last_misses = misses;

            let current_histogram = metrics::RESPONSE_LATENCY.load();
            let (p50, p90, p99, p999, p9999, max) = match (&current_histogram, &last_histogram) {
                (Some(current), Some(previous)) => match current.wrapping_sub(previous) {
                    Ok(delta) => (
                        percentile_from_histogram(&delta, 0.50) / 1000.0,
                        percentile_from_histogram(&delta, 0.90) / 1000.0,
                        percentile_from_histogram(&delta, 0.99) / 1000.0,
                        percentile_from_histogram(&delta, 0.999) / 1000.0,
                        percentile_from_histogram(&delta, 0.9999) / 1000.0,
                        max_from_histogram(&delta) / 1000.0,
                    ),
                    Err(e) => {
                        tracing::warn!("histogram delta computation failed: {e}");
                        (0.0, 0.0, 0.0, 0.0, 0.0, 0.0)
                    }
                },
                (Some(current), None) => (
                    percentile_from_histogram(current, 0.50) / 1000.0,
                    percentile_from_histogram(current, 0.90) / 1000.0,
                    percentile_from_histogram(current, 0.99) / 1000.0,
                    percentile_from_histogram(current, 0.999) / 1000.0,
                    percentile_from_histogram(current, 0.9999) / 1000.0,
                    max_from_histogram(current) / 1000.0,
                ),
                _ => (0.0, 0.0, 0.0, 0.0, 0.0, 0.0),
            };
            last_histogram = current_histogram;

            let sample = Sample {
                timestamp: Utc::now(),
                req_per_sec: rate,
                err_per_sec: err_rate,
                hit_pct,
                p50_us: p50,
                p90_us: p90,
                p99_us: p99,
                p999_us: p999,
                p9999_us: p9999,
                max_us: max,
            };

            formatter.print_sample(&sample);

            // Diagnostic: absolute counter values to detect data flow
            tracing::trace!(
                responses_total = responses,
                requests_total = metrics::REQUESTS_SENT.value(),
                bytes_tx = metrics::BYTES_TX.value(),
                bytes_rx = metrics::BYTES_RX.value(),
                conns_active = metrics::CONNECTIONS_ACTIVE.value(),
                conns_failed = metrics::CONNECTIONS_FAILED.value(),
                "main thread diagnostic"
            );

            if let Some(ref mut state) = saturation_state {
                state.check_and_advance(&*formatter);
            }

            last_report = Instant::now();
        }
    }

    // Handle prefill timeout/stall
    if let Some(ref diag) = prefill_timeout_diag {
        shared.set_phase(Phase::Stop);
        formatter.print_prefill_timeout(diag);

        // Still shutdown workers cleanly
        shutdown_handle.shutdown();

        let shutdown_start = Instant::now();
        for (i, handle) in handles.into_iter().enumerate() {
            let remaining = WORKER_SHUTDOWN_TIMEOUT.saturating_sub(shutdown_start.elapsed());
            if remaining.is_zero() {
                tracing::warn!("shutdown timeout: some workers did not finish");
                break;
            }
            if let Err(e) = handle.join() {
                tracing::error!("worker {i} panicked during prefill shutdown: {e:?}");
            }
        }

        return Err("prefill failed: timed out or stalled".into());
    }

    // Shutdown ringline workers
    shutdown_handle.shutdown();

    // Wait for workers to finish with timeout
    let shutdown_start = Instant::now();
    for handle in handles {
        let remaining = WORKER_SHUTDOWN_TIMEOUT.saturating_sub(shutdown_start.elapsed());
        if remaining.is_zero() {
            tracing::warn!("shutdown timeout: some workers did not finish");
            break;
        }
        // JoinHandle doesn't have timeout, just join
        match handle.join() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::error!("worker thread returned error: {}", e),
            Err(e) => tracing::error!("worker thread panicked: {:?}", e),
        }
    }

    // Final report — subtract baselines captured at warmup->running transition
    let requests = metrics::REQUESTS_SENT.value() - baseline_requests;
    let responses = metrics::RESPONSES_RECEIVED.value() - baseline_responses;
    let errors = metrics::REQUEST_ERRORS.value() - baseline_errors;
    let hits = metrics::CACHE_HITS.value() - baseline_hits;
    let misses = metrics::CACHE_MISSES.value() - baseline_misses;
    let bytes_tx = metrics::BYTES_TX.value() - baseline_bytes_tx;
    let bytes_rx = metrics::BYTES_RX.value() - baseline_bytes_rx;
    let get_count = metrics::GET_COUNT.value() - baseline_get_count;
    let set_count = metrics::SET_COUNT.value() - baseline_set_count;
    let backfill_set_count = metrics::BACKFILL_SET_COUNT.value() - baseline_backfill_set_count;
    let active = metrics::CONNECTIONS_ACTIVE.value();
    let failed = metrics::CONNECTIONS_FAILED.value();
    let elapsed_secs = actual_duration.as_secs_f64();

    let get_latencies = delta_latency_stats(&metrics::GET_LATENCY, &baseline_get_latency);
    let get_ttfb = delta_latency_stats(&metrics::GET_TTFB, &baseline_get_ttfb);
    let set_latencies = delta_latency_stats(&metrics::SET_LATENCY, &baseline_set_latency);
    let backfill_set_latencies = delta_latency_stats(
        &metrics::BACKFILL_SET_LATENCY,
        &baseline_backfill_set_latency,
    );

    let results = Results {
        duration_secs: elapsed_secs,
        requests,
        responses,
        errors,
        hits,
        misses,
        bytes_tx,
        bytes_rx,
        get_count,
        set_count,
        get_latencies,
        get_ttfb,
        set_latencies,
        backfill_set_count,
        backfill_set_latencies,
        conns_active: active,
        conns_failed: failed,
        conns_total: total_connections as u64,
    };

    formatter.print_results(&results);

    if let Some(state) = saturation_state {
        formatter.print_saturation_results(&state.results());
    }

    drop(_admin_handle);

    Ok(())
}

/// Compute latency stats from the running-phase delta of an atomic histogram.
///
/// Subtracts the baseline snapshot (captured at warmup→running transition) from
/// the current cumulative histogram so that precheck/prefill/warmup samples are
/// excluded from the final results.
fn delta_latency_stats(hist: &AtomicHistogram, baseline: &Option<Histogram>) -> LatencyStats {
    let current = hist.load();
    let delta = match (&current, baseline) {
        (Some(cur), Some(base)) => cur.wrapping_sub(base).ok(),
        (Some(cur), None) => Some(cur.clone()),
        _ => None,
    };
    match delta {
        Some(d) => LatencyStats {
            p50_us: percentile_from_histogram(&d, 0.50) / 1000.0,
            p90_us: percentile_from_histogram(&d, 0.90) / 1000.0,
            p99_us: percentile_from_histogram(&d, 0.99) / 1000.0,
            p999_us: percentile_from_histogram(&d, 0.999) / 1000.0,
            p9999_us: percentile_from_histogram(&d, 0.9999) / 1000.0,
            max_us: max_from_histogram(&d) / 1000.0,
        },
        None => LatencyStats {
            p50_us: 0.0,
            p90_us: 0.0,
            p99_us: 0.0,
            p999_us: 0.0,
            p9999_us: 0.0,
            max_us: 0.0,
        },
    }
}

/// Get a percentile from a histogram snapshot.
fn percentile_from_histogram(hist: &Histogram, p: f64) -> f64 {
    match hist.percentiles(&[p]) {
        Ok(Some(results)) => {
            if let Some((_pct, bucket)) = results.first() {
                return bucket.end() as f64;
            }
        }
        Err(e) => {
            tracing::warn!("histogram percentile computation failed: {e}");
        }
        Ok(None) => {}
    }
    0.0
}

/// Get the max value from a histogram snapshot.
fn max_from_histogram(hist: &Histogram) -> f64 {
    match hist.percentiles(&[1.0]) {
        Ok(Some(results)) => {
            if let Some((_pct, bucket)) = results.first() {
                return bucket.end() as f64;
            }
        }
        Err(e) => {
            tracing::warn!("histogram max computation failed: {e}");
        }
        Ok(None) => {}
    }
    0.0
}

/// No-op certificate verifier for `tls_verify = false` (e.g., self-signed certs in CI).
#[derive(Debug)]
struct NoCertificateVerification;

impl rustls::client::danger::ServerCertVerifier for NoCertificateVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}
