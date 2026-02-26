//! Worker implementation using ringline AsyncEventHandler.
//!
//! Each worker runs inside a ringline event loop. On startup, `on_start()` spawns
//! one async task per connection. Each task independently connects, drives
//! requests, parses responses, and reconnects on failure. `on_tick()` handles
//! phase transitions, diagnostics, and shutdown.

use crate::client::{MomentoSetup, RequestResult, RequestType};
use crate::config::{Config, Protocol as CacheProtocol, TimestampMode};
use crate::metrics;
use crate::ratelimit::DynamicRateLimiter;

use ringline::{AsyncEventHandler, ConnCtx, DriverCtx, RegionId, SendGuard};

use rand::prelude::*;
use rand_xoshiro::Xoshiro256PlusPlus;
use std::any::Any;
use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Test phase, controlled by main thread and read by workers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Phase {
    /// Initial connection phase
    Connect = 0,
    /// Precheck phase - verify basic connectivity with a test command
    Precheck = 1,
    /// Prefill phase - write each key exactly once
    Prefill = 2,
    /// Warmup phase - run workload but don't record metrics
    Warmup = 3,
    /// Main measurement phase - record metrics
    Running = 4,
    /// Stop phase - workers should exit
    Stop = 5,
}

impl Phase {
    #[inline]
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => Phase::Connect,
            1 => Phase::Precheck,
            2 => Phase::Prefill,
            3 => Phase::Warmup,
            4 => Phase::Running,
            _ => Phase::Stop,
        }
    }

    #[inline]
    pub fn is_recording(self) -> bool {
        self == Phase::Running
    }

    #[inline]
    pub fn should_stop(self) -> bool {
        self == Phase::Stop
    }
}

/// Reason for a connection disconnect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisconnectReason {
    /// Server closed connection (recv returned 0)
    Eof,
    /// Error during recv
    RecvError,
    /// Error during send
    SendError,
    /// Closed completion event from driver
    ClosedEvent,
    /// Error completion event from driver
    ErrorEvent,
    /// Failed to establish connection
    ConnectFailed,
}

/// Shared state between workers and main thread.
pub struct SharedState {
    /// Current test phase (controlled by main thread)
    phase: AtomicU8,
    /// Number of workers that have completed precheck
    precheck_done: AtomicUsize,
    /// Number of workers that have completed prefill
    prefill_complete: AtomicUsize,
    /// Total number of prefill keys confirmed across all workers
    prefill_keys_confirmed: AtomicUsize,
    /// Total number of prefill keys assigned across all workers
    prefill_keys_total: AtomicUsize,
}

impl SharedState {
    pub fn new() -> Self {
        Self {
            phase: AtomicU8::new(Phase::Connect as u8),
            precheck_done: AtomicUsize::new(0),
            prefill_complete: AtomicUsize::new(0),
            prefill_keys_confirmed: AtomicUsize::new(0),
            prefill_keys_total: AtomicUsize::new(0),
        }
    }

    /// Get the current phase.
    #[inline]
    pub fn phase(&self) -> Phase {
        Phase::from_u8(self.phase.load(Ordering::Acquire))
    }

    /// Set the phase (called by main thread).
    #[inline]
    pub fn set_phase(&self, phase: Phase) {
        self.phase.store(phase as u8, Ordering::Release);
    }

    /// Mark one worker's precheck as complete.
    #[inline]
    pub fn mark_precheck_complete(&self) {
        self.precheck_done.fetch_add(1, Ordering::Release);
    }

    /// Get the number of workers that have completed precheck.
    #[inline]
    pub fn precheck_complete_count(&self) -> usize {
        self.precheck_done.load(Ordering::Acquire)
    }

    /// Mark this worker's prefill as complete.
    #[inline]
    pub fn mark_prefill_complete(&self) {
        self.prefill_complete.fetch_add(1, Ordering::Release);
    }

    /// Get the number of workers that have completed prefill.
    #[inline]
    pub fn prefill_complete_count(&self) -> usize {
        self.prefill_complete.load(Ordering::Acquire)
    }

    /// Add to the total prefill keys confirmed count.
    #[inline]
    pub fn add_prefill_confirmed(&self, n: usize) {
        self.prefill_keys_confirmed.fetch_add(n, Ordering::Release);
    }

    /// Get the total prefill keys confirmed across all workers.
    #[inline]
    pub fn prefill_keys_confirmed(&self) -> usize {
        self.prefill_keys_confirmed.load(Ordering::Acquire)
    }

    /// Add to the total prefill keys assigned count.
    #[inline]
    pub fn add_prefill_total(&self, n: usize) {
        self.prefill_keys_total.fetch_add(n, Ordering::Release);
    }

    /// Get the total prefill keys assigned across all workers.
    #[inline]
    pub fn prefill_keys_total(&self) -> usize {
        self.prefill_keys_total.load(Ordering::Acquire)
    }
}

impl Default for SharedState {
    fn default() -> Self {
        Self::new()
    }
}

/// Worker configuration passed through the config channel.
pub struct BenchWorkerConfig {
    pub id: usize,
    pub config: Config,
    pub shared: Arc<SharedState>,
    pub ratelimiter: Option<Arc<DynamicRateLimiter>>,
    /// Whether to record metrics (only true during Running phase)
    pub recording: bool,
    /// Range of key IDs this worker should prefill (start..end).
    pub prefill_range: Option<std::ops::Range<usize>>,
    /// CPU IDs for pinning (if configured).
    pub cpu_ids: Option<Vec<usize>>,
    /// Shared 1GB random value pool (all workers share the same Arc).
    pub value_pool: Arc<Vec<u8>>,
    /// Cluster mode: slot → endpoint index (16384 entries). None = ketama routing.
    pub slot_table: Option<Vec<u16>>,
}

// ── Config channel (same pattern as server) ──────────────────────────────

static CONFIG_CHANNEL: OnceLock<Mutex<Box<dyn Any + Send>>> = OnceLock::new();

/// Initialize the global config channel. Must be called before launch.
pub fn init_config_channel(rx: crossbeam_channel::Receiver<BenchWorkerConfig>) {
    let boxed: Box<dyn Any + Send> = Box::new(rx);
    CONFIG_CHANNEL
        .set(Mutex::new(boxed))
        .expect("init_config_channel called twice");
}

fn recv_config() -> BenchWorkerConfig {
    let guard = CONFIG_CHANNEL
        .get()
        .expect("config channel not initialized")
        .lock()
        .unwrap();
    let rx = guard
        .downcast_ref::<crossbeam_channel::Receiver<BenchWorkerConfig>>()
        .expect("wrong config channel type");
    rx.recv().expect("config channel closed")
}

// ── ValuePoolGuard ────────────────────────────────────────────────────────

/// Zero-copy send guard referencing a slice of the shared value pool.
///
/// When used with `conn.send_parts().build()`, the kernel sends directly from
/// the pool memory via SendMsgZc. The `Arc<Vec<u8>>` keeps the pool alive
/// until the kernel signals completion.
///
/// Size: 8 (Arc ptr) + 4 + 4 = 16 bytes, well within GuardBox's 64-byte limit.
struct ValuePoolGuard {
    pool: Arc<Vec<u8>>,
    offset: u32,
    len: u32,
}

impl SendGuard for ValuePoolGuard {
    fn as_ptr_len(&self) -> (*const u8, u32) {
        // SAFETY: offset + len is always within the pool bounds (enforced at construction).
        let ptr = unsafe { self.pool.as_ptr().add(self.offset as usize) };
        (ptr, self.len)
    }

    fn region(&self) -> RegionId {
        RegionId::UNREGISTERED
    }
}

/// Create a ValuePoolGuard referencing a random slice of the value pool.
fn make_value_guard(
    rng: &mut Xoshiro256PlusPlus,
    value_pool: &Arc<Vec<u8>>,
    value_len: usize,
    pool_len: usize,
) -> ValuePoolGuard {
    let max_offset = pool_len - value_len;
    let offset = rng.random_range(0..=max_offset);
    ValuePoolGuard {
        pool: Arc::clone(value_pool),
        offset: offset as u32,
        len: value_len as u32,
    }
}

// ── Shared task state (Arc-wrapped, accessed by all connection tasks) ────

/// State shared across all connection tasks spawned by a single worker.
struct TaskSharedState {
    config: Config,
    shared: Arc<SharedState>,
    ratelimiter: Option<Arc<DynamicRateLimiter>>,
    value_pool: Arc<Vec<u8>>,
    endpoints: Vec<SocketAddr>,
    ring: ketama::Ring,
    slot_table: Mutex<Option<Vec<u16>>>,
    /// Shared prefill queue: tasks pop key IDs from here.
    prefill_queue: Mutex<VecDeque<usize>>,
    /// Worker ID for logging.
    worker_id: usize,
    /// Whether backfill_on_miss is enabled.
    backfill_on_miss: bool,
    /// Momento setup (resolved once, shared across tasks).
    momento_setup: Mutex<Option<MomentoSetup>>,
    /// Whether TLS is enabled for connections.
    tls_enabled: bool,
    /// SNI server name for TLS connections.
    tls_server_name: Option<String>,
}

/// State shared between the BenchHandler (on_tick) and connection tasks.
/// Wrapped in Arc so on_tick and spawned tasks can both access it.
struct SharedWorkerState {
    task_state: Arc<TaskSharedState>,
    /// Prefill tracking: total keys assigned to this worker.
    prefill_total: usize,
    /// Whether prefill is already complete for this worker.
    prefill_done: AtomicU8, // 0 = not done, 1 = done
    /// Whether precheck is already complete for this worker.
    precheck_done: AtomicU8, // 0 = not done, 1 = done
}

impl SharedWorkerState {
    fn is_prefill_done(&self) -> bool {
        self.prefill_done.load(Ordering::Acquire) != 0
    }

    fn is_precheck_done(&self) -> bool {
        self.precheck_done.load(Ordering::Acquire) != 0
    }

    /// Mark this worker's precheck as complete (only the first connection to succeed does this).
    fn mark_precheck_done(&self) {
        if self
            .precheck_done
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.task_state.shared.mark_precheck_complete();
        }
    }
}

// ── BenchHandler ─────────────────────────────────────────────────────────

/// Benchmark worker async event handler for ringline.
pub struct BenchHandler {
    id: usize,
    shared: Arc<SharedState>,
    worker_state: Arc<SharedWorkerState>,

    /// Last observed phase (for transition detection)
    last_phase: Phase,
    /// Whether to record metrics (only true during Running phase)
    recording: bool,

    /// Tick counter for periodic diagnostics
    tick_count: u64,
    /// Last diagnostic log time
    last_diag: Instant,

    /// Number of connections this worker manages
    my_connections: usize,
}

impl AsyncEventHandler for BenchHandler {
    #[allow(clippy::manual_async_fn)]
    fn on_accept(&self, _conn: ConnCtx) -> impl Future<Output = ()> + 'static {
        // Benchmark is client-only, no accepts expected
        async {}
    }

    fn on_start(&self) -> Option<Pin<Box<dyn Future<Output = ()> + 'static>>> {
        let worker_state = Arc::clone(&self.worker_state);
        let my_connections = self.my_connections;
        let worker_id = self.id;
        let protocol = worker_state.task_state.config.target.protocol;

        Some(Box::pin(async move {
            match protocol {
                CacheProtocol::Momento => {
                    spawn_momento_tasks(&worker_state, my_connections, worker_id);
                }
                CacheProtocol::Ping => {
                    spawn_protocol_tasks(&worker_state, my_connections, worker_id);
                }
                CacheProtocol::Resp | CacheProtocol::Resp3 => {
                    spawn_protocol_tasks(&worker_state, my_connections, worker_id);
                }
                CacheProtocol::Memcache => {
                    spawn_protocol_tasks(&worker_state, my_connections, worker_id);
                }
                CacheProtocol::MemcacheBinary => {
                    tracing::error!(
                        "MemcacheBinary protocol is not supported; use Memcache (ASCII) instead"
                    );
                }
            }
        }))
    }

    fn on_tick(&mut self, ctx: &mut DriverCtx<'_>) {
        let phase = self.shared.phase();

        // Update recording state on phase transition
        if phase != self.last_phase {
            if phase == Phase::Running {
                tracing::debug!(worker = self.id, "entering Running phase");
            }
            self.recording = phase.is_recording();
            self.last_phase = phase;
        }

        // Check for shutdown
        if phase.should_stop() {
            ctx.request_shutdown();
            return;
        }

        // Periodic diagnostic heartbeat (every 2 seconds)
        self.tick_count += 1;
        if self.last_diag.elapsed() >= Duration::from_secs(2) {
            tracing::trace!(
                worker = self.id,
                phase = ?phase,
                recording = self.recording,
                ticks = self.tick_count,
                connections = self.my_connections,
                "diagnostic heartbeat"
            );
            self.tick_count = 0;
            self.last_diag = Instant::now();
        }
    }

    fn create_for_worker(worker_id: usize) -> Self {
        tracing::debug!(worker_id, "worker thread starting create_for_worker");
        let cfg = recv_config();
        if let Some(ref ids) = cfg.cpu_ids
            && !ids.is_empty()
        {
            let cpu_id = ids[cfg.id % ids.len()];
            #[cfg(target_os = "linux")]
            {
                if let Err(e) = pin_to_cpu(cpu_id) {
                    tracing::warn!("failed to pin worker {} to CPU {}: {}", cfg.id, cpu_id, e);
                } else {
                    tracing::debug!("pinned worker {} to CPU {}", cfg.id, cpu_id);
                }
            }
            let _ = cpu_id;
        }

        // Set metrics thread shard
        metrics::set_thread_shard(worker_id);

        // Initialize prefill state
        let (prefill_queue, prefill_total, prefill_done) = match cfg.prefill_range {
            Some(range) => {
                let pending: VecDeque<usize> = (range.start..range.end).collect();
                let total = range.end - range.start;
                cfg.shared.add_prefill_total(total);
                tracing::debug!(
                    worker_id = cfg.id,
                    total,
                    start = range.start,
                    end = range.end,
                    "prefill initialized"
                );
                (pending, total, total == 0)
            }
            None => {
                cfg.shared.mark_prefill_complete();
                (VecDeque::new(), 0, true)
            }
        };

        // Compute connection distribution
        let endpoints = cfg.config.target.endpoints.clone();
        let total_connections = cfg.config.connection.total_connections();
        let num_threads = cfg.config.general.threads;

        let base_per_thread = total_connections / num_threads;
        let remainder = total_connections % num_threads;
        let my_connections = if cfg.id < remainder {
            base_per_thread + 1
        } else {
            base_per_thread
        };

        let backfill_on_miss = cfg.config.workload.backfill_on_miss;
        let slot_table = cfg.slot_table;

        // Build ketama consistent hash ring from endpoint addresses.
        // For Momento, endpoints is empty — use a dummy single-node ring
        // (the ring is never consulted when there are no endpoints).
        let ring = if endpoints.is_empty() {
            ketama::Ring::build(&["_"])
        } else {
            let server_ids: Vec<String> = endpoints.iter().map(|a| a.to_string()).collect();
            ketama::Ring::build(&server_ids.iter().map(|s| s.as_str()).collect::<Vec<_>>())
        };

        let tls_enabled = cfg.config.target.tls;
        let tls_server_name = cfg.config.target.tls_hostname.clone();

        let task_state = Arc::new(TaskSharedState {
            config: cfg.config.clone(),
            shared: Arc::clone(&cfg.shared),
            ratelimiter: cfg.ratelimiter.clone(),
            value_pool: cfg.value_pool,
            endpoints,
            ring,
            slot_table: Mutex::new(slot_table),
            prefill_queue: Mutex::new(prefill_queue),
            worker_id: cfg.id,
            backfill_on_miss,
            momento_setup: Mutex::new(None),
            tls_enabled,
            tls_server_name,
        });

        // Workers with no connections immediately pass precheck
        let precheck_already_done = my_connections == 0;
        if precheck_already_done {
            cfg.shared.mark_precheck_complete();
        }

        let worker_state = Arc::new(SharedWorkerState {
            task_state,
            prefill_total,
            prefill_done: AtomicU8::new(if prefill_done { 1 } else { 0 }),
            precheck_done: AtomicU8::new(if precheck_already_done { 1 } else { 0 }),
        });

        let result = BenchHandler {
            id: cfg.id,
            shared: cfg.shared,
            worker_state,
            last_phase: Phase::Connect,
            recording: cfg.recording,
            tick_count: 0,
            last_diag: Instant::now(),
            my_connections,
        };
        tracing::debug!(
            worker_id = result.id,
            my_connections = result.my_connections,
            "worker create_for_worker complete, entering event loop"
        );
        result
    }
}

// ── Spawn helpers ────────────────────────────────────────────────────────

/// Spawn one async task per protocol connection (RESP, Memcache, or Ping).
fn spawn_protocol_tasks(
    worker_state: &Arc<SharedWorkerState>,
    my_connections: usize,
    worker_id: usize,
) {
    let num_endpoints = worker_state.task_state.endpoints.len();
    let total_connections = worker_state
        .task_state
        .config
        .connection
        .total_connections();
    let num_threads = worker_state.task_state.config.general.threads;

    let base_per_thread = total_connections / num_threads;
    let remainder = total_connections % num_threads;
    let my_start = if worker_id < remainder {
        worker_id * (base_per_thread + 1)
    } else {
        remainder * (base_per_thread + 1) + (worker_id - remainder) * base_per_thread
    };

    let protocol = worker_state.task_state.config.target.protocol;

    for i in 0..my_connections {
        let global_conn_idx = my_start + i;
        let endpoint_idx = global_conn_idx % num_endpoints;
        let state = Arc::clone(worker_state);
        let session_seed = 42 + worker_id as u64 * 10000 + i as u64;

        let _ = ringline::spawn(async move {
            match protocol {
                CacheProtocol::Resp | CacheProtocol::Resp3 => {
                    resp_connection_task(state, endpoint_idx, session_seed).await;
                }
                CacheProtocol::Memcache => {
                    memcache_connection_task(state, endpoint_idx, session_seed).await;
                }
                CacheProtocol::Ping => {
                    ping_connection_task(state, endpoint_idx, session_seed).await;
                }
                _ => unreachable!(),
            }
        });
    }
}

/// Spawn one async task per Momento TLS connection.
fn spawn_momento_tasks(
    worker_state: &Arc<SharedWorkerState>,
    my_connections: usize,
    worker_id: usize,
) {
    // Resolve Momento setup once
    {
        let mut setup_guard = worker_state.task_state.momento_setup.lock().unwrap();
        if setup_guard.is_none() {
            match MomentoSetup::from_config(&worker_state.task_state.config) {
                Ok(setup) => *setup_guard = Some(setup),
                Err(e) => {
                    tracing::error!("failed to resolve Momento config: {}", e);
                    metrics::CONNECTIONS_FAILED.increment();
                    return;
                }
            }
        }
    }

    for i in 0..my_connections {
        let state = Arc::clone(worker_state);
        let session_seed = 42 + worker_id as u64 * 10000 + i as u64;
        let conn_idx = i;

        let _ = ringline::spawn(async move {
            momento_connection_task(state, conn_idx, session_seed).await;
        });
    }
}

// ── Connection establishment helper ──────────────────────────────────────

/// Try to connect to an endpoint, returning Ok(conn) or Err with retry sleep.
/// When `tls_server_name` is Some, uses TLS via `ringline::connect_tls`.
async fn establish_connection(
    endpoint: SocketAddr,
    worker_id: usize,
    tls_server_name: Option<&str>,
) -> Result<ConnCtx, DisconnectReason> {
    let connect_result = if let Some(server_name) = tls_server_name {
        ringline::connect_tls(endpoint, server_name)
    } else {
        ringline::connect(endpoint)
    };

    match connect_result {
        Ok(future) => match future.await {
            Ok(conn) => Ok(conn),
            Err(e) => {
                tracing::debug!(
                    worker = worker_id,
                    endpoint = %endpoint,
                    "connect failed: {}",
                    e
                );
                metrics::CONNECTIONS_FAILED.increment();
                metrics::DISCONNECTS_CONNECT_FAILED.increment();
                Err(DisconnectReason::ConnectFailed)
            }
        },
        Err(e) => {
            tracing::warn!(
                worker = worker_id,
                endpoint = %endpoint,
                "connect initiation failed: {}",
                e
            );
            metrics::CONNECTIONS_FAILED.increment();
            Err(DisconnectReason::ConnectFailed)
        }
    }
}

/// Resolve the TLS server name for an endpoint from task shared state.
/// Returns None if TLS is disabled, otherwise the explicit hostname or the IP string.
fn resolve_tls_server_name(state: &TaskSharedState, endpoint: SocketAddr) -> Option<String> {
    if !state.tls_enabled {
        return None;
    }
    Some(
        state
            .tls_server_name
            .clone()
            .unwrap_or_else(|| endpoint.ip().to_string()),
    )
}

// ── on_result callback factories ─────────────────────────────────────────

/// Create a RESP on_result callback that records latency metrics only.
/// Counter metrics (RESPONSES_RECEIVED, GET_COUNT, etc.) are recorded by record_counters().
fn make_resp_callback() -> impl Fn(&ringline_redis::CommandResult) {
    move |r| {
        match r.command {
            ringline_redis::CommandType::Get => {
                let _ = metrics::GET_LATENCY.increment(r.latency_ns);
            }
            ringline_redis::CommandType::Set => {
                let _ = metrics::SET_LATENCY.increment(r.latency_ns);
            }
            ringline_redis::CommandType::Del => {
                let _ = metrics::DELETE_LATENCY.increment(r.latency_ns);
            }
            _ => {}
        }
        let _ = metrics::RESPONSE_LATENCY.increment(r.latency_ns);
    }
}

/// Create a Memcache on_result callback that records latency metrics only.
/// Counter metrics (RESPONSES_RECEIVED, GET_COUNT, etc.) are recorded by record_counters().
fn make_memcache_callback() -> impl Fn(&ringline_memcache::CommandResult) {
    move |r| {
        match r.command {
            ringline_memcache::CommandType::Get => {
                let _ = metrics::GET_LATENCY.increment(r.latency_ns);
            }
            ringline_memcache::CommandType::Set => {
                let _ = metrics::SET_LATENCY.increment(r.latency_ns);
            }
            ringline_memcache::CommandType::Delete => {
                let _ = metrics::DELETE_LATENCY.increment(r.latency_ns);
            }
            _ => {}
        }
        let _ = metrics::RESPONSE_LATENCY.increment(r.latency_ns);
    }
}

/// Create a Ping on_result callback that records metrics for each completed ping.
fn make_ping_callback() -> impl Fn(&ringline_ping::CommandResult) {
    move |r| {
        metrics::RESPONSES_RECEIVED.increment();
        metrics::GET_COUNT.increment(); // Ping counts as GET for metrics
        if !r.success {
            metrics::REQUEST_ERRORS.increment();
        }
        let _ = metrics::RESPONSE_LATENCY.increment(r.latency_ns);
        let _ = metrics::GET_LATENCY.increment(r.latency_ns);
    }
}

/// Create a Momento on_result callback that records latency metrics for each completed command.
fn make_momento_callback() -> impl Fn(&ringline_momento::CommandResult) {
    move |r| {
        match r.command {
            ringline_momento::CommandType::Get => {
                let _ = metrics::GET_LATENCY.increment(r.latency_ns);
            }
            ringline_momento::CommandType::Set => {
                let _ = metrics::SET_LATENCY.increment(r.latency_ns);
            }
            ringline_momento::CommandType::Delete => {
                let _ = metrics::DELETE_LATENCY.increment(r.latency_ns);
            }
        }
        let _ = metrics::RESPONSE_LATENCY.increment(r.latency_ns);
    }
}

// ── RESP connection task ─────────────────────────────────────────────────

/// A single RESP (Redis) connection task that connects, drives workload, and reconnects.
async fn resp_connection_task(state: Arc<SharedWorkerState>, endpoint_idx: usize, seed: u64) {
    let endpoint = state.task_state.endpoints[endpoint_idx];
    let config = &state.task_state.config;
    let tls_name = resolve_tls_server_name(&state.task_state, endpoint);
    let mut rng = Xoshiro256PlusPlus::seed_from_u64(seed);
    let mut key_buf = vec![0u8; config.workload.keyspace.length];
    let mut backfill_queue: Vec<usize> = Vec::new();

    loop {
        let phase = state.task_state.shared.phase();
        if phase.should_stop() {
            return;
        }

        let conn =
            match establish_connection(endpoint, state.task_state.worker_id, tls_name.as_deref())
                .await
            {
                Ok(conn) => conn,
                Err(_) => {
                    ringline::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };

        metrics::CONNECTIONS_ACTIVE.increment();
        let use_kernel_ts = matches!(config.timestamps.mode, TimestampMode::Software);
        let mut client = ringline_redis::Client::builder(conn)
            .on_result(make_resp_callback())
            .kernel_timestamps(use_kernel_ts)
            .build();

        tracing::debug!(
            worker = state.task_state.worker_id,
            endpoint = %endpoint,
            conn_index = conn.index(),
            "connected (RESP)"
        );

        // Precheck: send PING to verify connectivity
        if state.task_state.shared.phase() == Phase::Precheck && !state.is_precheck_done() {
            match client.ping().await {
                Ok(()) => {
                    tracing::debug!(
                        worker = state.task_state.worker_id,
                        endpoint = %endpoint,
                        "precheck PING ok"
                    );
                    state.mark_precheck_done();
                }
                Err(e) => {
                    tracing::debug!(
                        worker = state.task_state.worker_id,
                        endpoint = %endpoint,
                        "precheck PING failed: {}",
                        e
                    );
                    metrics::CONNECTIONS_ACTIVE.decrement();
                    metrics::CONNECTIONS_FAILED.increment();
                    ringline::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            }
            // Wait for phase to advance past Precheck
            while state.task_state.shared.phase() == Phase::Precheck {
                if state.task_state.shared.phase().should_stop() {
                    metrics::CONNECTIONS_ACTIVE.decrement();
                    return;
                }
                ringline::sleep(Duration::from_millis(10)).await;
            }
        }

        let result = drive_resp_workload(
            &mut client,
            &state,
            endpoint_idx,
            &mut rng,
            &mut key_buf,
            &mut backfill_queue,
        )
        .await;

        metrics::CONNECTIONS_ACTIVE.decrement();

        match result {
            Ok(()) => return,
            Err(reason) => {
                metrics::CONNECTIONS_FAILED.increment();
                record_disconnect_reason(reason);
            }
        }

        ringline::sleep(Duration::from_millis(100)).await;
    }
}

/// Drive the RESP workload on a connected client using fire/recv pipelining.
async fn drive_resp_workload(
    client: &mut ringline_redis::Client,
    state: &Arc<SharedWorkerState>,
    endpoint_idx: usize,
    rng: &mut Xoshiro256PlusPlus,
    key_buf: &mut [u8],
    backfill_queue: &mut Vec<usize>,
) -> Result<(), DisconnectReason> {
    let config = &state.task_state.config;
    let key_count = config.workload.keyspace.count;
    let get_ratio = config.workload.commands.get as usize;
    let delete_ratio = config.workload.commands.delete as usize;
    let value_len = config.workload.values.length;
    let pool_len = state.task_state.value_pool.len();
    let backfill_on_miss = state.task_state.backfill_on_miss;
    let multi_endpoint = state.task_state.endpoints.len() > 1;
    let pipeline_depth = config.connection.pipeline_depth;

    loop {
        let phase = state.task_state.shared.phase();
        if phase.should_stop() {
            return Ok(());
        }

        // Fill pipeline
        if phase == Phase::Prefill && !state.is_prefill_done() {
            while client.pending_count() < pipeline_depth {
                let key_id = {
                    let mut queue = state.task_state.prefill_queue.lock().unwrap();
                    match queue.pop_front() {
                        Some(id) => id,
                        None => break,
                    }
                };

                write_key(key_buf, key_id);
                let guard =
                    make_value_guard(rng, &state.task_state.value_pool, value_len, pool_len);

                match client.fire_set_with_guard(key_buf, guard, 0) {
                    Ok(_) => {
                        metrics::REQUESTS_SENT.increment();
                    }
                    Err(_) => {
                        let mut queue = state.task_state.prefill_queue.lock().unwrap();
                        queue.push_front(key_id);
                        break;
                    }
                }
            }
        } else if phase == Phase::Warmup || phase == Phase::Running {
            while client.pending_count() < pipeline_depth {
                // Drain backfill queue first
                if let Some(key_id) = backfill_queue.pop() {
                    write_key(key_buf, key_id);
                    let guard =
                        make_value_guard(rng, &state.task_state.value_pool, value_len, pool_len);

                    if let Some(ref rl) = state.task_state.ratelimiter
                        && !rl.try_acquire()
                    {
                        backfill_queue.push(key_id);
                        break;
                    }

                    // user_data encodes key_id with backfill marker (high bit set)
                    let user_data = key_id as u64 | BACKFILL_MARKER;
                    match client.fire_set_with_guard(key_buf, guard, user_data) {
                        Ok(_) => {
                            metrics::REQUESTS_SENT.increment();
                        }
                        Err(_) => {
                            backfill_queue.push(key_id);
                            break;
                        }
                    }
                    continue;
                }

                // Rate limiting
                if let Some(ref rl) = state.task_state.ratelimiter
                    && !rl.try_acquire()
                {
                    break;
                }

                // Generate random key
                let key_id = rng.random_range(0..key_count);
                write_key(key_buf, key_id);

                // Multi-endpoint routing
                if multi_endpoint {
                    let routed = route_key(&state.task_state, key_buf);
                    if routed != endpoint_idx {
                        continue;
                    }
                }

                // Choose command
                let roll = rng.random_range(0..100);
                let sent = if roll < get_ratio {
                    // Pass key_id as user_data for backfill-on-miss tracking
                    client.fire_get(key_buf, key_id as u64).is_ok()
                } else if roll < get_ratio + delete_ratio {
                    client.fire_del(key_buf, 0).is_ok()
                } else {
                    let guard =
                        make_value_guard(rng, &state.task_state.value_pool, value_len, pool_len);
                    client.fire_set_with_guard(key_buf, guard, 0).is_ok()
                };

                if sent {
                    metrics::REQUESTS_SENT.increment();
                } else {
                    break;
                }
            }
        }

        // Wait for one response
        if client.pending_count() == 0 {
            continue;
        }

        let op = match client.recv().await {
            Ok(op) => op,
            Err(ringline_redis::Error::ConnectionClosed) => {
                return Err(DisconnectReason::Eof);
            }
            Err(_) => {
                return Err(DisconnectReason::RecvError);
            }
        };

        // Map CompletedOp to RequestResult
        let result = map_resp_op(op);

        // Handle prefill tracking
        if phase == Phase::Prefill
            && !state.is_prefill_done()
            && result.request_type == RequestType::Set
            && result.success
        {
            confirm_prefill_key(state);
        }

        // Handle backfill-on-miss
        if backfill_on_miss
            && result.request_type == RequestType::Get
            && result.success
            && result.hit == Some(false)
            && let Some(key_id) = result.key_id
        {
            backfill_queue.push(key_id);
        }

        // Handle backfill SET tracking
        if result.backfill && result.request_type == RequestType::Set && result.success {
            metrics::BACKFILL_SET_COUNT.increment();
        }

        // Handle cluster redirects
        if let Some(ref redirect) = result.redirect {
            check_resp_redirect_parsed(redirect, state);
        }

        // Record counter metrics (latency is recorded by the on_result callback)
        record_counters(&result);
    }
}

/// Marker bit for backfill SET user_data (high bit of u64).
const BACKFILL_MARKER: u64 = 1 << 63;

/// Map a ringline-redis CompletedOp to a RequestResult.
fn map_resp_op(op: ringline_redis::CompletedOp) -> RequestResult {
    match op {
        ringline_redis::CompletedOp::Get { result, user_data } => {
            let (success, is_error, hit) = match &result {
                Ok(Some(_)) => (true, false, Some(true)),
                Ok(None) => (true, false, Some(false)),
                Err(_) => (false, true, None),
            };
            let redirect = if let Err(ringline_redis::Error::Redis(ref msg)) = result {
                parse_resp_redirect(msg)
            } else {
                None
            };
            RequestResult {
                id: 0,
                success,
                is_error_response: is_error,
                latency_ns: 0,
                ttfb_ns: None,
                request_type: RequestType::Get,
                hit,
                key_id: Some(user_data as usize),
                backfill: false,
                redirect,
            }
        }
        ringline_redis::CompletedOp::Set { result, user_data } => {
            let (success, is_error) = match &result {
                Ok(()) => (true, false),
                Err(_) => (false, true),
            };
            let redirect = if let Err(ringline_redis::Error::Redis(ref msg)) = result {
                parse_resp_redirect(msg)
            } else {
                None
            };
            let backfill = user_data & BACKFILL_MARKER != 0;
            RequestResult {
                id: 0,
                success,
                is_error_response: is_error,
                latency_ns: 0,
                ttfb_ns: None,
                request_type: RequestType::Set,
                hit: None,
                key_id: None,
                backfill,
                redirect,
            }
        }
        ringline_redis::CompletedOp::Del {
            result,
            user_data: _,
        } => {
            let (success, is_error) = match &result {
                Ok(_) => (true, false),
                Err(_) => (false, true),
            };
            let redirect = if let Err(ringline_redis::Error::Redis(ref msg)) = result {
                parse_resp_redirect(msg)
            } else {
                None
            };
            RequestResult {
                id: 0,
                success,
                is_error_response: is_error,
                latency_ns: 0,
                ttfb_ns: None,
                request_type: RequestType::Delete,
                hit: None,
                key_id: None,
                backfill: false,
                redirect,
            }
        }
    }
}

/// Parse a Redis error message for MOVED/ASK redirect.
fn parse_resp_redirect(msg: &str) -> Option<resp_proto::Redirect> {
    let (kind, rest) = if let Some(rest) = msg.strip_prefix("MOVED ") {
        (resp_proto::RedirectKind::Moved, rest)
    } else if let Some(rest) = msg.strip_prefix("ASK ") {
        (resp_proto::RedirectKind::Ask, rest)
    } else {
        return None;
    };

    let mut parts = rest.splitn(2, ' ');
    let slot: u16 = parts.next()?.parse().ok()?;
    let address = parts.next()?;

    Some(resp_proto::Redirect {
        kind,
        slot,
        address: address.to_string(),
    })
}

/// Handle a parsed MOVED redirect by updating the slot table.
fn check_resp_redirect_parsed(redirect: &resp_proto::Redirect, state: &Arc<SharedWorkerState>) {
    metrics::CLUSTER_REDIRECTS.increment();

    if redirect.kind != resp_proto::RedirectKind::Moved {
        return;
    }

    if let Ok(addr) = redirect.address.parse::<SocketAddr>() {
        let mut slot_table = state.task_state.slot_table.lock().unwrap();
        if let Some(ref mut table) = *slot_table {
            let endpoint_idx = state.task_state.endpoints.iter().position(|a| *a == addr);
            if let Some(idx) = endpoint_idx {
                table[redirect.slot as usize] = idx as u16;
            } else {
                tracing::warn!(
                    worker = state.task_state.worker_id,
                    addr = %addr,
                    slot = redirect.slot,
                    "MOVED redirect to unknown endpoint (cannot add dynamically)"
                );
            }
        }
    }
}

// ── Memcache connection task ─────────────────────────────────────────────

/// A single Memcache (ASCII) connection task.
async fn memcache_connection_task(state: Arc<SharedWorkerState>, endpoint_idx: usize, seed: u64) {
    let endpoint = state.task_state.endpoints[endpoint_idx];
    let config = &state.task_state.config;
    let tls_name = resolve_tls_server_name(&state.task_state, endpoint);
    let mut rng = Xoshiro256PlusPlus::seed_from_u64(seed);
    let mut key_buf = vec![0u8; config.workload.keyspace.length];
    let mut backfill_queue: Vec<usize> = Vec::new();

    loop {
        let phase = state.task_state.shared.phase();
        if phase.should_stop() {
            return;
        }

        let conn =
            match establish_connection(endpoint, state.task_state.worker_id, tls_name.as_deref())
                .await
            {
                Ok(conn) => conn,
                Err(_) => {
                    ringline::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };

        metrics::CONNECTIONS_ACTIVE.increment();
        let use_kernel_ts = matches!(config.timestamps.mode, TimestampMode::Software);
        let mut client = ringline_memcache::Client::builder(conn)
            .on_result(make_memcache_callback())
            .kernel_timestamps(use_kernel_ts)
            .build();

        tracing::debug!(
            worker = state.task_state.worker_id,
            endpoint = %endpoint,
            conn_index = conn.index(),
            "connected (Memcache)"
        );

        // Precheck: send VERSION to verify connectivity
        if state.task_state.shared.phase() == Phase::Precheck && !state.is_precheck_done() {
            match client.version().await {
                Ok(_version) => {
                    tracing::debug!(
                        worker = state.task_state.worker_id,
                        endpoint = %endpoint,
                        "precheck VERSION ok"
                    );
                    state.mark_precheck_done();
                }
                Err(e) => {
                    tracing::debug!(
                        worker = state.task_state.worker_id,
                        endpoint = %endpoint,
                        "precheck VERSION failed: {}",
                        e
                    );
                    metrics::CONNECTIONS_ACTIVE.decrement();
                    metrics::CONNECTIONS_FAILED.increment();
                    ringline::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            }
            // Wait for phase to advance past Precheck
            while state.task_state.shared.phase() == Phase::Precheck {
                if state.task_state.shared.phase().should_stop() {
                    metrics::CONNECTIONS_ACTIVE.decrement();
                    return;
                }
                ringline::sleep(Duration::from_millis(10)).await;
            }
        }

        let result = drive_memcache_workload(
            &mut client,
            &state,
            endpoint_idx,
            &mut rng,
            &mut key_buf,
            &mut backfill_queue,
        )
        .await;

        metrics::CONNECTIONS_ACTIVE.decrement();

        match result {
            Ok(()) => return,
            Err(reason) => {
                metrics::CONNECTIONS_FAILED.increment();
                record_disconnect_reason(reason);
            }
        }

        ringline::sleep(Duration::from_millis(100)).await;
    }
}

/// Drive the Memcache workload on a connected client using fire/recv pipelining.
async fn drive_memcache_workload(
    client: &mut ringline_memcache::Client,
    state: &Arc<SharedWorkerState>,
    endpoint_idx: usize,
    rng: &mut Xoshiro256PlusPlus,
    key_buf: &mut [u8],
    backfill_queue: &mut Vec<usize>,
) -> Result<(), DisconnectReason> {
    let config = &state.task_state.config;
    let key_count = config.workload.keyspace.count;
    let get_ratio = config.workload.commands.get as usize;
    let delete_ratio = config.workload.commands.delete as usize;
    let value_len = config.workload.values.length;
    let pool_len = state.task_state.value_pool.len();
    let backfill_on_miss = state.task_state.backfill_on_miss;
    let multi_endpoint = state.task_state.endpoints.len() > 1;
    let pipeline_depth = config.connection.pipeline_depth;

    loop {
        let phase = state.task_state.shared.phase();
        if phase.should_stop() {
            return Ok(());
        }

        // Fill pipeline
        if phase == Phase::Prefill && !state.is_prefill_done() {
            while client.pending_count() < pipeline_depth {
                let key_id = {
                    let mut queue = state.task_state.prefill_queue.lock().unwrap();
                    match queue.pop_front() {
                        Some(id) => id,
                        None => break,
                    }
                };

                write_key(key_buf, key_id);
                let guard =
                    make_value_guard(rng, &state.task_state.value_pool, value_len, pool_len);

                match client.fire_set_with_guard(key_buf, guard, 0, 0, 0) {
                    Ok(_) => {
                        metrics::REQUESTS_SENT.increment();
                    }
                    Err(_) => {
                        let mut queue = state.task_state.prefill_queue.lock().unwrap();
                        queue.push_front(key_id);
                        break;
                    }
                }
            }
        } else if phase == Phase::Warmup || phase == Phase::Running {
            while client.pending_count() < pipeline_depth {
                // Drain backfill queue first
                if let Some(key_id) = backfill_queue.pop() {
                    write_key(key_buf, key_id);
                    let guard =
                        make_value_guard(rng, &state.task_state.value_pool, value_len, pool_len);

                    if let Some(ref rl) = state.task_state.ratelimiter
                        && !rl.try_acquire()
                    {
                        backfill_queue.push(key_id);
                        break;
                    }

                    let user_data = key_id as u64 | BACKFILL_MARKER;
                    match client.fire_set_with_guard(key_buf, guard, 0, 0, user_data) {
                        Ok(_) => {
                            metrics::REQUESTS_SENT.increment();
                        }
                        Err(_) => {
                            backfill_queue.push(key_id);
                            break;
                        }
                    }
                    continue;
                }

                // Rate limiting
                if let Some(ref rl) = state.task_state.ratelimiter
                    && !rl.try_acquire()
                {
                    break;
                }

                // Generate random key
                let key_id = rng.random_range(0..key_count);
                write_key(key_buf, key_id);

                // Multi-endpoint routing
                if multi_endpoint {
                    let routed = route_key(&state.task_state, key_buf);
                    if routed != endpoint_idx {
                        continue;
                    }
                }

                // Choose command
                let roll = rng.random_range(0..100);
                let sent = if roll < get_ratio {
                    client.fire_get(key_buf, key_id as u64).is_ok()
                } else if roll < get_ratio + delete_ratio {
                    client.fire_delete(key_buf, 0).is_ok()
                } else {
                    let guard =
                        make_value_guard(rng, &state.task_state.value_pool, value_len, pool_len);
                    client.fire_set_with_guard(key_buf, guard, 0, 0, 0).is_ok()
                };

                if sent {
                    metrics::REQUESTS_SENT.increment();
                } else {
                    break;
                }
            }
        }

        // Wait for one response
        if client.pending_count() == 0 {
            continue;
        }

        let op = match client.recv().await {
            Ok(op) => op,
            Err(ringline_memcache::Error::ConnectionClosed) => {
                return Err(DisconnectReason::Eof);
            }
            Err(_) => {
                return Err(DisconnectReason::RecvError);
            }
        };

        // Map CompletedOp to RequestResult
        let result = map_memcache_op(op);

        // Handle prefill tracking
        if phase == Phase::Prefill
            && !state.is_prefill_done()
            && result.request_type == RequestType::Set
            && result.success
        {
            confirm_prefill_key(state);
        }

        // Handle backfill-on-miss
        if backfill_on_miss
            && result.request_type == RequestType::Get
            && result.success
            && result.hit == Some(false)
            && let Some(key_id) = result.key_id
        {
            backfill_queue.push(key_id);
        }

        // Handle backfill SET tracking
        if result.backfill && result.request_type == RequestType::Set && result.success {
            metrics::BACKFILL_SET_COUNT.increment();
        }

        // Record counter metrics
        record_counters(&result);
    }
}

/// Map a ringline-memcache CompletedOp to a RequestResult.
fn map_memcache_op(op: ringline_memcache::CompletedOp) -> RequestResult {
    match op {
        ringline_memcache::CompletedOp::Get { result, user_data } => {
            let (success, is_error, hit) = match &result {
                Ok(Some(_)) => (true, false, Some(true)),
                Ok(None) => (true, false, Some(false)),
                Err(_) => (false, true, None),
            };
            RequestResult {
                id: 0,
                success,
                is_error_response: is_error,
                latency_ns: 0,
                ttfb_ns: None,
                request_type: RequestType::Get,
                hit,
                key_id: Some(user_data as usize),
                backfill: false,
                redirect: None,
            }
        }
        ringline_memcache::CompletedOp::Set { result, user_data } => {
            let (success, is_error) = match &result {
                Ok(()) => (true, false),
                Err(_) => (false, true),
            };
            let backfill = user_data & BACKFILL_MARKER != 0;
            RequestResult {
                id: 0,
                success,
                is_error_response: is_error,
                latency_ns: 0,
                ttfb_ns: None,
                request_type: RequestType::Set,
                hit: None,
                key_id: None,
                backfill,
                redirect: None,
            }
        }
        ringline_memcache::CompletedOp::Delete {
            result,
            user_data: _,
        } => {
            let (success, is_error) = match &result {
                Ok(_) => (true, false),
                Err(_) => (false, true),
            };
            RequestResult {
                id: 0,
                success,
                is_error_response: is_error,
                latency_ns: 0,
                ttfb_ns: None,
                request_type: RequestType::Delete,
                hit: None,
                key_id: None,
                backfill: false,
                redirect: None,
            }
        }
    }
}

// ── Ping connection task ─────────────────────────────────────────────────

/// A single Ping protocol connection task.
async fn ping_connection_task(state: Arc<SharedWorkerState>, endpoint_idx: usize, _seed: u64) {
    let endpoint = state.task_state.endpoints[endpoint_idx];
    let config = &state.task_state.config;
    let tls_name = resolve_tls_server_name(&state.task_state, endpoint);

    loop {
        let phase = state.task_state.shared.phase();
        if phase.should_stop() {
            return;
        }

        let conn =
            match establish_connection(endpoint, state.task_state.worker_id, tls_name.as_deref())
                .await
            {
                Ok(conn) => conn,
                Err(_) => {
                    ringline::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };

        metrics::CONNECTIONS_ACTIVE.increment();
        let use_kernel_ts = matches!(config.timestamps.mode, TimestampMode::Software);
        let mut client = ringline_ping::Client::builder(conn)
            .on_result(make_ping_callback())
            .kernel_timestamps(use_kernel_ts)
            .build();

        tracing::debug!(
            worker = state.task_state.worker_id,
            endpoint = %endpoint,
            conn_index = conn.index(),
            "connected (Ping)"
        );

        // Precheck: send PING to verify connectivity
        if state.task_state.shared.phase() == Phase::Precheck && !state.is_precheck_done() {
            match client.ping().await {
                Ok(()) => {
                    tracing::debug!(
                        worker = state.task_state.worker_id,
                        endpoint = %endpoint,
                        "precheck PING ok (ping protocol)"
                    );
                    state.mark_precheck_done();
                }
                Err(e) => {
                    tracing::debug!(
                        worker = state.task_state.worker_id,
                        endpoint = %endpoint,
                        "precheck PING failed (ping protocol): {}",
                        e
                    );
                    metrics::CONNECTIONS_ACTIVE.decrement();
                    metrics::CONNECTIONS_FAILED.increment();
                    ringline::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            }
            // Wait for phase to advance past Precheck
            while state.task_state.shared.phase() == Phase::Precheck {
                if state.task_state.shared.phase().should_stop() {
                    metrics::CONNECTIONS_ACTIVE.decrement();
                    return;
                }
                ringline::sleep(Duration::from_millis(10)).await;
            }
        }

        let result = drive_ping_workload(&mut client, &state).await;

        metrics::CONNECTIONS_ACTIVE.decrement();

        match result {
            Ok(()) => return,
            Err(reason) => {
                metrics::CONNECTIONS_FAILED.increment();
                record_disconnect_reason(reason);
            }
        }

        ringline::sleep(Duration::from_millis(100)).await;
    }
}

/// Drive the Ping workload: send PING, parse PONG, repeat.
async fn drive_ping_workload(
    client: &mut ringline_ping::Client,
    state: &Arc<SharedWorkerState>,
) -> Result<(), DisconnectReason> {
    loop {
        let phase = state.task_state.shared.phase();
        if phase.should_stop() {
            return Ok(());
        }

        // Skip Connect/Prefill phases (no prefill for ping)
        if phase != Phase::Warmup && phase != Phase::Running {
            // Mark prefill as done if in Prefill phase
            if phase == Phase::Prefill
                && !state.is_prefill_done()
                && state
                    .prefill_done
                    .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            {
                state.task_state.shared.mark_prefill_complete();
            }
            continue;
        }

        // Rate limiting
        if let Some(ref rl) = state.task_state.ratelimiter
            && !rl.try_acquire()
        {
            continue;
        }

        metrics::REQUESTS_SENT.increment();
        match client.ping().await {
            Ok(()) => {}
            Err(ringline_ping::Error::ConnectionClosed) => {
                return Err(DisconnectReason::Eof);
            }
            Err(_) => {}
        }
    }
}

// ── Momento connection task ──────────────────────────────────────────────

/// A single Momento connection task using ringline-momento.
async fn momento_connection_task(state: Arc<SharedWorkerState>, _conn_idx: usize, seed: u64) {
    let config = &state.task_state.config;
    let credential = {
        let setup_guard = state.task_state.momento_setup.lock().unwrap();
        let setup = setup_guard.as_ref().unwrap();
        setup.credential.clone()
    };

    let mut rng = Xoshiro256PlusPlus::seed_from_u64(seed);
    let mut key_buf = vec![0u8; config.workload.keyspace.length];
    let mut value_buf = vec![0u8; config.workload.values.length];

    // Fill value buffer with random data
    let mut init_rng = Xoshiro256PlusPlus::seed_from_u64(42);
    init_rng.fill_bytes(&mut value_buf);

    loop {
        let phase = state.task_state.shared.phase();
        if phase.should_stop() {
            return;
        }

        // Connect via ringline-momento (handles TLS + auth internally)
        let authenticated = match ringline_momento::Client::connect(&credential).await {
            Ok(client) => client,
            Err(e) => {
                // During precheck, surface connection errors at warn level so they're
                // visible without RUST_LOG=debug
                if phase == Phase::Precheck {
                    tracing::warn!(
                        worker = state.task_state.worker_id,
                        "Momento connect failed: {}",
                        e
                    );
                } else {
                    tracing::debug!(
                        worker = state.task_state.worker_id,
                        "Momento connect failed: {}",
                        e
                    );
                }
                metrics::CONNECTIONS_FAILED.increment();
                ringline::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };

        // Rebuild with on_result callback + kernel timestamps for latency recording
        let use_kernel_ts = matches!(config.timestamps.mode, TimestampMode::Software);
        let mut client = ringline_momento::Client::builder(authenticated.conn())
            .on_result(make_momento_callback())
            .kernel_timestamps(use_kernel_ts)
            .build();

        metrics::CONNECTIONS_ACTIVE.increment();

        tracing::debug!(worker = state.task_state.worker_id, "Momento connected");

        // Precheck: successful connect already proves connectivity for Momento
        if state.task_state.shared.phase() == Phase::Precheck && !state.is_precheck_done() {
            tracing::debug!(
                worker = state.task_state.worker_id,
                "precheck ok (Momento connect succeeded)"
            );
            state.mark_precheck_done();
            // Wait for phase to advance past Precheck
            while state.task_state.shared.phase() == Phase::Precheck {
                if state.task_state.shared.phase().should_stop() {
                    metrics::CONNECTIONS_ACTIVE.decrement();
                    return;
                }
                ringline::sleep(Duration::from_millis(10)).await;
            }
        }

        // Drive workload
        let result =
            drive_momento_session(&mut client, &state, &mut rng, &mut key_buf, &mut value_buf)
                .await;

        metrics::CONNECTIONS_ACTIVE.decrement();

        match result {
            Ok(()) => return, // Clean shutdown
            Err(_) => {
                metrics::CONNECTIONS_FAILED.increment();
                metrics::DISCONNECTS_CLOSED_EVENT.increment();
            }
        }

        ringline::sleep(Duration::from_millis(100)).await;
    }
}

/// Drive the Momento workload on a connected session.
async fn drive_momento_session(
    client: &mut ringline_momento::Client,
    state: &Arc<SharedWorkerState>,
    rng: &mut Xoshiro256PlusPlus,
    key_buf: &mut [u8],
    value_buf: &mut [u8],
) -> Result<(), DisconnectReason> {
    let config = &state.task_state.config;
    let key_count = config.workload.keyspace.count;
    let get_ratio = config.workload.commands.get as usize;
    let delete_ratio = config.workload.commands.delete as usize;
    let pipeline_depth = config.connection.pipeline_depth;
    let cache_name = &config.momento.cache_name;
    let ttl_ms = config.momento.ttl_seconds * 1000;

    loop {
        let phase = state.task_state.shared.phase();
        if phase.should_stop() {
            return Ok(());
        }

        // Fill pipeline
        if phase == Phase::Prefill && !state.is_prefill_done() {
            while client.pending_count() < pipeline_depth {
                let key_id = {
                    let mut queue = state.task_state.prefill_queue.lock().unwrap();
                    match queue.pop_front() {
                        Some(id) => id,
                        None => break,
                    }
                };

                write_key(key_buf, key_id);
                rng.fill_bytes(value_buf);

                match client.fire_set(cache_name, key_buf, value_buf, ttl_ms) {
                    Ok(_) => {
                        metrics::REQUESTS_SENT.increment();
                    }
                    Err(_) => {
                        let mut queue = state.task_state.prefill_queue.lock().unwrap();
                        queue.push_front(key_id);
                        break;
                    }
                }
            }
        } else if phase == Phase::Warmup || phase == Phase::Running {
            while client.pending_count() < pipeline_depth {
                if let Some(ref rl) = state.task_state.ratelimiter
                    && !rl.try_acquire()
                {
                    break;
                }

                let key_id = rng.random_range(0..key_count);
                write_key(key_buf, key_id);

                let roll = rng.random_range(0..100);
                let sent = if roll < get_ratio {
                    client.fire_get(cache_name, key_buf).is_ok()
                } else if roll < get_ratio + delete_ratio {
                    client.fire_delete(cache_name, key_buf).is_ok()
                } else {
                    rng.fill_bytes(value_buf);
                    client
                        .fire_set(cache_name, key_buf, value_buf, ttl_ms)
                        .is_ok()
                };

                if sent {
                    metrics::REQUESTS_SENT.increment();
                } else {
                    break;
                }
            }
        }

        // Wait for one response
        if client.pending_count() == 0 {
            continue;
        }

        let op = match client.recv().await {
            Ok(op) => op,
            Err(_) => {
                return Err(DisconnectReason::Eof);
            }
        };

        // Map CompletedOp to RequestResult
        let result = map_momento_op(op);

        // Handle prefill tracking
        if phase == Phase::Prefill
            && !state.is_prefill_done()
            && result.request_type == RequestType::Set
            && result.success
        {
            let results = std::slice::from_ref(&result);
            handle_momento_prefill_results(results, state);
        }

        // Record counter metrics (latency is recorded by the on_result callback)
        record_counters(&result);
    }
}

/// Map a ringline-momento CompletedOp to a RequestResult.
fn map_momento_op(op: ringline_momento::CompletedOp) -> RequestResult {
    match op {
        ringline_momento::CompletedOp::Get { id, result, .. } => {
            let (success, is_error, hit) = match result {
                Ok(Some(_)) => (true, false, Some(true)),
                Ok(None) => (true, false, Some(false)),
                Err(_) => (false, true, None),
            };
            RequestResult {
                id: id.value(),
                success,
                is_error_response: is_error,
                latency_ns: 0,
                ttfb_ns: None,
                request_type: RequestType::Get,
                hit,
                key_id: None,
                backfill: false,
                redirect: None,
            }
        }
        ringline_momento::CompletedOp::Set { id, result, .. } => {
            let (success, is_error) = match result {
                Ok(()) => (true, false),
                Err(_) => (false, true),
            };
            RequestResult {
                id: id.value(),
                success,
                is_error_response: is_error,
                latency_ns: 0,
                ttfb_ns: None,
                request_type: RequestType::Set,
                hit: None,
                key_id: None,
                backfill: false,
                redirect: None,
            }
        }
        ringline_momento::CompletedOp::Delete { id, result, .. } => {
            let (success, is_error) = match result {
                Ok(()) => (true, false),
                Err(_) => (false, true),
            };
            RequestResult {
                id: id.value(),
                success,
                is_error_response: is_error,
                latency_ns: 0,
                ttfb_ns: None,
                request_type: RequestType::Delete,
                hit: None,
                key_id: None,
                backfill: false,
                redirect: None,
            }
        }
    }
}

/// Handle Momento prefill response tracking.
fn handle_momento_prefill_results(results: &[RequestResult], state: &Arc<SharedWorkerState>) {
    if state.is_prefill_done() {
        return;
    }

    let mut confirmed_batch = 0usize;
    for result in results {
        if result.request_type == RequestType::Set && result.success {
            confirmed_batch += 1;
        }
    }

    if confirmed_batch > 0 {
        let new_total = state
            .task_state
            .shared
            .prefill_keys_confirmed
            .fetch_add(confirmed_batch, Ordering::AcqRel)
            + confirmed_batch;

        if new_total >= state.prefill_total
            && state.prefill_total > 0
            && state
                .prefill_done
                .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            tracing::debug!(
                worker_id = state.task_state.worker_id,
                confirmed = new_total,
                total = state.prefill_total,
                "prefill complete (momento)"
            );
            state.task_state.shared.mark_prefill_complete();
        }
    }
}

// ── Prefill helpers ──────────────────────────────────────────────────────

/// Confirm a single prefill key was successfully stored.
fn confirm_prefill_key(state: &Arc<SharedWorkerState>) {
    let new_total = state
        .task_state
        .shared
        .prefill_keys_confirmed
        .fetch_add(1, Ordering::AcqRel)
        + 1;

    if new_total >= state.prefill_total
        && state.prefill_total > 0
        && state
            .prefill_done
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    {
        tracing::debug!(
            worker_id = state.task_state.worker_id,
            confirmed = new_total,
            total = state.prefill_total,
            "prefill complete"
        );
        state.task_state.shared.mark_prefill_complete();
    }
}

// ── Utility functions ────────────────────────────────────────────────────

/// Record disconnect reason metrics.
fn record_disconnect_reason(reason: DisconnectReason) {
    match reason {
        DisconnectReason::Eof => metrics::DISCONNECTS_EOF.increment(),
        DisconnectReason::RecvError => metrics::DISCONNECTS_RECV_ERROR.increment(),
        DisconnectReason::SendError => metrics::DISCONNECTS_SEND_ERROR.increment(),
        DisconnectReason::ClosedEvent => metrics::DISCONNECTS_CLOSED_EVENT.increment(),
        DisconnectReason::ErrorEvent => metrics::DISCONNECTS_ERROR_EVENT.increment(),
        DisconnectReason::ConnectFailed => metrics::DISCONNECTS_CONNECT_FAILED.increment(),
    }
}

/// Record counter metrics for a completed request result (always called).
fn record_counters(result: &RequestResult) {
    metrics::RESPONSES_RECEIVED.increment();
    if result.redirect.is_some() {
        // Redirects are counted via CLUSTER_REDIRECTS, not as errors
    } else if result.is_error_response {
        metrics::REQUEST_ERRORS.increment();
    }
    if let Some(hit) = result.hit {
        if hit {
            metrics::CACHE_HITS.increment();
        } else {
            metrics::CACHE_MISSES.increment();
        }
    }
    match result.request_type {
        RequestType::Get => metrics::GET_COUNT.increment(),
        RequestType::Set => metrics::SET_COUNT.increment(),
        RequestType::Delete => metrics::DELETE_COUNT.increment(),
        RequestType::Ping | RequestType::Other => {}
    }
}

/// Route a key to an endpoint index.
fn route_key(state: &TaskSharedState, key: &[u8]) -> usize {
    let slot_table = state.slot_table.lock().unwrap();
    if let Some(ref table) = *slot_table {
        let slot = resp_proto::hash_slot(key);
        table[slot as usize] as usize
    } else if state.endpoints.len() == 1 {
        0
    } else {
        state.ring.route(key)
    }
}

/// Write a numeric key ID into the buffer as hex.
fn write_key(buf: &mut [u8], id: usize) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut n = id;
    for byte in buf.iter_mut().rev() {
        *byte = HEX[n & 0xf];
        n >>= 4;
    }
}

/// Pin the current thread to a specific CPU core.
#[cfg(target_os = "linux")]
fn pin_to_cpu(cpu_id: usize) -> std::io::Result<()> {
    use std::mem;

    unsafe {
        let mut cpuset: libc::cpu_set_t = mem::zeroed();
        libc::CPU_ZERO(&mut cpuset);
        libc::CPU_SET(cpu_id, &mut cpuset);

        let result = libc::sched_setaffinity(0, mem::size_of::<libc::cpu_set_t>(), &cpuset);

        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}
