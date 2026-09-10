#[cfg(target_os = "linux")]
use crate::config::TimestampMode;
use crate::config::{Config, Protocol as CacheProtocol};
use crate::keydist::KeyDist;
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
    // Fix the key format (hex vs UUID) before any key is generated, so the
    // key writer sees the configured format for the whole run.
    crate::worker::KEY_FORMAT.store(
        config.workload.keyspace.format as u8,
        std::sync::atomic::Ordering::Relaxed,
    );

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
    let warmup = effective_warmup(&config);
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
        // Ensure max_tokens can hold a full batch so try_wait_n(batch_size)
        // in the worker fire loops never deadlocks under very low rates.
        let max_tokens = initial_rate.max(config.connection.effective_batch_size() as u64);
        Some(Arc::new(
            Ratelimiter::builder(initial_rate)
                .initial_available(initial_rate)
                .max_tokens(max_tokens)
                .build()
                .expect("failed to build ratelimiter"),
        ))
    } else {
        None
    };
    metrics::TARGET_RATE.set(initial_rate as i64);

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
    let num_endpoints = config.target.endpoints.len();

    // Guardrail: prefill is drained from shared per-endpoint queues, so it can
    // only complete if every node has at least one connection across all
    // workers. Fewer connections than nodes leaves a node's queue with no
    // draining task — fail loud at startup instead of stalling silently
    // mid-prefill at conns/nodes progress.
    if prefill_enabled && num_endpoints > 1 && total_connections < num_endpoints {
        return Err(format!(
            "cluster prefill needs at least one connection per node, but \
             --connections={} < {} cluster nodes. Increase --connections to >= {} \
             (ideally >= threads x nodes for an even spread).",
            total_connections, num_endpoints, num_endpoints
        )
        .into());
    }

    // Build the shared per-endpoint prefill queues once: the full keyspace,
    // partitioned by owning endpoint. All workers share this `Arc` and drain
    // the queues for whichever endpoints their connections serve.
    let prefill_queues = Arc::new(crate::worker::build_prefill_queues(
        if prefill_enabled { key_count } else { 0 },
        config.workload.keyspace.length,
        &config.target.endpoints,
        &slot_table,
    ));
    if prefill_enabled {
        shared.add_prefill_total(key_count);
    }

    // Build the shared per-endpoint key-id lists once (full keyspace,
    // partitioned by owning endpoint). Connection tasks index these for O(1)
    // steady-state key selection instead of rejection-sampling+re-routing
    // random keys. Empty for single-endpoint setups (plain random key-id).
    let endpoint_keys = Arc::new(crate::worker::build_endpoint_keys(
        key_count,
        config.workload.keyspace.length,
        &config.target.endpoints,
        &slot_table,
    ));

    // Steady-state key distribution. Built ONCE here rather than per worker:
    // zipf setup computes zeta(n, theta), which is O(keyspace) and would
    // otherwise be repeated by every worker thread at startup.
    let key_dist = Arc::new(KeyDist::from_keyspace(&config.workload.keyspace));
    debug_assert_eq!(key_dist.len(), key_count);

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
                key_dist: Arc::clone(&key_dist),
                id,
                config: config.clone(),
                shared: Arc::clone(&shared),
                ratelimiter: ratelimiter.clone(),
                recording: false,
                prefill_queues: Arc::clone(&prefill_queues),
                endpoint_keys: Arc::clone(&endpoint_keys),
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
    let needs_tls = config.target.tls;
    let tls_client = if needs_tls {
        let tls_config = build_tls_client_config(&config.target)?;
        Some(ringline::TlsClientConfig::new(std::sync::Arc::new(
            tls_config,
        )))
    } else {
        None
    };

    let total_conns = config.connection.total_connections();
    let standalone_task_capacity = standalone_task_capacity(total_conns, num_threads);
    let timer_slots = timer_slots(total_conns, num_threads);

    let mut ringline_builder = ringline::ConfigBuilder::new()
        .workers(num_threads)
        .standalone_task_capacity(standalone_task_capacity)
        .timer_slots(timer_slots)
        .pin_to_core(false) // We pin in create_for_worker instead
        .core_offset(0)
        .tcp_nodelay(true)
        .loop_diag(config.general.ringline_diag);
    // No recv-buffer override: ringline's default geometry is used. The old
    // value-size-derived override (256 × 256KiB for large values) compensated
    // for a per-CQE-buffer-size starvation cliff that ringline's fallback-recv
    // (#274) + segmented recv (#286) have since eliminated. A/B verified
    // (2026-07-21, 2× c8gn.16xlarge, valkey 9.1.0 io-threads=16): with vs
    // without the override, GET at 1M/16M/64M values both saturate 200 GbE
    // (201 Gbps, 3/3 reps, byte-identical) — the override buys nothing on
    // current ringline, so the generator stays out of ringline's recv tuning.
    if let Some(tls_client) = tls_client {
        ringline_builder = ringline_builder.tls_client(tls_client);
    }
    #[cfg(target_os = "linux")]
    {
        ringline_builder =
            ringline_builder.timestamps(matches!(config.timestamps.mode, TimestampMode::Software));
    }
    let ringline_config = ringline_builder.build()?;

    // Enter the precheck phase BEFORE launching workers. `launch()` starts the
    // worker threads, which immediately run `on_start` → `connect()` → check
    // `phase == Precheck` to decide whether to send the precheck PING. If we set
    // the phase only after `launch()` returns, a worker whose connections
    // establish inside that window observes the initial `Connect` phase, skips
    // the PING permanently, and idle-spins in the workload loop — so its
    // connections never mark precheck complete and the run fails with
    // "no connectivity". Faster connection establishment (e.g. ringline 0.2)
    // makes that race reliably lost. Setting the phase first closes the window.
    shared.set_phase(Phase::Precheck);

    // Launch ringline workers (client-only, no bind)
    tracing::debug!(num_threads, "launching ringline workers");
    let (shutdown_handle, handles) =
        RinglineBuilder::new(ringline_config).launch::<crate::worker::BenchHandler>()?;
    tracing::debug!(workers = handles.len(), "ringline workers launched");

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
    let mut baseline_conn_failures = 0u64;
    let mut baseline_hits = 0u64;
    let mut baseline_misses = 0u64;
    let mut baseline_get_count = 0u64;
    let mut baseline_set_count = 0u64;
    let mut baseline_backfill_set_count = 0u64;
    let mut baseline_get_latency: Option<Histogram> = None;
    let mut baseline_get_ttfb: Option<Histogram> = None;
    let mut baseline_set_latency: Option<Histogram> = None;
    let mut baseline_backfill_set_latency: Option<Histogram> = None;
    let mut baseline_schedule_slip: Option<Histogram> = None;
    let mut baseline_perceived: Option<Histogram> = None;
    let mut baseline_requests_dropped = 0u64;
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
                formatter.print_precheck_failed(elapsed, conns_failed);
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
            let confirmed = shared.prefill_keys_confirmed();
            let total = shared.prefill_keys_total();

            // Prefill is a global operation drained from shared per-endpoint
            // queues; it completes when all assigned keys are confirmed — NOT
            // when N per-worker markers fire, since a worker may serve only a
            // subset of cluster nodes and would never reach a per-worker total.
            if total > 0 && confirmed >= total {
                shared.set_phase(Phase::Warmup);
                current_phase = Phase::Warmup;
                warmup_start = Some(Instant::now());
                formatter.print_warmup(warmup);
                continue;
            }

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
                    workers_complete: shared.prefill_complete_count(),
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
            baseline_conn_failures = metrics::CONNECTIONS_FAILED.value();
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
            baseline_schedule_slip = metrics::SCHEDULE_SLIP.load();
            baseline_perceived = metrics::PERCEIVED_LATENCY.load();
            baseline_requests_dropped = ratelimiter.as_ref().map(|rl| rl.dropped()).unwrap_or(0);

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
        let now = Instant::now();
        if now.duration_since(last_report) >= report_interval {
            let responses = metrics::RESPONSES_RECEIVED.value();
            let errors = metrics::REQUEST_ERRORS.value();
            let conn_failures = metrics::CONNECTIONS_FAILED.value();
            let hits = metrics::CACHE_HITS.value();
            let misses = metrics::CACHE_MISSES.value();

            let elapsed_secs = now.duration_since(last_report).as_secs_f64();

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

            if let Some(ref rl) = ratelimiter {
                metrics::REQUESTS_DROPPED.set(rl.dropped() as i64);
            }

            if let Some(ref mut state) = saturation_state {
                state.check_and_advance(&*formatter);
            }

            last_report = now;
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

    // Sample the active-connections gauge BEFORE initiating shutdown:
    // teardown closes connections (each close decrements the gauge), so a
    // post-join read reports whatever subset happened to close cleanly
    // rather than the connection count the benchmark actually ran with.
    let active = metrics::CONNECTIONS_ACTIVE.value();

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
    let conn_failures = metrics::CONNECTIONS_FAILED.value() - baseline_conn_failures;
    let errors = (metrics::REQUEST_ERRORS.value() - baseline_errors) + conn_failures;
    let hits = metrics::CACHE_HITS.value() - baseline_hits;
    let misses = metrics::CACHE_MISSES.value() - baseline_misses;
    let bytes_tx = metrics::BYTES_TX.value() - baseline_bytes_tx;
    let bytes_rx = metrics::BYTES_RX.value() - baseline_bytes_rx;
    let get_count = metrics::GET_COUNT.value() - baseline_get_count;
    let set_count = metrics::SET_COUNT.value() - baseline_set_count;
    let backfill_set_count = metrics::BACKFILL_SET_COUNT.value() - baseline_backfill_set_count;
    let failed = conn_failures;
    let elapsed_secs = actual_duration.as_secs_f64();

    let requests_dropped = ratelimiter
        .as_ref()
        .map(|rl| rl.dropped().saturating_sub(baseline_requests_dropped))
        .unwrap_or(0);

    let get_latencies = delta_latency_stats(&metrics::GET_LATENCY, &baseline_get_latency);
    let get_ttfb = delta_latency_stats(&metrics::GET_TTFB, &baseline_get_ttfb);
    let set_latencies = delta_latency_stats(&metrics::SET_LATENCY, &baseline_set_latency);
    let backfill_set_latencies = delta_latency_stats(
        &metrics::BACKFILL_SET_LATENCY,
        &baseline_backfill_set_latency,
    );
    let schedule_slip = delta_latency_stats(&metrics::SCHEDULE_SLIP, &baseline_schedule_slip);
    let perceived_latency = delta_latency_stats(&metrics::PERCEIVED_LATENCY, &baseline_perceived);

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
        requests_dropped,
        schedule_slip,
        perceived_latency,
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
    match hist.quantiles(&[p]) {
        Ok(Some(results)) => {
            if let Some(bucket) = results.entries().values().next() {
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
    match hist.quantile(1.0) {
        Ok(Some(results)) => return results.max().end() as f64,
        Err(e) => {
            tracing::warn!("histogram max computation failed: {e}");
        }
        Ok(None) => {}
    }
    0.0
}

/// Build the rustls client config from the target's TLS settings.
///
/// Two independent axes:
///
/// * **Server trust** (`tls_ca_file`) — which CAs verify the server. An
///   explicit CA file replaces the public roots rather than adding to them,
///   so a private CA is trusted and the public ones are not. This is what
///   `tls_verify = false` used to be the only workaround for, and unlike that
///   flag it still authenticates the server.
/// * **Client identity** (`tls_cert_file` + `tls_key_file`) — the certificate
///   presented for mutual TLS. Applied on both verification paths, so a
///   self-signed server and client auth can be used together.
fn build_tls_client_config(
    target: &crate::config::Target,
) -> Result<rustls::ClientConfig, Box<dyn std::error::Error>> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};

    let mut root_store = rustls::RootCertStore::empty();
    if let Some(ca_path) = &target.tls_ca_file {
        let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(ca_path)
            .map_err(|e| format!("tls_ca_file {}: {e}", ca_path.display()))?
            .collect::<Result<_, _>>()
            .map_err(|e| format!("tls_ca_file {}: {e}", ca_path.display()))?;

        // A CA file that parses but yields nothing would silently leave an
        // empty root store, failing every handshake with an opaque error.
        if certs.is_empty() {
            return Err(
                format!("tls_ca_file {} contains no certificates", ca_path.display()).into(),
            );
        }
        let (added, ignored) = root_store.add_parsable_certificates(certs);
        if added == 0 {
            return Err(format!(
                "tls_ca_file {} contains no usable CA certificates ({ignored} rejected)",
                ca_path.display()
            )
            .into());
        }
        if ignored > 0 {
            tracing::warn!(
                "tls_ca_file {}: {ignored} certificate(s) rejected, {added} loaded",
                ca_path.display()
            );
        }
    } else {
        root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    }

    let builder = if target.tls_verify {
        rustls::ClientConfig::builder().with_root_certificates(root_store)
    } else {
        rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(std::sync::Arc::new(NoCertificateVerification))
    };

    let tls_config = match (&target.tls_cert_file, &target.tls_key_file) {
        (Some(cert_path), Some(key_path)) => {
            let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert_path)
                .map_err(|e| format!("tls_cert_file {}: {e}", cert_path.display()))?
                .collect::<Result<_, _>>()
                .map_err(|e| format!("tls_cert_file {}: {e}", cert_path.display()))?;
            if chain.is_empty() {
                return Err(format!(
                    "tls_cert_file {} contains no certificates",
                    cert_path.display()
                )
                .into());
            }
            // rustls cannot read password-protected keys; an encrypted PEM
            // fails here rather than at handshake time.
            let key = PrivateKeyDer::from_pem_file(key_path)
                .map_err(|e| format!("tls_key_file {}: {e}", key_path.display()))?;
            builder
                .with_client_auth_cert(chain, key)
                .map_err(|e| format!("client certificate rejected: {e}"))?
        }
        // Half-configured pairs are rejected by config validation.
        _ => builder.with_no_client_auth(),
    };

    Ok(tls_config)
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

/// Connections handled by the busiest worker.
///
/// Connections are split evenly across workers with the remainder spread one
/// each, so ceiling division is the count the busiest worker sees.
fn connections_per_worker(total_connections: usize, num_threads: usize) -> usize {
    total_connections.div_ceil(num_threads.max(1))
}

/// ringline rejects per-worker pool sizes at or above `1 << 31`.
const RINGLINE_POOL_MAX: usize = (1 << 31) - 1;
/// ringline's own default for the pools sized here; the floor for small runs,
/// so a small run never ends up with *less* headroom than the default.
const RINGLINE_POOL_DEFAULT: usize = 256;
/// Slack above the per-worker connection count, for pool users that are not
/// 1:1 with connections.
const POOL_HEADROOM: usize = 16;

/// Standalone-task slab capacity to request from ringline, per worker.
///
/// Every connection is a standalone task (`worker::spawn_connection_tasks`
/// spawns one per connection), and ringline's default capacity is 256 per
/// worker — so a run asking for more than 256 connections on any one worker
/// silently lost the excess: `ringline::spawn` returns `Err` once the slab is
/// full, and those connections were never created while the run still reported
/// success. A 4096-connection run over 8 threads established exactly 2040
/// (8 × 255) and reported "0 failed".
///
/// Derived from the workload rather than set to a large constant, so the
/// ceiling tracks the connection count instead of becoming a new silent cap at
/// some higher number.
fn standalone_task_capacity(total_connections: usize, num_threads: usize) -> u32 {
    connections_per_worker(total_connections, num_threads)
        .saturating_add(POOL_HEADROOM)
        .clamp(RINGLINE_POOL_DEFAULT, RINGLINE_POOL_MAX) as u32
}

/// Timer slot capacity to request from ringline, per worker.
///
/// A sibling of the slab above: another per-worker ringline pool defaulting to
/// 256 and sized independently of the workload. Connection tasks take a timer
/// for request timeouts and for retry sleeps, so the pool scales with
/// connections — and unlike the task slab, exhausting it **panics** the worker
/// with `timer slot pool exhausted (256 slots) — raise Config::timer_slots`.
///
/// Sizing only the task slab moved the ceiling from a silent cap at 2040
/// connections to a hard abort at 4096, which is how this was found.
///
/// Allows two concurrent timers per connection. A connection can hold a request
/// timeout and a retry sleep across its lifecycle, and timer slots are small
/// enough that the margin costs nothing next to being one short.
fn timer_slots(total_connections: usize, num_threads: usize) -> u32 {
    connections_per_worker(total_connections, num_threads)
        .saturating_mul(2)
        .saturating_add(POOL_HEADROOM)
        .clamp(RINGLINE_POOL_DEFAULT, RINGLINE_POOL_MAX) as u32
}

#[cfg(test)]
mod tests {
    use super::{standalone_task_capacity, timer_slots};

    #[test]
    fn covers_the_connection_count_that_was_silently_capped() {
        // The regression: 4096 connections over 8 threads needs 512 per worker,
        // but ringline's 256 default admitted only 255, so the run established
        // 2040 and reported success.
        let capacity = standalone_task_capacity(4096, 8);
        assert!(
            capacity as usize >= 4096usize.div_ceil(8),
            "capacity {capacity} must cover 512 connections per worker"
        );
    }

    #[test]
    fn uneven_split_covers_the_busiest_worker() {
        // 4097 over 8 gives one worker 513; ceiling division must not round down.
        assert!(standalone_task_capacity(4097, 8) as usize >= 513);
    }

    #[test]
    fn small_runs_keep_the_ringline_default_as_a_floor() {
        assert_eq!(standalone_task_capacity(8, 8), 256);
        assert_eq!(standalone_task_capacity(0, 8), 256);
    }

    #[test]
    fn zero_threads_does_not_divide_by_zero() {
        assert_eq!(standalone_task_capacity(64, 0), 256);
    }

    #[test]
    fn timer_slots_cover_the_connection_count_that_panicked() {
        // ringline panics the worker when the timer pool is exhausted, and the
        // default 256 aborted a 4096-connection run over 8 threads.
        let slots = timer_slots(4096, 8);
        assert!(
            slots as usize >= 4096usize.div_ceil(8),
            "timer slots {slots} must cover 512 connections per worker"
        );
    }

    #[test]
    fn timer_slots_allow_more_than_one_timer_per_connection() {
        // A connection can hold a request timeout and a retry sleep at once.
        assert!(timer_slots(4096, 8) as usize >= 2 * 4096usize.div_ceil(8));
    }

    #[test]
    fn timer_slots_keep_the_ringline_default_as_a_floor() {
        assert_eq!(timer_slots(8, 8), 256);
        assert_eq!(timer_slots(0, 8), 256);
    }

    #[test]
    fn timer_slots_survive_zero_threads_and_absurd_counts() {
        assert_eq!(timer_slots(64, 0), 256);
        assert!((timer_slots(usize::MAX, 1) as u64) < (1 << 31));
    }

    #[test]
    fn absurd_connection_counts_stay_within_ringline_limits() {
        // ringline rejects >= 1 << 31 at config validation.
        assert!((standalone_task_capacity(usize::MAX, 1) as u64) < (1 << 31));
    }
}

#[cfg(test)]
mod tls_tests {
    use super::build_tls_client_config;
    use crate::config::{Protocol, Target};
    use std::path::{Path, PathBuf};
    use std::process::Command;

    fn target() -> Target {
        Target {
            endpoints: vec!["127.0.0.1:11211".parse().unwrap()],
            protocol: Protocol::Memcache,
            tls: true,
            tls_hostname: None,
            tls_verify: true,
            tls_ca_file: None,
            tls_cert_file: None,
            tls_key_file: None,
            cluster: false,
        }
    }

    fn tmpdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cc-tls-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// Generate a self-signed cert+key with openssl. Returns None when openssl
    /// is unavailable, so the suite still runs on a machine without it.
    fn gen_cert(dir: &Path) -> Option<(PathBuf, PathBuf)> {
        let crt = dir.join("c.crt");
        let key = dir.join("c.key");
        let out = Command::new("openssl")
            .args([
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-sha256",
                "-days",
                "3650",
                "-nodes",
                "-keyout",
                key.to_str()?,
                "-out",
                crt.to_str()?,
                "-subj",
                "/CN=cachecannon-test",
                "-addext",
                "subjectAltName=DNS:localhost,IP:127.0.0.1",
            ])
            .output()
            .ok()?;
        out.status.success().then_some((crt, key))
    }

    #[test]
    fn no_cert_options_uses_public_roots() {
        build_tls_client_config(&target()).expect("plain TLS config should build");
    }

    #[test]
    fn missing_ca_file_errors_with_the_path() {
        let dir = tmpdir("missing-ca");
        let mut t = target();
        t.tls_ca_file = Some(dir.join("does-not-exist.pem"));
        let err = build_tls_client_config(&t).expect_err("a missing CA file must not be ignored");
        assert!(
            err.to_string().contains("does-not-exist.pem"),
            "error should name the file: {err}"
        );
    }

    #[test]
    fn ca_file_without_certificates_is_rejected() {
        // An empty/garbage CA file would otherwise leave an empty root
        // store and fail every handshake with an opaque error.
        let dir = tmpdir("empty-ca");
        let ca = dir.join("empty.pem");
        std::fs::write(&ca, b"not a certificate\n").unwrap();
        let mut t = target();
        t.tls_ca_file = Some(ca);
        build_tls_client_config(&t).expect_err("a CA file with no certs must not build");
    }

    #[test]
    fn private_ca_is_accepted_as_a_root() {
        let dir = tmpdir("ca");
        let Some((crt, _key)) = gen_cert(&dir) else {
            return; // openssl unavailable
        };
        let mut t = target();
        t.tls_ca_file = Some(crt);
        build_tls_client_config(&t).expect("a private CA should load as a trust root");
    }

    #[test]
    fn client_certificate_is_accepted_for_mutual_tls() {
        let dir = tmpdir("mtls");
        let Some((crt, key)) = gen_cert(&dir) else {
            return;
        };
        let mut t = target();
        t.tls_cert_file = Some(crt.clone());
        t.tls_key_file = Some(key.clone());
        build_tls_client_config(&t).expect("a client certificate should build");

        // Client auth must also apply on the unverified-server path, so a
        // self-signed server and client auth can be used together.
        t.tls_verify = false;
        build_tls_client_config(&t).expect("client auth should apply with tls_verify = false");
    }

    #[test]
    fn missing_client_key_file_errors_with_the_path() {
        let dir = tmpdir("mtls-nokey");
        let Some((crt, _key)) = gen_cert(&dir) else {
            return;
        };
        let mut t = target();
        t.tls_cert_file = Some(crt);
        t.tls_key_file = Some(dir.join("absent.key"));
        let err = build_tls_client_config(&t).expect_err("a missing client key must error");
        assert!(
            err.to_string().contains("absent.key"),
            "error should name the file: {err}"
        );
    }
}

/// Warmup actually applied, which is longer than `general.warmup` when prefill
/// ran.
///
/// Prefill leaves dirty pages in the SERVER's page cache, and Linux does not
/// write them back promptly: `dirty_expire_centisecs` defaults to 30s, after
/// which the flusher evicts everything past expiry in one burst. Measured
/// against a disk-backed cache, that burst was ~4.8 GB at 483 MB/s arriving 40s
/// after prefill ended -- inside the first measured window, taking its p99 to
/// 151ms, which a saturation search then treated as the knee.
///
/// Warmup samples are discarded, so extending it past the expiry deadline moves
/// the burst out of the measurement. This is a named function rather than an
/// inline expression so the mapping from config to behavior can be asserted;
/// the equivalent inline version had no test and would not have failed if the
/// `prefill` condition were dropped.
fn effective_warmup(config: &Config) -> Duration {
    if config.workload.prefill {
        config.general.warmup + config.workload.prefill_settle
    } else {
        config.general.warmup
    }
}

#[cfg(test)]
mod warmup_tests {
    use super::effective_warmup;
    use crate::config::Config;
    use std::time::Duration;

    fn cfg(prefill: bool, warmup: &str, settle: Option<&str>) -> Config {
        let settle_line = settle
            .map(|s| format!("prefill_settle = \"{s}\""))
            .unwrap_or_default();
        toml::from_str(&format!(
            r#"
            [general]
            warmup = "{warmup}"
            [target]
            endpoints = ["127.0.0.1:11211"]
            protocol = "memcache-binary"
            [workload]
            prefill = {prefill}
            {settle_line}
            "#
        ))
        .expect("config should parse")
    }

    #[test]
    fn prefill_settle_defaults_to_past_dirty_expire() {
        // Linux dirty_expire_centisecs is 3000 (30s); a shorter default would
        // leave the writeback burst inside the first measured window.
        let c = cfg(true, "10s", None);
        assert!(
            c.workload.prefill_settle >= Duration::from_secs(31),
            "default settle {:?} does not outlast a 30s dirty expiry",
            c.workload.prefill_settle
        );
    }

    #[test]
    fn warmup_is_extended_only_when_prefill_runs() {
        // Without prefill there are no dirty pages to wait on, so a run against
        // a memory-only server must pay nothing.
        assert_eq!(
            effective_warmup(&cfg(false, "10s", Some("60s"))),
            Duration::from_secs(10)
        );
        assert_eq!(
            effective_warmup(&cfg(true, "10s", Some("60s"))),
            Duration::from_secs(70)
        );
    }

    #[test]
    fn zero_settle_restores_the_previous_behavior() {
        assert_eq!(
            effective_warmup(&cfg(true, "10s", Some("0s"))),
            Duration::from_secs(10)
        );
    }
}
