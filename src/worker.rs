//! Worker implementation using ringline AsyncEventHandler.
//!
//! Each worker runs inside a ringline event loop. On startup, `on_start()` spawns
//! one async task per connection. Each task independently connects, drives
//! requests, parses responses, and reconnects on failure. `on_tick()` handles
//! phase transitions, diagnostics, and shutdown.

use crate::client::{RequestResult, RequestType};
#[cfg(target_os = "linux")]
use crate::config::TimestampMode;
use crate::config::{Config, Protocol as CacheProtocol};
use crate::keydist::KeyDist;
use crate::metrics;
use ratelimit::{Ratelimiter, TryWaitError};

use ringline::{AsyncEventHandler, ConnCtx, DriverCtx, RegionId, SendGuard};

use rand::prelude::*;
use rand_xoshiro::Xoshiro256PlusPlus;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock, RwLock};
use std::task::{Context, Poll, Waker};
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
    /// Warmup phase - run workload; counter baselines are captured at the end so only Running phase data is reported
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
    /// A request outlived `connection.request_timeout`; the connection was
    /// closed because replies are ordered and a stalled head blocks them all.
    Timeout,
}

/// Shared state between workers and main thread.
pub struct SharedState {
    /// Current test phase (controlled by main thread)
    phase: AtomicU8,
    /// Number of workers whose event loops have started (on_start called)
    workers_started: AtomicUsize,
    /// Number of workers that have completed precheck
    precheck_done: AtomicUsize,
    /// Number of workers that have completed prefill
    prefill_complete: AtomicUsize,
    /// Total number of prefill keys confirmed across all workers
    prefill_keys_confirmed: AtomicUsize,
    /// Total number of prefill keys assigned across all workers
    prefill_keys_total: AtomicUsize,
    /// Append stream: keys confirmed written by writer connections (cumulative).
    append_keys_confirmed: AtomicUsize,
    /// Append stream: keys enqueued so far (cumulative). A batch is complete
    /// when confirmed catches up with this.
    append_keys_total: AtomicUsize,
}

impl SharedState {
    pub fn new() -> Self {
        Self {
            phase: AtomicU8::new(Phase::Connect as u8),
            workers_started: AtomicUsize::new(0),
            precheck_done: AtomicUsize::new(0),
            prefill_complete: AtomicUsize::new(0),
            prefill_keys_confirmed: AtomicUsize::new(0),
            prefill_keys_total: AtomicUsize::new(0),
            append_keys_confirmed: AtomicUsize::new(0),
            append_keys_total: AtomicUsize::new(0),
        }
    }

    /// Append stream: keys confirmed written so far.
    pub fn append_keys_confirmed(&self) -> usize {
        self.append_keys_confirmed.load(Ordering::Acquire)
    }

    /// Append stream: keys enqueued so far.
    pub fn append_keys_total(&self) -> usize {
        self.append_keys_total.load(Ordering::Acquire)
    }

    /// Append stream: account for a batch of `n` keys just enqueued.
    pub fn add_append_total(&self, n: usize) {
        self.append_keys_total.fetch_add(n, Ordering::Release);
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

    /// Mark one worker's event loop as started.
    #[inline]
    pub fn mark_worker_started(&self) {
        self.workers_started.fetch_add(1, Ordering::Release);
    }

    /// Get the number of workers whose event loops have started.
    #[inline]
    pub fn workers_started(&self) -> usize {
        self.workers_started.load(Ordering::Acquire)
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
    pub ratelimiter: Option<Arc<Ratelimiter>>,
    /// Whether to record metrics (only true during Running phase)
    pub recording: bool,
    /// Shared per-endpoint prefill queues (full keyspace, built once by the
    /// runner). All workers share the same `Arc`.
    pub(crate) prefill_queues: Arc<PrefillQueues>,
    /// Shared per-endpoint append queues (see `TaskSharedState::append_queues`).
    pub(crate) append_queues: Arc<PrefillQueues>,
    /// Writer-side rate limiter for `append.pace = "spread"`.
    pub append_ratelimiter: Option<Arc<Ratelimiter>>,
    /// Per-endpoint key-id lists for steady-state key selection, SHARED across
    /// all workers (built once by the runner). `endpoint_keys[ep]` is the set of
    /// key-ids whose slot is owned by endpoint `ep`, so a connection task can
    /// pick a key for its endpoint with an O(1) random index instead of
    /// rejection-sampling random keys and re-routing each one (which made CRC16
    /// slot routing ~40% of CPU at high pipeline depth). Empty for the
    /// single-endpoint case (callers fall back to a plain random key-id).
    pub(crate) endpoint_keys: Arc<Vec<Vec<u32>>>,
    /// CPU IDs for pinning (if configured).
    pub cpu_ids: Option<Vec<usize>>,
    /// Shared 1GB random value pool (all workers share the same Arc).
    pub value_pool: Arc<Vec<u8>>,
    /// Cluster mode: slot → endpoint index (16384 entries). None = ketama routing.
    pub slot_table: Option<Vec<u16>>,
    /// Steady-state key-id distribution, built ONCE by the runner and shared by
    /// every worker. Zipf setup is O(keyspace) so it must not be per-worker.
    pub(crate) key_dist: Arc<KeyDist>,
}

// ── Config channel ────────────────────────────────────────────────────────

static CONFIG_CHANNEL: OnceLock<crossbeam_channel::Receiver<BenchWorkerConfig>> = OnceLock::new();

/// Initialize the global config channel. Must be called before launch.
pub fn init_config_channel(rx: crossbeam_channel::Receiver<BenchWorkerConfig>) {
    CONFIG_CHANNEL
        .set(rx)
        .expect("init_config_channel called twice");
}

fn recv_config() -> BenchWorkerConfig {
    CONFIG_CHANNEL
        .get()
        .expect("config channel not initialized")
        .recv()
        .expect("config channel closed")
}

// ── ValuePoolGuard ────────────────────────────────────────────────────────

/// Zero-copy send guard referencing a slice of the shared value pool.
///
/// When passed to `fire_set_with_guard()`, the ringline layer can send directly
/// from the pool memory without copying. The `Arc<Vec<u8>>` keeps the pool
/// alive until the send completes.
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
    debug_assert!(
        value_len > 0 && value_len <= pool_len,
        "value_len ({value_len}) must be in 1..={pool_len}"
    );
    let max_offset = pool_len - value_len;
    let offset = rng.random_range(0..=max_offset);
    ValuePoolGuard {
        pool: Arc::clone(value_pool),
        offset: offset as u32,
        len: value_len as u32,
    }
}

/// Minimum value size at which a zero-copy `SendGuard` (SendMsgZc) send is
/// worth it. Each ZC send costs two CQEs (send completion + ZC notification),
/// pins the buffer (`get_user_pages`), and holds an in-flight slab slot until
/// the notification lands. For small values that overhead dwarfs a memcpy and
/// caps pipelined SET throughput, so below this threshold we use the
/// copy-based `fire_set` path (one CQE, no pinning) instead. ZC only pays off
/// once the value is large enough to amortize the per-send cost.
const ZC_VALUE_THRESHOLD: usize = 4096;

// ── Rate-limit dispatch ─────────────────────────────────────────────────

/// Fallback poll interval, and what a closed-loop run uses.
///
/// Without a rate limiter a connection only reaches the idle path when a `fire`
/// fails on backpressure, so this is rarely taken and costs ~0.6 us of user CPU
/// per request. Also the safety bound on a dispatcher wait, so a connection can
/// never hang if the dispatcher task dies.
const IDLE_SLEEP_MIN: Duration = Duration::from_micros(100);

/// Upper bound on a single dispatcher wait, and on how long the dispatcher
/// itself sleeps when no connection wants a token.
const IDLE_SLEEP_MAX: Duration = Duration::from_millis(50);

/// One connection's claim on the rate limiter.
///
/// The dispatcher funds a slot outright or not at all, then wakes it. Granting
/// to a named slot rather than into a shared pool is what keeps the dispatcher
/// from spinning: once the head of the queue cannot be funded, there is nothing
/// to do until the limiter mints more, so it sleeps for exactly that long.
struct TokenSlot {
    want: u64,
    granted: Cell<u64>,
    waker: RefCell<Option<Waker>>,
}

/// Worker-local dispatcher between the shared `Ratelimiter` and this worker's
/// connection tasks.
///
/// Connections used to poll the limiter individually. That made the wakeup rate
/// `connections / sleep`, and since offered-load smoothness is *also*
/// `connections / sleep` -- how often anybody checks for a token -- CPU cost and
/// measurement fidelity were the same quantity and could only be traded against
/// each other. Measured at 2048 connections and 20,000 req/s: cutting the
/// wakeup rate eightfold halved CPU and took p99 from ~270 us to 376-1868 us,
/// with schedule slip p99 rising 100 us -> 803 us.
///
/// One task per worker polls the limiter instead, so wakeups scale with the
/// token rate rather than the connection count, and the cadence that governs
/// smoothness is set by the dispatcher alone. The two quantities come apart.
#[derive(Default)]
struct TokenDispatcher {
    queue: RefCell<VecDeque<Rc<TokenSlot>>>,
    /// Set once the dispatcher has stopped servicing. Sticky, because `drain`
    /// can only release the waiters queued at that instant -- a connection
    /// arriving afterwards would otherwise queue behind a dispatcher that is
    /// gone and wait forever, the wait being untimed by design.
    closed: Cell<bool>,
}

impl TokenDispatcher {
    /// Queue a claim for `want` tokens and wait for the dispatcher to fund it.
    ///
    /// Deliberately untimed. An earlier version wrapped this in
    /// `ringline::timeout` as a liveness guard, which armed and cancelled a
    /// timer on *every* acquire -- reintroducing the per-request timer the
    /// dispatcher exists to remove, and costing more CPU than the polling it
    /// replaced. Liveness comes from the dispatcher instead: it re-checks the
    /// phase at least every `IDLE_SLEEP_MAX` and `drain`s every waiter when the
    /// run stops, and the release profile is `panic = "abort"`, so a dispatcher
    /// that failed would take the process with it rather than strand anyone.
    async fn acquire(self: &Rc<Self>, want: u64) -> u64 {
        if self.closed.get() {
            return 0;
        }
        let slot = Rc::new(TokenSlot {
            want,
            granted: Cell::new(0),
            waker: RefCell::new(None),
        });
        self.queue.borrow_mut().push_back(Rc::clone(&slot));
        TokenWait {
            slot: Rc::clone(&slot),
        }
        .await;
        slot.granted.get()
    }

    /// Fund as many queued claims as `rl` will pay for right now.
    ///
    /// Returns how long to wait before trying again: `None` when the queue is
    /// empty, otherwise the limiter's own estimate of when the head becomes
    /// fundable.
    fn dispatch(&self, rl: &Ratelimiter) -> Option<Duration> {
        loop {
            let slot = self.queue.borrow_mut().pop_front()?;
            let want = slot.want.min(rl.max_tokens()).max(1);
            match rl.try_wait_n(want) {
                Ok(()) => {
                    slot.granted.set(want);
                    if let Some(w) = slot.waker.borrow_mut().take() {
                        w.wake();
                    }
                }
                // The head cannot be funded, so nothing behind it can be
                // either -- the queue is FIFO and service is in order. Put it
                // back; it keeps its place.
                Err(TryWaitError::Insufficient(d)) => {
                    self.queue.borrow_mut().push_front(slot);
                    return Some(d);
                }
                // `want` exceeds the bucket outright; waiting cannot fix it.
                // Fail the claim rather than stall the whole queue behind it.
                Err(TryWaitError::ExceedsCapacity) => {
                    if let Some(w) = slot.waker.borrow_mut().take() {
                        w.wake();
                    }
                }
                // `TryWaitError` is `#[non_exhaustive]`. A future variant we do
                // not understand gets a bounded retry: dropping the claim could
                // spin the caller, and stalling forever would wedge the queue.
                Err(_) => {
                    self.queue.borrow_mut().push_front(slot);
                    return Some(IDLE_SLEEP_MIN);
                }
            }
        }
    }

    /// Release every waiter, funded or not, so shutdown cannot block on one.
    fn drain(&self) {
        self.closed.set(true);
        for slot in self.queue.borrow_mut().drain(..) {
            if let Some(w) = slot.waker.borrow_mut().take() {
                w.wake();
            }
        }
    }
}

struct TokenWait {
    slot: Rc<TokenSlot>,
}

impl Future for TokenWait {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.slot.granted.get() > 0 {
            return Poll::Ready(());
        }
        *self.slot.waker.borrow_mut() = Some(cx.waker().clone());
        Poll::Pending
    }
}

thread_local! {
    /// This worker's dispatcher. `None` for a closed-loop run, which has no
    /// limiter to dispatch from and never needed one.
    static DISPATCHER: RefCell<Option<Rc<TokenDispatcher>>> = const { RefCell::new(None) };
}

fn dispatcher() -> Option<Rc<TokenDispatcher>> {
    DISPATCHER.with(|d| d.borrow().clone())
}

/// Run this worker's dispatcher until the run stops.
async fn run_dispatcher(
    dispatch: Rc<TokenDispatcher>,
    rl: Arc<Ratelimiter>,
    shared: Arc<SharedState>,
) {
    loop {
        if shared.phase().should_stop() {
            dispatch.drain();
            return;
        }
        // `None` means nobody is waiting; there is no token to chase, so idle
        // at the coarse bound rather than spinning on an empty queue.
        let nap = dispatch.dispatch(&rl).unwrap_or(IDLE_SLEEP_MAX);
        ringline::sleep(nap.clamp(Duration::from_micros(1), IDLE_SLEEP_MAX)).await;
    }
}

/// Borrow a random `value_len`-byte slice of the value pool, for the
/// copy-based `fire_set` path (the bytes are copied into the send pool
/// synchronously, so the borrow only needs to outlive the fire call).
fn value_slice<'a>(
    rng: &mut Xoshiro256PlusPlus,
    value_pool: &'a Arc<Vec<u8>>,
    value_len: usize,
    pool_len: usize,
) -> &'a [u8] {
    debug_assert!(
        value_len > 0 && value_len <= pool_len,
        "value_len ({value_len}) must be in 1..={pool_len}"
    );
    let max_offset = pool_len - value_len;
    let offset = rng.random_range(0..=max_offset);
    &value_pool[offset..offset + value_len]
}

// ── Shared task state (Arc-wrapped, accessed by all connection tasks) ────

/// Per-endpoint prefill key queues for a single worker.
///
/// At init time, the worker's assigned key range is partitioned by routing
/// destination so a connection task to endpoint `i` draws only from
/// `queues[i]`. This avoids the routing-miss thrashing the original
/// single-shared-queue design suffered from in multi-endpoint setups, where
/// every connection had to lock-pop, route-check, lock-push-back on most
/// keys it saw.
///
/// For single-endpoint setups (no routing) there is exactly one queue at
/// index 0.
pub(crate) struct PrefillQueues {
    queues: Vec<Mutex<VecDeque<usize>>>,
}

impl PrefillQueues {
    fn new(num: usize) -> Self {
        let n = num.max(1);
        Self {
            queues: (0..n).map(|_| Mutex::new(VecDeque::new())).collect(),
        }
    }

    fn push_back(&self, endpoint_idx: usize, key_id: usize) {
        self.queues[endpoint_idx].lock().unwrap().push_back(key_id);
    }

    fn push_front(&self, endpoint_idx: usize, key_id: usize) {
        self.queues[endpoint_idx].lock().unwrap().push_front(key_id);
    }

    fn pop_front(&self, endpoint_idx: usize) -> Option<usize> {
        self.queues[endpoint_idx].lock().unwrap().pop_front()
    }

    fn is_empty(&self, endpoint_idx: usize) -> bool {
        self.queues[endpoint_idx].lock().unwrap().is_empty()
    }
}

/// State shared across all connection tasks spawned by a single worker.
struct TaskSharedState {
    config: Config,
    shared: Arc<SharedState>,
    ratelimiter: Option<Arc<Ratelimiter>>,
    value_pool: Arc<Vec<u8>>,
    endpoints: Vec<SocketAddr>,
    ring: ketama::Ring,
    slot_table: RwLock<Option<Vec<u16>>>,
    /// Per-endpoint prefill key queues, SHARED across all workers (one queue
    /// per endpoint, each internally locked). Built once in the runner with
    /// the full keyspace so any worker's connection-task can drain the queue
    /// for the endpoint it serves — required when connections-per-worker is
    /// fewer than the number of cluster nodes.
    prefill_queues: Arc<PrefillQueues>,
    /// Per-endpoint append queues, SHARED across all workers. The runner
    /// enqueues each batch; writer connection tasks drain the queue for the
    /// endpoint they serve. Empty queues when no append stream is configured.
    append_queues: Arc<PrefillQueues>,
    /// Writer-side rate limiter for `append.pace = "spread"`; `None` for
    /// burst pacing or no append stream.
    append_ratelimiter: Option<Arc<Ratelimiter>>,
    /// Per-endpoint key-id lists for steady-state key selection (see
    /// `BenchWorkerConfig::endpoint_keys`). Empty for single-endpoint setups.
    endpoint_keys: Arc<Vec<Vec<u32>>>,
    /// Worker ID for logging.
    worker_id: usize,
    /// Whether backfill_on_miss is enabled.
    backfill_on_miss: bool,
    /// Whether TLS is enabled for connections.
    tls_enabled: bool,
    /// SNI server name for TLS connections.
    tls_server_name: Option<String>,
    /// Steady-state key-id distribution (see `BenchWorkerConfig::key_dist`).
    key_dist: Arc<KeyDist>,
}

/// State shared between the BenchHandler (on_tick) and connection tasks.
/// Wrapped in Arc so on_tick and spawned tasks can both access it.
struct SharedWorkerState {
    task_state: Arc<TaskSharedState>,
    /// Prefill tracking: total keys assigned to this worker.
    prefill_total: usize,
    /// Number of prefill keys confirmed by this worker.
    prefill_confirmed: AtomicUsize,
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
            // Install this worker's dispatcher before any connection task can
            // look for it. A closed-loop run has no limiter and gets none, so
            // `dispatcher()` returns `None` and those tasks keep the old poll.
            if let Some(rl) = worker_state.task_state.ratelimiter.clone() {
                let dispatch = Rc::new(TokenDispatcher::default());
                DISPATCHER.with(|slot| *slot.borrow_mut() = Some(Rc::clone(&dispatch)));
                let shared = Arc::clone(&worker_state.task_state.shared);
                if let Err(e) = ringline::spawn(run_dispatcher(dispatch, rl, shared)) {
                    // Without it every connection falls back to the bounded
                    // wait in `acquire`, which still makes progress -- but it
                    // is the old per-connection poll, so say so.
                    tracing::error!(
                        worker = worker_id,
                        "failed to spawn rate-limit dispatcher: {e}; \
                         connections will poll the limiter individually"
                    );
                    DISPATCHER.with(|slot| *slot.borrow_mut() = None);
                }
            }
            match protocol {
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
                    spawn_protocol_tasks(&worker_state, my_connections, worker_id);
                }
            }
            spawn_append_tasks(&worker_state, worker_id);
            worker_state.task_state.shared.mark_worker_started();
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
            match pin_to_cpu(cpu_id) {
                Ok(()) => tracing::debug!("pinned worker {} to CPU {}", cfg.id, cpu_id),
                Err(e) => {
                    tracing::warn!("failed to pin worker {} to CPU {}: {}", cfg.id, cpu_id, e)
                }
            }
        }

        // Set metrics thread shard
        metrics::set_thread_shard(worker_id);

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
        // An empty endpoint list would panic ketama::Ring::build, so fall back
        // to a dummy single-node ring that is never consulted.
        let ring = if endpoints.is_empty() {
            ketama::Ring::build(&["_"])
        } else {
            let server_ids: Vec<String> = endpoints.iter().map(|a| a.to_string()).collect();
            ketama::Ring::build(&server_ids.iter().map(|s| s.as_str()).collect::<Vec<_>>())
        };

        let tls_enabled = cfg.config.target.tls;
        let tls_server_name = cfg.config.target.tls_hostname.clone();

        // Initialize per-endpoint prefill queues. We partition this worker's
        // prefill range by routing destination so each connection task draws
        // only from its endpoint's sub-queue — avoiding the lock-pop /
        // lock-push-back thrashing the original single-shared-queue design
        // suffered from when most popped keys didn't route to the popping
        // connection's endpoint.
        // Prefill queues are SHARED across all workers (built once by the
        // runner with the full keyspace, partitioned by endpoint). Every
        // worker drains them for the endpoints it serves; prefill completion
        // is gated globally on confirmed >= total in the runner. Per-worker
        // prefill_total stays 0 so the per-worker completion path in
        // `confirm_prefill_key` is inert and `is_prefill_done()` stays false
        // for the whole prefill phase (the phase gate ends prefill).
        let prefill_queues = cfg.prefill_queues;
        let append_queues = cfg.append_queues;
        let append_ratelimiter = cfg.append_ratelimiter.clone();
        let endpoint_keys = cfg.endpoint_keys;
        let prefill_total = 0usize;
        let prefill_done = false;

        let task_state = Arc::new(TaskSharedState {
            key_dist: Arc::clone(&cfg.key_dist),
            config: cfg.config.clone(),
            shared: Arc::clone(&cfg.shared),
            ratelimiter: cfg.ratelimiter.clone(),
            value_pool: cfg.value_pool,
            endpoints,
            ring,
            slot_table: RwLock::new(slot_table),
            prefill_queues,
            append_queues,
            append_ratelimiter,
            endpoint_keys,
            worker_id: cfg.id,
            backfill_on_miss,
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
            prefill_confirmed: AtomicUsize::new(0),
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

        // A failed spawn means this connection never exists. Discarding the
        // error made an over-capacity run look like a clean success with fewer
        // connections than requested, so count and log it: the report shows
        // them as failed rather than silently missing.
        if let Err(e) = ringline::spawn(async move {
            match protocol {
                CacheProtocol::Resp | CacheProtocol::Resp3 => {
                    resp_connection_task(state, endpoint_idx, session_seed).await;
                }
                CacheProtocol::Memcache | CacheProtocol::MemcacheBinary => {
                    memcache_connection_task(state, endpoint_idx, session_seed).await;
                }
                CacheProtocol::Ping => {
                    ping_connection_task(state, endpoint_idx, session_seed).await;
                }
            }
        }) {
            tracing::error!(
                worker = worker_id,
                conn = global_conn_idx,
                "failed to spawn connection task: {e}"
            );
            metrics::CONNECTIONS_FAILED.increment();
        }
    }
}

// ── Connection establishment helper ──────────────────────────────────────

/// Try to connect to an endpoint, returning Ok(conn) or Err with retry sleep.
/// When `tls_server_name` is Some, uses TLS via `ringline::connect_tls`.
///
/// The attempt is bounded by `connect_timeout` (zero disables the bound).
/// Without it a SYN to a host that never answers hangs the task for the
/// kernel's own connect timeout, minutes on Linux, and the precheck deadline
/// in the runner only covers the first attempt of the run, not reconnects.
async fn establish_connection(
    endpoint: SocketAddr,
    worker_id: usize,
    tls_server_name: Option<&str>,
    connect_timeout: Duration,
) -> Result<ConnCtx, DisconnectReason> {
    let connect_result = if let Some(server_name) = tls_server_name {
        ringline::connect_tls(endpoint, server_name)
    } else {
        ringline::connect(endpoint)
    };

    match connect_result {
        Ok(future) => {
            let outcome = if connect_timeout.is_zero() {
                Some(future.await)
            } else {
                match ringline::try_timeout(connect_timeout, future) {
                    Ok(bounded) => bounded.await.ok(),
                    // Timer pool exhausted. The attempt was consumed by the
                    // failed arm, so report it as a failed connect and let the
                    // caller's retry loop try again.
                    Err(_) => Some(connect_timer_exhausted()),
                }
            };
            match outcome {
                Some(Ok(conn)) => Ok(conn),
                Some(Err(e)) => {
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
                None => {
                    tracing::debug!(
                        worker = worker_id,
                        endpoint = %endpoint,
                        timeout = ?connect_timeout,
                        "connect timed out"
                    );
                    metrics::CONNECTIONS_FAILED.increment();
                    metrics::DISCONNECTS_CONNECT_FAILED.increment();
                    Err(DisconnectReason::ConnectFailed)
                }
            }
        }
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

/// Create a RESP on_result callback that records latency and byte metrics.
/// Counter metrics (RESPONSES_RECEIVED, GET_COUNT, etc.) are recorded by record_counters().
fn make_resp_callback() -> impl Fn(&ringline_redis::CommandResult) {
    move |r| {
        match r.command {
            ringline_redis::CommandType::Get => {
                let _ = metrics::GET_LATENCY.increment(r.latency_ns);
                if let Some(ttfb) = r.ttfb_ns {
                    let _ = metrics::GET_TTFB.increment(ttfb);
                }
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
        let _ = metrics::PERCEIVED_LATENCY
            .increment(r.latency_ns + metrics::CURRENT_SLIP_NS.value() as u64);
        metrics::BYTES_TX.add(r.tx_bytes as u64);
        metrics::BYTES_RX.add(r.rx_bytes as u64);
    }
}

/// Create a Memcache on_result callback that records latency and byte metrics.
/// Counter metrics (RESPONSES_RECEIVED, GET_COUNT, etc.) are recorded by record_counters().
fn make_memcache_callback() -> impl Fn(&ringline_memcache::CommandResult) {
    move |r| {
        match r.command {
            ringline_memcache::CommandType::Get => {
                let _ = metrics::GET_LATENCY.increment(r.latency_ns);
                if let Some(ttfb) = r.ttfb_ns {
                    let _ = metrics::GET_TTFB.increment(ttfb);
                }
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
        let _ = metrics::PERCEIVED_LATENCY
            .increment(r.latency_ns + metrics::CURRENT_SLIP_NS.value() as u64);
        metrics::BYTES_TX.add(r.tx_bytes as u64);
        metrics::BYTES_RX.add(r.rx_bytes as u64);
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
        let _ = metrics::PERCEIVED_LATENCY
            .increment(r.latency_ns + metrics::CURRENT_SLIP_NS.value() as u64);
    }
}

// ── RESP connection task ─────────────────────────────────────────────────

/// A single RESP (Valkey/Redis) connection task that connects, drives workload, and reconnects.
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

        let conn = match establish_connection(
            endpoint,
            state.task_state.worker_id,
            tls_name.as_deref(),
            state.task_state.config.connection.connect_timeout,
        )
        .await
        {
            Ok(conn) => conn,
            Err(_) => {
                ringline::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };

        metrics::CONNECTIONS_ACTIVE.increment();
        let builder = ringline_redis::Client::builder(conn).on_result(make_resp_callback());
        #[cfg(target_os = "linux")]
        let builder =
            builder.kernel_timestamps(matches!(config.timestamps.mode, TimestampMode::Software));
        // Zero-copy recv is not a builder toggle — it lives in the recv method:
        // the recv loop calls `client.recv_meta()`, which drains each value
        // without materializing it (zero-copy on io_uring, streaming-drain on
        // mio). `fire_*` is unchanged and metrics are identical (recorded by the
        // `on_result` callback above).
        let mut client = builder
            .max_batch_size(config.connection.effective_batch_size())
            .build();
        log_resp_recv_path_once();

        tracing::debug!(
            worker = state.task_state.worker_id,
            endpoint = %endpoint,
            conn_index = conn.index(),
            "connected (RESP)"
        );

        // RESP3: negotiate the protocol on every new connection before any
        // workload traffic. `HELLO 3` switches the server to RESP3, so its reply
        // and all subsequent replies use RESP3 framing — notably `_\r\n` for a
        // GET miss (vs RESP2 `$-1\r\n`). Without this the `--resp3` flag is inert:
        // the server stays in RESP2. Uses the async request/response path, which
        // is safe here because no `fire_*` traffic is in flight on a fresh
        // connection.
        if matches!(config.target.protocol, CacheProtocol::Resp3)
            && let Err(e) = client
                .cmd(&resp_proto::Request::cmd(b"HELLO").arg(b"3"))
                .await
        {
            tracing::debug!(
                worker = state.task_state.worker_id,
                endpoint = %endpoint,
                "HELLO 3 failed: {}",
                e
            );
            metrics::CONNECTIONS_ACTIVE.decrement();
            metrics::CONNECTIONS_FAILED.increment();
            ringline::sleep(Duration::from_millis(100)).await;
            continue;
        }

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
    let get_ratio = config.workload.commands.get as usize;
    let delete_ratio = config.workload.commands.delete as usize;
    let value_len = config.workload.values.length;
    let pool_len = state.task_state.value_pool.len();
    let backfill_on_miss = state.task_state.backfill_on_miss;
    let num_endpoints = state.task_state.endpoints.len();
    let multi_endpoint = num_endpoints > 1;
    let pipeline_depth = config.connection.pipeline_depth;
    let batch_size = config.connection.effective_batch_size();
    // `None` for a closed-loop run; see `TokenDispatcher`.
    let token_dispatch = dispatcher();
    // Tokens the dispatcher already charged to this connection, carried to the
    // next iteration. `acquire` debits the limiter, so re-polling it here
    // instead of spending these would hand them back to nobody and quietly
    // depress the achieved rate below the configured one.
    let mut carried_tokens: u64 = 0;
    let mut prefill_in_flight: VecDeque<usize> = VecDeque::new();
    let mut inflight = InFlight::new(config.connection.request_timeout, pipeline_depth);

    loop {
        let phase = state.task_state.shared.phase();
        if phase.should_stop() {
            return Ok(());
        }
        // One clock read per iteration stamps every request fired in this
        // batch; the batch is one coalesced send, so the error is bounded by
        // the fire loop, not the network.
        let fired_at = Instant::now();

        // Only refill when there's room for a full batch, so fire_* calls
        // accumulate into a single coalesced send rather than one-per-response.
        let want_fire = pipeline_depth.saturating_sub(client.pending_count()) >= batch_size;

        // Fill pipeline
        if phase == Phase::Prefill && !state.is_prefill_done() {
            while want_fire && client.pending_count() < pipeline_depth {
                let Some(key_id) = state.task_state.prefill_queues.pop_front(endpoint_idx) else {
                    break;
                };

                write_key(key_buf, key_id);

                // Keys were partitioned to this endpoint at init time, so
                // routing should always match. The only way it wouldn't is
                // a slot-table update mid-prefill (RESP cluster topology
                // change). In that case, hand the key to the new owner's
                // queue and move on — no busy-loop.
                if multi_endpoint {
                    let routed = route_key(&state.task_state, key_buf);
                    if routed != endpoint_idx {
                        state.task_state.prefill_queues.push_back(routed, key_id);
                        continue;
                    }
                }

                let user_data = key_id as u64 | PREFILL_MARKER;
                let res = if value_len >= ZC_VALUE_THRESHOLD {
                    let guard =
                        make_value_guard(rng, &state.task_state.value_pool, value_len, pool_len);
                    client.fire_set_with_guard(key_buf, guard, user_data)
                } else {
                    let value = value_slice(rng, &state.task_state.value_pool, value_len, pool_len);
                    client.fire_set(key_buf, value, user_data)
                };
                match res {
                    Ok(_) => {
                        metrics::REQUESTS_SENT.increment();
                        prefill_in_flight.push_back(key_id);
                        inflight.push(fired_at);
                    }
                    Err(_) => {
                        state
                            .task_state
                            .prefill_queues
                            .push_front(endpoint_idx, key_id);
                        break;
                    }
                }
            }
        } else if (phase == Phase::Warmup || phase == Phase::Running) && want_fire {
            // Pre-acquire a full batch of rate-limit tokens up front so the
            // fire loop produces a single coalesced send, rather than being
            // broken into tiny partial flushes by per-iteration try_wait()
            // rejections.
            let mut token_budget = match state.task_state.ratelimiter {
                Some(ref rl) => {
                    if carried_tokens > 0 {
                        let n = carried_tokens as usize;
                        carried_tokens = 0;
                        n
                    } else {
                        let n = (batch_size as u64).min(rl.max_tokens());
                        if n > 0 && rl.try_wait_n(n).is_ok() {
                            n as usize
                        } else {
                            0
                        }
                    }
                }
                None => batch_size,
            };

            // Coordinated omission: publish the current schedule slip (rate
            // limiter backlog) so each sent request can be charged a sample
            // and the result callback can compute perceived latency.
            let slip_ns = match state.task_state.ratelimiter {
                Some(ref rl) => crate::metrics::slip_ns(rl.available(), rl.rate()),
                None => 0,
            };
            crate::metrics::CURRENT_SLIP_NS.set(slip_ns as i64);

            while client.pending_count() < pipeline_depth && token_budget > 0 {
                // Drain backfill queue first
                if let Some(key_id) = backfill_queue.pop() {
                    write_key(key_buf, key_id);

                    token_budget -= 1;

                    // user_data encodes key_id with backfill marker (high bit set)
                    let user_data = key_id as u64 | BACKFILL_MARKER;
                    let res = if value_len >= ZC_VALUE_THRESHOLD {
                        let guard = make_value_guard(
                            rng,
                            &state.task_state.value_pool,
                            value_len,
                            pool_len,
                        );
                        client.fire_set_with_guard(key_buf, guard, user_data)
                    } else {
                        let value =
                            value_slice(rng, &state.task_state.value_pool, value_len, pool_len);
                        client.fire_set(key_buf, value, user_data)
                    };
                    match res {
                        Ok(_) => {
                            metrics::REQUESTS_SENT.increment();
                            let _ = metrics::SCHEDULE_SLIP.increment(slip_ns);
                            inflight.push(fired_at);
                        }
                        Err(_) => {
                            backfill_queue.push(key_id);
                            break;
                        }
                    }
                    continue;
                }

                // Pick a key owned by this connection's endpoint.
                //
                // UNIFORM + multi-endpoint: index a precomputed per-endpoint
                // key-id list (O(1)). Rejection-sampling random keys and
                // re-routing each one made CRC16 slot routing ~40% of CPU at
                // high pipeline depth, and with a uniform draw the per-endpoint
                // list reproduces the global distribution exactly, so the fast
                // path is free.
                //
                // SKEWED + multi-endpoint: the fast path would be WRONG. Drawing
                // uniformly from each endpoint's own bucket makes every shard see
                // a uniform workload and equal load, which is precisely the hot-
                // shard effect a skewed keyspace exists to measure. So draw from
                // the global distribution and reject-route, exactly as the RESP
                // path does. A rejected draw must NOT consume a rate-limit token:
                // because tokens are only spent on a match, each endpoint ends up
                // receiving traffic in proportion to the share of the key
                // distribution it owns -- which is the imbalance we want, not an
                // artifact to correct for.
                let key_id = if !multi_endpoint {
                    state.task_state.key_dist.sample(rng)
                } else if matches!(&*state.task_state.key_dist, KeyDist::Uniform { .. }) {
                    let bucket = &state.task_state.endpoint_keys[endpoint_idx];
                    if bucket.is_empty() {
                        // Endpoint owns no keys (degenerate keyspace); yield to
                        // recv and try the next cycle.
                        break;
                    }
                    bucket[rng.random_range(0..bucket.len())] as usize
                } else {
                    let mut attempts = 0usize;
                    let max_attempts = max_routing_attempts(num_endpoints);
                    let picked = loop {
                        let candidate = state.task_state.key_dist.sample(rng);
                        write_key(key_buf, candidate);
                        if route_key(&state.task_state, key_buf) == endpoint_idx {
                            break Some(candidate);
                        }
                        attempts += 1;
                        if attempts >= max_attempts {
                            break None;
                        }
                    };
                    match picked {
                        Some(k) => k,
                        // Could not find a key for this endpoint in the attempt
                        // budget; yield to recv rather than spending a token.
                        None => break,
                    }
                };
                write_key(key_buf, key_id);

                token_budget -= 1;

                // Choose command
                let roll = rng.random_range(0..100);
                let sent = if roll < get_ratio {
                    // Pass key_id as user_data for backfill-on-miss tracking
                    client.fire_get(key_buf, key_id as u64).is_ok()
                } else if roll < get_ratio + delete_ratio {
                    client.fire_del(key_buf, 0).is_ok()
                } else if value_len >= ZC_VALUE_THRESHOLD {
                    let guard =
                        make_value_guard(rng, &state.task_state.value_pool, value_len, pool_len);
                    client.fire_set_with_guard(key_buf, guard, 0).is_ok()
                } else {
                    let value = value_slice(rng, &state.task_state.value_pool, value_len, pool_len);
                    client.fire_set(key_buf, value, 0).is_ok()
                };

                if sent {
                    metrics::REQUESTS_SENT.increment();
                    let _ = metrics::SCHEDULE_SLIP.increment(slip_ns);
                    inflight.push(fired_at);
                } else {
                    break;
                }
            }
        }

        // Refresh the slip gauge every iteration (including when the pipeline
        // is full and we did not fire) so perceived latency reflects a still-
        // growing backlog.
        if let Some(ref rl) = state.task_state.ratelimiter {
            crate::metrics::CURRENT_SLIP_NS
                .set(crate::metrics::slip_ns(rl.available(), rl.rate()) as i64);
        }

        // Nothing in flight, so this connection is waiting for a rate-limit
        // token rather than for the network. Park on the worker's dispatcher,
        // which wakes it when one is actually available -- one wakeup per token
        // granted instead of one per poll interval per connection. A run with
        // no limiter has no dispatcher and keeps the old fixed poll, which it
        // reaches only on send backpressure.
        if client.pending_count() == 0 {
            match token_dispatch {
                Some(ref d) => {
                    carried_tokens = d.acquire(batch_size as u64).await;
                }
                None => ringline::sleep(IDLE_SLEEP_MIN).await,
            }
            continue;
        }

        // Zero-copy recv, every reply, every op kind and phase (prefill SETs
        // included). A load generator never needs value *content* — only the
        // header (kind / hit-miss / length / error) — so `recv_meta` drains each
        // value without materializing it: zero-copy over provided buffers on
        // io_uring, bounded streaming-drain on mio. One uniform call, no config
        // flag, no cfg split, no workload restriction.
        let conn = client.conn();
        let result = match recv_with_timeout(client.recv_meta(), &mut inflight, conn).await {
            Ok(Ok(meta)) => map_respmeta(meta),
            Ok(Err(ringline_redis::Error::ConnectionClosed)) => {
                requeue_drained_prefill(&state.task_state, key_buf, prefill_in_flight.drain(..));
                return Err(DisconnectReason::Eof);
            }
            Ok(Err(_)) => {
                requeue_drained_prefill(&state.task_state, key_buf, prefill_in_flight.drain(..));
                return Err(DisconnectReason::RecvError);
            }
            Err(reason) => {
                requeue_drained_prefill(&state.task_state, key_buf, prefill_in_flight.drain(..));
                return Err(reason);
            }
        };
        inflight.pop();

        // Handle prefill tracking. The PREFILL_MARKER bit in user_data
        // identifies prefill SETs unambiguously across phases, so we don't
        // mis-attribute a non-prefill response to a prefill key.
        if result.prefill {
            let key_id = prefill_in_flight
                .pop_front()
                .or(result.key_id)
                .expect("prefill response without key_id");
            if result.success {
                confirm_prefill_key(state);
            } else {
                // Retry failed prefill SET
                state
                    .task_state
                    .prefill_queues
                    .push_back(endpoint_idx, key_id);
            }
        }

        // Handle backfill-on-miss
        if backfill_on_miss
            && result.request_type == RequestType::Get
            && result.success
            && result.hit == Some(false)
            && backfill_queue.len() < BACKFILL_QUEUE_CAP
            && let Some(key_id) = result.key_id
        {
            backfill_queue.push(key_id);
        }

        // Handle backfill SET tracking
        if result.backfill && result.request_type == RequestType::Set && result.success {
            metrics::BACKFILL_SET_COUNT.increment();
            let _ = metrics::BACKFILL_SET_LATENCY.increment(result.latency_ns);
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

/// Marker bit for prefill SET user_data (second-highest bit of u64).
/// The remaining low bits carry the key_id so we can identify a prefill
/// response without relying on FIFO assumptions across phases.
const PREFILL_MARKER: u64 = 1 << 62;

/// Marker bit for append-stream SET user_data (third-highest bit of u64).
const APPEND_MARKER: u64 = 1 << 61;

/// Mask recovering the key id from a marked `user_data`.
const USER_DATA_ID_MASK: u64 = !(BACKFILL_MARKER | PREFILL_MARKER | APPEND_MARKER);

/// Per-connection cap on pending backfill-on-miss key IDs. Bounds memory
/// when miss rate outpaces backfill drain (cold cache, large keyspace).
/// Overflow drops the new miss; backfill is best-effort.
const BACKFILL_QUEUE_CAP: usize = 1024;

/// Report the RESP recv path once, so a run's output makes the strategy
/// explicit (transparency: automatic, but observable).
fn log_resp_recv_path_once() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        tracing::info!(
            "RESP recv: recv_meta — value bytes drained, never materialized \
             (zero-copy on io_uring, bounded streaming-drain on mio)"
        );
    });
}

/// Map a ringline-redis zero-copy `RespMeta` (from `recv_meta`) to a
/// `RequestResult`, for any op kind, on any backend. The value bytes were
/// drained, never materialized — only the header metadata is used, which is all
/// a load generator records: hit/miss, prefill/backfill markers, key_id, a
/// MOVED/ASK redirect from the error string, and per-op `latency_ns`. rx-bytes
/// are recorded by the `on_result` callback.
fn map_respmeta(meta: ringline_redis::RespMeta) -> RequestResult {
    use ringline_redis::OpKind;
    let request_type = match meta.kind {
        OpKind::Get => RequestType::Get,
        OpKind::Set => RequestType::Set,
        OpKind::Del => RequestType::Delete,
    };
    // hit is meaningful only for a successful GET.
    let hit = match meta.kind {
        OpKind::Get if meta.success => Some(meta.value_len.is_some()),
        _ => None,
    };
    // prefill/backfill/append markers + key_id apply per kind.
    let (backfill, prefill, append, key_id) = match meta.kind {
        OpKind::Get => (
            false,
            false,
            false,
            Some((meta.user_data & USER_DATA_ID_MASK) as usize),
        ),
        OpKind::Set => {
            let backfill = meta.user_data & BACKFILL_MARKER != 0;
            let prefill = meta.user_data & PREFILL_MARKER != 0;
            let append = meta.user_data & APPEND_MARKER != 0;
            let key_id = if prefill || append {
                Some((meta.user_data & USER_DATA_ID_MASK) as usize)
            } else {
                None
            };
            (backfill, prefill, append, key_id)
        }
        OpKind::Del => (false, false, false, None),
    };
    // A server error carries the message (incl. MOVED/ASK) in `error`.
    let redirect = meta.error.as_deref().and_then(parse_resp_redirect);
    RequestResult {
        id: 0,
        success: meta.success,
        is_error_response: !meta.success,
        latency_ns: meta.latency_ns,
        ttfb_ns: None,
        request_type,
        hit,
        key_id,
        backfill,
        prefill,
        append,
        redirect,
    }
}

/// Parse a Valkey/Redis error message for MOVED/ASK redirect.
fn parse_resp_redirect(msg: &str) -> Option<resp_proto::Redirect> {
    let (kind, rest) = if let Some(rest) = msg.strip_prefix("MOVED ") {
        (resp_proto::RedirectKind::Moved, rest)
    } else {
        let rest = msg.strip_prefix("ASK ")?;
        (resp_proto::RedirectKind::Ask, rest)
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
        let mut slot_table = state.task_state.slot_table.write().unwrap();
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

/// The two ringline-memcache client flavors behind one type, so a single drive
/// loop serves both the ASCII and binary protocols. `Client` and `BinaryClient`
/// expose the same fire/recv pipelining surface (only the ASCII client adds the
/// high-level request API and `version()`), so each method here just forwards.
enum McClient {
    Ascii(ringline_memcache::Client),
    Binary(ringline_memcache::BinaryClient),
}

impl McClient {
    #[inline]
    fn pending_count(&self) -> usize {
        match self {
            McClient::Ascii(c) => c.pending_count(),
            McClient::Binary(c) => c.pending_count(),
        }
    }

    #[inline]
    fn fire_get(&mut self, key: &[u8], user_data: u64) -> Result<(), ringline_memcache::Error> {
        match self {
            McClient::Ascii(c) => c.fire_get(key, user_data),
            McClient::Binary(c) => c.fire_get(key, user_data),
        }
    }

    #[inline]
    fn fire_set_with_guard<G: SendGuard>(
        &mut self,
        key: &[u8],
        guard: G,
        flags: u32,
        exptime: u32,
        user_data: u64,
    ) -> Result<(), ringline_memcache::Error> {
        match self {
            McClient::Ascii(c) => c.fire_set_with_guard(key, guard, flags, exptime, user_data),
            McClient::Binary(c) => c.fire_set_with_guard(key, guard, flags, exptime, user_data),
        }
    }

    #[inline]
    fn fire_delete(&mut self, key: &[u8], user_data: u64) -> Result<(), ringline_memcache::Error> {
        match self {
            McClient::Ascii(c) => c.fire_delete(key, user_data),
            McClient::Binary(c) => c.fire_delete(key, user_data),
        }
    }

    #[inline]
    async fn recv(&mut self) -> Result<ringline_memcache::CompletedOp, ringline_memcache::Error> {
        match self {
            McClient::Ascii(c) => c.recv().await,
            McClient::Binary(c) => c.recv().await,
        }
    }

    #[inline]
    fn conn(&self) -> ConnCtx {
        match self {
            McClient::Ascii(c) => c.conn(),
            McClient::Binary(c) => c.conn(),
        }
    }
}

/// A single Memcache connection task (ASCII or binary, per protocol).
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

        let conn = match establish_connection(
            endpoint,
            state.task_state.worker_id,
            tls_name.as_deref(),
            state.task_state.config.connection.connect_timeout,
        )
        .await
        {
            Ok(conn) => conn,
            Err(_) => {
                ringline::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };

        metrics::CONNECTIONS_ACTIVE.increment();
        let builder = ringline_memcache::Client::builder(conn)
            .on_result(make_memcache_callback())
            .max_batch_size(config.connection.effective_batch_size());
        #[cfg(target_os = "linux")]
        let builder =
            builder.kernel_timestamps(matches!(config.timestamps.mode, TimestampMode::Software));
        let binary = matches!(config.target.protocol, CacheProtocol::MemcacheBinary);
        let mut client = if binary {
            McClient::Binary(builder.build_binary())
        } else {
            McClient::Ascii(builder.build())
        };

        tracing::debug!(
            worker = state.task_state.worker_id,
            endpoint = %endpoint,
            conn_index = conn.index(),
            "connected (Memcache)"
        );

        // Precheck: send VERSION to verify connectivity
        if state.task_state.shared.phase() == Phase::Precheck && !state.is_precheck_done() {
            let precheck = match &mut client {
                McClient::Ascii(c) => c.version().await.map(|_| ()),
                // The binary subset has no VERSION opcode (some servers drop the
                // connection on it) and BinaryClient exposes no version(); a live
                // connection is a sufficient precheck.
                McClient::Binary(_) => Ok(()),
            };
            match precheck {
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
                record_disconnect_reason(reason);
            }
        }

        ringline::sleep(Duration::from_millis(100)).await;
    }
}

/// Drive the Memcache workload on a connected client using fire/recv pipelining.
async fn drive_memcache_workload(
    client: &mut McClient,
    state: &Arc<SharedWorkerState>,
    endpoint_idx: usize,
    rng: &mut Xoshiro256PlusPlus,
    key_buf: &mut [u8],
    backfill_queue: &mut Vec<usize>,
) -> Result<(), DisconnectReason> {
    let config = &state.task_state.config;
    let get_ratio = config.workload.commands.get as usize;
    let delete_ratio = config.workload.commands.delete as usize;
    let value_len = config.workload.values.length;
    let pool_len = state.task_state.value_pool.len();
    let backfill_on_miss = state.task_state.backfill_on_miss;
    let num_endpoints = state.task_state.endpoints.len();
    let multi_endpoint = num_endpoints > 1;
    let pipeline_depth = config.connection.pipeline_depth;
    let batch_size = config.connection.effective_batch_size();
    // `None` for a closed-loop run; see `TokenDispatcher`.
    let token_dispatch = dispatcher();
    // Tokens the dispatcher already charged to this connection, carried to the
    // next iteration. `acquire` debits the limiter, so re-polling it here
    // instead of spending these would hand them back to nobody and quietly
    // depress the achieved rate below the configured one.
    let mut carried_tokens: u64 = 0;
    let mut prefill_in_flight: VecDeque<usize> = VecDeque::new();
    let mut inflight = InFlight::new(config.connection.request_timeout, pipeline_depth);

    loop {
        let phase = state.task_state.shared.phase();
        if phase.should_stop() {
            return Ok(());
        }
        // One clock read per iteration stamps every request fired in this
        // batch; the batch is one coalesced send, so the error is bounded by
        // the fire loop, not the network.
        let fired_at = Instant::now();

        // Only refill when there's room for a full batch, so fire_* calls
        // accumulate into a single coalesced send rather than one-per-response.
        let want_fire = pipeline_depth.saturating_sub(client.pending_count()) >= batch_size;

        // Fill pipeline
        if phase == Phase::Prefill && !state.is_prefill_done() {
            while want_fire && client.pending_count() < pipeline_depth {
                let Some(key_id) = state.task_state.prefill_queues.pop_front(endpoint_idx) else {
                    break;
                };

                write_key(key_buf, key_id);

                // Keys were partitioned to this endpoint at init time; only
                // a slot-table change mid-prefill can land one here that no
                // longer routes to us. Hand it to the new owner's queue.
                if multi_endpoint {
                    let routed = route_key(&state.task_state, key_buf);
                    if routed != endpoint_idx {
                        state.task_state.prefill_queues.push_back(routed, key_id);
                        continue;
                    }
                }

                let guard =
                    make_value_guard(rng, &state.task_state.value_pool, value_len, pool_len);

                let user_data = key_id as u64 | PREFILL_MARKER;
                match client.fire_set_with_guard(key_buf, guard, 0, 0, user_data) {
                    Ok(_) => {
                        metrics::REQUESTS_SENT.increment();
                        prefill_in_flight.push_back(key_id);
                        inflight.push(fired_at);
                    }
                    Err(_) => {
                        state
                            .task_state
                            .prefill_queues
                            .push_front(endpoint_idx, key_id);
                        break;
                    }
                }
            }
        } else if (phase == Phase::Warmup || phase == Phase::Running) && want_fire {
            // Pre-acquire a full batch of rate-limit tokens up front so the
            // fire loop produces a single coalesced send, rather than being
            // broken into tiny partial flushes by per-iteration try_wait()
            // rejections.
            let mut token_budget = match state.task_state.ratelimiter {
                Some(ref rl) => {
                    if carried_tokens > 0 {
                        let n = carried_tokens as usize;
                        carried_tokens = 0;
                        n
                    } else {
                        let n = (batch_size as u64).min(rl.max_tokens());
                        if n > 0 && rl.try_wait_n(n).is_ok() {
                            n as usize
                        } else {
                            0
                        }
                    }
                }
                None => batch_size,
            };

            let slip_ns = match state.task_state.ratelimiter {
                Some(ref rl) => crate::metrics::slip_ns(rl.available(), rl.rate()),
                None => 0,
            };
            crate::metrics::CURRENT_SLIP_NS.set(slip_ns as i64);

            while client.pending_count() < pipeline_depth && token_budget > 0 {
                // Drain backfill queue first
                if let Some(key_id) = backfill_queue.pop() {
                    write_key(key_buf, key_id);
                    let guard =
                        make_value_guard(rng, &state.task_state.value_pool, value_len, pool_len);

                    token_budget -= 1;

                    let user_data = key_id as u64 | BACKFILL_MARKER;
                    match client.fire_set_with_guard(key_buf, guard, 0, 0, user_data) {
                        Ok(_) => {
                            metrics::REQUESTS_SENT.increment();
                            let _ = metrics::SCHEDULE_SLIP.increment(slip_ns);
                            inflight.push(fired_at);
                        }
                        Err(_) => {
                            backfill_queue.push(key_id);
                            break;
                        }
                    }
                    continue;
                }

                // Generate a random key. In multi-endpoint setups, only keys
                // that route to this connection's endpoint can be sent, so
                // retry without consuming rate-limit tokens until one matches.
                // A token consumed on a routing miss would be globally lost
                // from a rate limiter shared across all workers.
                let key_id = {
                    let mut attempts = 0usize;
                    let max_attempts = max_routing_attempts(num_endpoints);
                    loop {
                        let candidate = state.task_state.key_dist.sample(rng);
                        write_key(key_buf, candidate);
                        if !multi_endpoint || route_key(&state.task_state, key_buf) == endpoint_idx
                        {
                            break Some(candidate);
                        }
                        attempts += 1;
                        if attempts >= max_attempts {
                            break None;
                        }
                    }
                };
                let Some(key_id) = key_id else {
                    // No matching key found; yield to recv and try next cycle.
                    break;
                };

                token_budget -= 1;

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
                    let _ = metrics::SCHEDULE_SLIP.increment(slip_ns);
                    inflight.push(fired_at);
                } else {
                    break;
                }
            }
        }

        if let Some(ref rl) = state.task_state.ratelimiter {
            crate::metrics::CURRENT_SLIP_NS
                .set(crate::metrics::slip_ns(rl.available(), rl.rate()) as i64);
        }

        // Nothing in flight, so this connection is waiting for a rate-limit
        // token rather than for the network. Park on the worker's dispatcher,
        // which wakes it when one is actually available -- one wakeup per token
        // granted instead of one per poll interval per connection. A run with
        // no limiter has no dispatcher and keeps the old fixed poll, which it
        // reaches only on send backpressure.
        if client.pending_count() == 0 {
            match token_dispatch {
                Some(ref d) => {
                    carried_tokens = d.acquire(batch_size as u64).await;
                }
                None => ringline::sleep(IDLE_SLEEP_MIN).await,
            }
            continue;
        }

        let conn = client.conn();
        let op = match recv_with_timeout(client.recv(), &mut inflight, conn).await {
            Ok(Ok(op)) => op,
            Ok(Err(ringline_memcache::Error::ConnectionClosed)) => {
                requeue_drained_prefill(&state.task_state, key_buf, prefill_in_flight.drain(..));
                return Err(DisconnectReason::Eof);
            }
            Ok(Err(_)) => {
                requeue_drained_prefill(&state.task_state, key_buf, prefill_in_flight.drain(..));
                return Err(DisconnectReason::RecvError);
            }
            Err(reason) => {
                requeue_drained_prefill(&state.task_state, key_buf, prefill_in_flight.drain(..));
                return Err(reason);
            }
        };
        inflight.pop();

        // Map CompletedOp to RequestResult
        let result = map_memcache_op(op);

        // Handle prefill tracking. See the RESP recv loop for rationale.
        if result.prefill {
            let key_id = prefill_in_flight
                .pop_front()
                .or(result.key_id)
                .expect("prefill response without key_id");
            if result.success {
                confirm_prefill_key(state);
            } else {
                // Retry failed prefill SET
                state
                    .task_state
                    .prefill_queues
                    .push_back(endpoint_idx, key_id);
            }
        }

        // Handle backfill-on-miss
        if backfill_on_miss
            && result.request_type == RequestType::Get
            && result.success
            && result.hit == Some(false)
            && backfill_queue.len() < BACKFILL_QUEUE_CAP
            && let Some(key_id) = result.key_id
        {
            backfill_queue.push(key_id);
        }

        // Handle backfill SET tracking
        if result.backfill && result.request_type == RequestType::Set && result.success {
            metrics::BACKFILL_SET_COUNT.increment();
            let _ = metrics::BACKFILL_SET_LATENCY.increment(result.latency_ns);
        }

        // Record counter metrics
        record_counters(&result);
    }
}

/// Map a ringline-memcache CompletedOp to a RequestResult.
fn map_memcache_op(op: ringline_memcache::CompletedOp) -> RequestResult {
    match op {
        ringline_memcache::CompletedOp::Get {
            result,
            user_data,
            latency_ns,
        } => {
            let (success, is_error, hit) = match &result {
                Ok(Some(_)) => (true, false, Some(true)),
                Ok(None) => (true, false, Some(false)),
                Err(_) => (false, true, None),
            };
            RequestResult {
                id: 0,
                success,
                is_error_response: is_error,
                latency_ns,
                ttfb_ns: None,
                request_type: RequestType::Get,
                hit,
                key_id: Some((user_data & USER_DATA_ID_MASK) as usize),
                backfill: false,
                prefill: false,
                append: false,
                redirect: None,
            }
        }
        ringline_memcache::CompletedOp::Set {
            result,
            user_data,
            latency_ns,
        } => {
            let (success, is_error) = match &result {
                Ok(()) => (true, false),
                Err(_) => (false, true),
            };
            let backfill = user_data & BACKFILL_MARKER != 0;
            let prefill = user_data & PREFILL_MARKER != 0;
            let append = user_data & APPEND_MARKER != 0;
            let key_id = if prefill || append {
                Some((user_data & USER_DATA_ID_MASK) as usize)
            } else {
                None
            };
            RequestResult {
                id: 0,
                success,
                is_error_response: is_error,
                latency_ns,
                ttfb_ns: None,
                request_type: RequestType::Set,
                hit: None,
                key_id,
                backfill,
                prefill,
                append,
                redirect: None,
            }
        }
        ringline_memcache::CompletedOp::Delete {
            result, latency_ns, ..
        } => {
            let (success, is_error) = match &result {
                Ok(_) => (true, false),
                Err(_) => (false, true),
            };
            RequestResult {
                id: 0,
                success,
                is_error_response: is_error,
                latency_ns,
                ttfb_ns: None,
                request_type: RequestType::Delete,
                hit: None,
                key_id: None,
                backfill: false,
                prefill: false,
                append: false,
                redirect: None,
            }
        }
        // cachecannon only fires Get/Set/Delete; `CompletedOp` is #[non_exhaustive].
        _ => unreachable!("unexpected ringline-memcache CompletedOp variant"),
    }
}

// ── Ping connection task ─────────────────────────────────────────────────

/// A single Ping protocol connection task.
async fn ping_connection_task(state: Arc<SharedWorkerState>, endpoint_idx: usize, _seed: u64) {
    let endpoint = state.task_state.endpoints[endpoint_idx];
    #[cfg(target_os = "linux")]
    let config = &state.task_state.config;
    let tls_name = resolve_tls_server_name(&state.task_state, endpoint);

    loop {
        let phase = state.task_state.shared.phase();
        if phase.should_stop() {
            return;
        }

        let conn = match establish_connection(
            endpoint,
            state.task_state.worker_id,
            tls_name.as_deref(),
            state.task_state.config.connection.connect_timeout,
        )
        .await
        {
            Ok(conn) => conn,
            Err(_) => {
                ringline::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };

        metrics::CONNECTIONS_ACTIVE.increment();
        let builder = ringline_ping::Client::builder(conn).on_result(make_ping_callback());
        #[cfg(target_os = "linux")]
        let builder =
            builder.kernel_timestamps(matches!(config.timestamps.mode, TimestampMode::Software));
        let mut client = builder.build();

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
    // One request in flight at a time on the ping path.
    let mut inflight = InFlight::new(state.task_state.config.connection.request_timeout, 1);
    loop {
        let phase = state.task_state.shared.phase();
        if phase.should_stop() {
            return Ok(());
        }
        // One clock read per iteration stamps every request fired in this
        // batch; the batch is one coalesced send, so the error is bounded by
        // the fire loop, not the network.
        let fired_at = Instant::now();

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
            // Yield so we don't monopolize the executor thread while waiting
            // for the main thread to advance the phase.
            ringline::sleep(Duration::from_millis(1)).await;
            continue;
        }

        // Rate limiting. On miss, sleep until the next token would be
        // available rather than tight-spinning — this is the only yield
        // point on the ping hot path, and skipping it starves every other
        // task sharing this executor thread.
        if let Some(ref rl) = state.task_state.ratelimiter {
            match rl.try_wait() {
                Ok(()) => {}
                Err(TryWaitError::Insufficient(wait)) => {
                    ringline::sleep(wait.max(Duration::from_micros(50))).await;
                    continue;
                }
                Err(_) => {
                    ringline::sleep(Duration::from_millis(1)).await;
                    continue;
                }
            }
        }

        let slip_ns = match state.task_state.ratelimiter {
            Some(ref rl) => crate::metrics::slip_ns(rl.available(), rl.rate()),
            None => 0,
        };
        crate::metrics::CURRENT_SLIP_NS.set(slip_ns as i64);

        metrics::REQUESTS_SENT.increment();
        let _ = metrics::SCHEDULE_SLIP.increment(slip_ns);
        inflight.push(fired_at);
        let conn = client.conn();
        match recv_with_timeout(client.ping(), &mut inflight, conn).await {
            Ok(Ok(())) => {}
            Ok(Err(ringline_ping::Error::ConnectionClosed)) => {
                return Err(DisconnectReason::Eof);
            }
            Ok(Err(_)) => {
                return Err(DisconnectReason::RecvError);
            }
            Err(reason) => return Err(reason),
        }
        inflight.pop();
    }
}

// ── Prefill helpers ──────────────────────────────────────────────────────

/// Confirm a single prefill key was successfully stored.
fn confirm_prefill_key(state: &Arc<SharedWorkerState>) {
    // Increment global counter (used by runner for progress reporting).
    state
        .task_state
        .shared
        .prefill_keys_confirmed
        .fetch_add(1, Ordering::Release);

    // Increment per-worker counter and check completion against this worker's total.
    let worker_confirmed = state.prefill_confirmed.fetch_add(1, Ordering::AcqRel) + 1;

    if worker_confirmed >= state.prefill_total
        && state.prefill_total > 0
        && state
            .prefill_done
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    {
        tracing::debug!(
            worker_id = state.task_state.worker_id,
            confirmed = worker_confirmed,
            total = state.prefill_total,
            "prefill complete"
        );
        state.task_state.shared.mark_prefill_complete();
    }
}

// ── Append role (writer connections) ─────────────────────────────────────
//
// The append stream is a separate role with its own connections. Sharing the
// reader pipelines would queue reads behind a batch of SETs inside this
// client, so read latency would carry an artifact of cachecannon's own
// pipeline rather than the server's behaviour under write load. A writer
// connection runs the prefill drain loop permanently against the append
// queues: pop ids, route, fire SETs, confirm on response, requeue on failure.
// The runner fills the queues one batch at a time and publishes the new
// keyspace head once every SET in the batch is confirmed.

/// Split `total` connections across `num_threads` workers the same way
/// `spawn_protocol_tasks` does, returning this worker's share and the global
/// index of its first connection.
fn connection_share(total: usize, num_threads: usize, worker_id: usize) -> (usize, usize) {
    let num_threads = num_threads.max(1);
    let base = total / num_threads;
    let rem = total % num_threads;
    let mine = if worker_id < rem { base + 1 } else { base };
    let start = if worker_id < rem {
        worker_id * (base + 1)
    } else {
        rem * (base + 1) + (worker_id - rem) * base
    };
    (mine, start)
}

/// Spawn this worker's share of the append-stream writer connections. A
/// no-op without `[workload.append]`.
fn spawn_append_tasks(worker_state: &Arc<SharedWorkerState>, worker_id: usize) {
    let Some(append) = worker_state.task_state.config.workload.append.as_ref() else {
        return;
    };
    let num_endpoints = worker_state.task_state.endpoints.len().max(1);
    let num_threads = worker_state.task_state.config.general.threads;
    let (mine, start) = connection_share(append.connections, num_threads, worker_id);
    let protocol = worker_state.task_state.config.target.protocol;

    for i in 0..mine {
        let global_idx = start + i;
        let endpoint_idx = global_idx % num_endpoints;
        let state = Arc::clone(worker_state);
        // Distinct seed space from the reader connections.
        let seed = 0xA99E_0000_0000 + worker_id as u64 * 10000 + i as u64;

        if let Err(e) = ringline::spawn(async move {
            match protocol {
                CacheProtocol::Resp | CacheProtocol::Resp3 => {
                    resp_append_task(state, endpoint_idx, seed).await;
                }
                CacheProtocol::Memcache | CacheProtocol::MemcacheBinary => {
                    memcache_append_task(state, endpoint_idx, seed).await;
                }
                // Rejected by config validation: ping has no SET.
                CacheProtocol::Ping => {}
            }
        }) {
            tracing::error!(
                worker = worker_id,
                conn = global_idx,
                "failed to spawn append writer task: {e}"
            );
            metrics::CONNECTIONS_FAILED.increment();
        }
    }
}

/// Writer-side on_result callback: append SET latency and bytes only. The
/// read workload's latency and count metrics are untouched.
fn make_resp_append_callback() -> impl Fn(&ringline_redis::CommandResult) {
    move |r| {
        let _ = metrics::APPEND_SET_LATENCY.increment(r.latency_ns);
        metrics::BYTES_TX.add(r.tx_bytes as u64);
        metrics::BYTES_RX.add(r.rx_bytes as u64);
    }
}

fn make_memcache_append_callback() -> impl Fn(&ringline_memcache::CommandResult) {
    move |r| {
        let _ = metrics::APPEND_SET_LATENCY.increment(r.latency_ns);
        metrics::BYTES_TX.add(r.tx_bytes as u64);
        metrics::BYTES_RX.add(r.rx_bytes as u64);
    }
}

/// How many SETs a writer may fire right now given `free` pipeline slots.
/// Unlimited for burst pacing. For spread pacing, take a full batch of tokens
/// when available, otherwise a single one, so a low rate still trickles out
/// one SET at a time instead of waiting for a whole batch of tokens.
fn append_token_budget(rl: &Option<Arc<Ratelimiter>>, free: usize) -> usize {
    let Some(rl) = rl else {
        return free;
    };
    let n = (free as u64).min(rl.max_tokens()).max(1);
    if rl.try_wait_n(n).is_ok() {
        n as usize
    } else if n > 1 && rl.try_wait_n(1).is_ok() {
        1
    } else {
        0
    }
}

/// Confirm a single append-stream key was stored. The runner watches the
/// shared counter to decide when a batch has fully landed.
fn confirm_append_key(state: &Arc<SharedWorkerState>) {
    state
        .task_state
        .shared
        .append_keys_confirmed
        .fetch_add(1, Ordering::Release);
    metrics::APPEND_SET_COUNT.increment();
}

/// Requeue in-flight append keys on disconnect, re-routing each against the
/// current slot table (see `requeue_drained_prefill`).
fn requeue_drained_append(
    state: &TaskSharedState,
    key_buf: &mut [u8],
    drained: impl IntoIterator<Item = usize>,
) {
    for key_id in drained {
        write_key(key_buf, key_id);
        let routed = route_key(state, key_buf);
        state.append_queues.push_back(routed, key_id);
    }
}

/// Build the ketama ring used for non-cluster routing.
fn build_ring(endpoints: &[SocketAddr]) -> ketama::Ring {
    if endpoints.is_empty() {
        ketama::Ring::build(&["_"])
    } else {
        let ids: Vec<String> = endpoints.iter().map(|a| a.to_string()).collect();
        ketama::Ring::build(&ids.iter().map(|s| s.as_str()).collect::<Vec<_>>())
    }
}

/// Enqueue one append batch, ids `start..start + count`, partitioned to the
/// owning endpoint the same way prefill is. Called by the runner when a batch
/// opens; the writer connections drain it.
pub(crate) fn enqueue_append_batch(
    queues: &PrefillQueues,
    start: usize,
    count: usize,
    key_len: usize,
    endpoints: &[SocketAddr],
    slot_table: &Option<Vec<u16>>,
) {
    let n = endpoints.len().max(1);
    let ring = build_ring(endpoints);
    let mut key_buf = vec![0u8; key_len];
    for key_id in start..start + count {
        write_key(&mut key_buf, key_id);
        let ep = route_partition(slot_table, &ring, n, &key_buf);
        queues.push_back(ep, key_id);
    }
}

/// A RESP append-stream writer connection: connect, drain the append queue
/// for its endpoint, reconnect on failure.
async fn resp_append_task(state: Arc<SharedWorkerState>, endpoint_idx: usize, seed: u64) {
    let endpoint = state.task_state.endpoints[endpoint_idx];
    let config = &state.task_state.config;
    let append = config
        .workload
        .append
        .as_ref()
        .expect("append task requires [workload.append]");
    let tls_name = resolve_tls_server_name(&state.task_state, endpoint);
    let mut rng = Xoshiro256PlusPlus::seed_from_u64(seed);
    let mut key_buf = vec![0u8; config.workload.keyspace.length];

    loop {
        if state.task_state.shared.phase().should_stop() {
            return;
        }

        let conn = match establish_connection(
            endpoint,
            state.task_state.worker_id,
            tls_name.as_deref(),
            state.task_state.config.connection.connect_timeout,
        )
        .await
        {
            Ok(conn) => conn,
            Err(_) => {
                ringline::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };

        metrics::APPEND_CONNECTIONS_ACTIVE.increment();
        let builder = ringline_redis::Client::builder(conn).on_result(make_resp_append_callback());
        #[cfg(target_os = "linux")]
        let builder =
            builder.kernel_timestamps(matches!(config.timestamps.mode, TimestampMode::Software));
        let mut client = builder.max_batch_size(append.pipeline_depth).build();

        // Same RESP3 negotiation as the reader connections.
        if matches!(config.target.protocol, CacheProtocol::Resp3)
            && let Err(e) = client
                .cmd(&resp_proto::Request::cmd(b"HELLO").arg(b"3"))
                .await
        {
            tracing::debug!(
                worker = state.task_state.worker_id,
                endpoint = %endpoint,
                "append writer HELLO 3 failed: {}",
                e
            );
            metrics::APPEND_CONNECTIONS_ACTIVE.decrement();
            metrics::CONNECTIONS_FAILED.increment();
            ringline::sleep(Duration::from_millis(100)).await;
            continue;
        }

        tracing::debug!(
            worker = state.task_state.worker_id,
            endpoint = %endpoint,
            conn_index = conn.index(),
            "connected (RESP, append writer)"
        );

        let result =
            drive_resp_append(&mut client, &state, endpoint_idx, &mut rng, &mut key_buf).await;

        metrics::APPEND_CONNECTIONS_ACTIVE.decrement();

        match result {
            Ok(()) => return,
            Err(reason) => record_disconnect_reason(reason),
        }

        ringline::sleep(Duration::from_millis(100)).await;
    }
}

/// Drain the append queue for this endpoint on a connected RESP client.
async fn drive_resp_append(
    client: &mut ringline_redis::Client,
    state: &Arc<SharedWorkerState>,
    endpoint_idx: usize,
    rng: &mut Xoshiro256PlusPlus,
    key_buf: &mut [u8],
) -> Result<(), DisconnectReason> {
    let config = &state.task_state.config;
    let append = config
        .workload
        .append
        .as_ref()
        .expect("append task requires [workload.append]");
    let value_len = config.workload.values.length;
    let pool_len = state.task_state.value_pool.len();
    let multi_endpoint = state.task_state.endpoints.len() > 1;
    let pipeline_depth = append.pipeline_depth;
    let queues = &state.task_state.append_queues;
    let mut in_flight: VecDeque<usize> = VecDeque::new();
    let mut inflight = InFlight::new(config.connection.request_timeout, pipeline_depth);

    loop {
        if state.task_state.shared.phase().should_stop() {
            return Ok(());
        }
        let fired_at = Instant::now();

        let free = pipeline_depth.saturating_sub(client.pending_count());
        if free > 0 && !queues.is_empty(endpoint_idx) {
            let mut budget = append_token_budget(&state.task_state.append_ratelimiter, free);
            while budget > 0 && client.pending_count() < pipeline_depth {
                let Some(key_id) = queues.pop_front(endpoint_idx) else {
                    break;
                };
                write_key(key_buf, key_id);
                // Hand a key to its new owner after a mid-run topology change.
                if multi_endpoint {
                    let routed = route_key(&state.task_state, key_buf);
                    if routed != endpoint_idx {
                        queues.push_back(routed, key_id);
                        continue;
                    }
                }
                budget -= 1;
                let user_data = key_id as u64 | APPEND_MARKER;
                let res = if value_len >= ZC_VALUE_THRESHOLD {
                    let guard =
                        make_value_guard(rng, &state.task_state.value_pool, value_len, pool_len);
                    client.fire_set_with_guard(key_buf, guard, user_data)
                } else {
                    let value = value_slice(rng, &state.task_state.value_pool, value_len, pool_len);
                    client.fire_set(key_buf, value, user_data)
                };
                match res {
                    Ok(_) => {
                        in_flight.push_back(key_id);
                        inflight.push(fired_at);
                    }
                    Err(_) => {
                        queues.push_front(endpoint_idx, key_id);
                        break;
                    }
                }
            }
        }

        if client.pending_count() == 0 {
            // Idle between batches (or paced out): poll the queue gently.
            ringline::sleep(Duration::from_millis(1)).await;
            continue;
        }

        let conn = client.conn();
        let result = match recv_with_timeout(client.recv_meta(), &mut inflight, conn).await {
            Ok(Ok(meta)) => map_respmeta(meta),
            Ok(Err(ringline_redis::Error::ConnectionClosed)) => {
                requeue_drained_append(&state.task_state, key_buf, in_flight.drain(..));
                return Err(DisconnectReason::Eof);
            }
            Ok(Err(_)) => {
                requeue_drained_append(&state.task_state, key_buf, in_flight.drain(..));
                return Err(DisconnectReason::RecvError);
            }
            Err(reason) => {
                requeue_drained_append(&state.task_state, key_buf, in_flight.drain(..));
                return Err(reason);
            }
        };
        inflight.pop();

        if result.append {
            let key_id = in_flight
                .pop_front()
                .or(result.key_id)
                .expect("append response without key_id");
            if result.success {
                confirm_append_key(state);
            } else {
                metrics::APPEND_SET_ERRORS.increment();
                queues.push_back(endpoint_idx, key_id);
            }
        }

        if let Some(ref redirect) = result.redirect {
            check_resp_redirect_parsed(redirect, state);
        }
    }
}

/// A Memcache append-stream writer connection (ASCII or binary).
async fn memcache_append_task(state: Arc<SharedWorkerState>, endpoint_idx: usize, seed: u64) {
    let endpoint = state.task_state.endpoints[endpoint_idx];
    let config = &state.task_state.config;
    let append = config
        .workload
        .append
        .as_ref()
        .expect("append task requires [workload.append]");
    let tls_name = resolve_tls_server_name(&state.task_state, endpoint);
    let mut rng = Xoshiro256PlusPlus::seed_from_u64(seed);
    let mut key_buf = vec![0u8; config.workload.keyspace.length];

    loop {
        if state.task_state.shared.phase().should_stop() {
            return;
        }

        let conn = match establish_connection(
            endpoint,
            state.task_state.worker_id,
            tls_name.as_deref(),
            state.task_state.config.connection.connect_timeout,
        )
        .await
        {
            Ok(conn) => conn,
            Err(_) => {
                ringline::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };

        metrics::APPEND_CONNECTIONS_ACTIVE.increment();
        let builder = ringline_memcache::Client::builder(conn)
            .on_result(make_memcache_append_callback())
            .max_batch_size(append.pipeline_depth);
        #[cfg(target_os = "linux")]
        let builder =
            builder.kernel_timestamps(matches!(config.timestamps.mode, TimestampMode::Software));
        let binary = matches!(config.target.protocol, CacheProtocol::MemcacheBinary);
        let mut client = if binary {
            McClient::Binary(builder.build_binary())
        } else {
            McClient::Ascii(builder.build())
        };

        tracing::debug!(
            worker = state.task_state.worker_id,
            endpoint = %endpoint,
            conn_index = conn.index(),
            "connected (Memcache, append writer)"
        );

        let result =
            drive_memcache_append(&mut client, &state, endpoint_idx, &mut rng, &mut key_buf).await;

        metrics::APPEND_CONNECTIONS_ACTIVE.decrement();

        match result {
            Ok(()) => return,
            Err(reason) => record_disconnect_reason(reason),
        }

        ringline::sleep(Duration::from_millis(100)).await;
    }
}

/// Drain the append queue for this endpoint on a connected Memcache client.
async fn drive_memcache_append(
    client: &mut McClient,
    state: &Arc<SharedWorkerState>,
    endpoint_idx: usize,
    rng: &mut Xoshiro256PlusPlus,
    key_buf: &mut [u8],
) -> Result<(), DisconnectReason> {
    let config = &state.task_state.config;
    let append = config
        .workload
        .append
        .as_ref()
        .expect("append task requires [workload.append]");
    let value_len = config.workload.values.length;
    let pool_len = state.task_state.value_pool.len();
    let multi_endpoint = state.task_state.endpoints.len() > 1;
    let pipeline_depth = append.pipeline_depth;
    let queues = &state.task_state.append_queues;
    let mut in_flight: VecDeque<usize> = VecDeque::new();
    let mut inflight = InFlight::new(config.connection.request_timeout, pipeline_depth);

    loop {
        if state.task_state.shared.phase().should_stop() {
            return Ok(());
        }
        let fired_at = Instant::now();

        let free = pipeline_depth.saturating_sub(client.pending_count());
        if free > 0 && !queues.is_empty(endpoint_idx) {
            let mut budget = append_token_budget(&state.task_state.append_ratelimiter, free);
            while budget > 0 && client.pending_count() < pipeline_depth {
                let Some(key_id) = queues.pop_front(endpoint_idx) else {
                    break;
                };
                write_key(key_buf, key_id);
                if multi_endpoint {
                    let routed = route_key(&state.task_state, key_buf);
                    if routed != endpoint_idx {
                        queues.push_back(routed, key_id);
                        continue;
                    }
                }
                budget -= 1;
                let guard =
                    make_value_guard(rng, &state.task_state.value_pool, value_len, pool_len);
                let user_data = key_id as u64 | APPEND_MARKER;
                match client.fire_set_with_guard(key_buf, guard, 0, 0, user_data) {
                    Ok(_) => {
                        in_flight.push_back(key_id);
                        inflight.push(fired_at);
                    }
                    Err(_) => {
                        queues.push_front(endpoint_idx, key_id);
                        break;
                    }
                }
            }
        }

        if client.pending_count() == 0 {
            ringline::sleep(Duration::from_millis(1)).await;
            continue;
        }

        let conn = client.conn();
        let op = match recv_with_timeout(client.recv(), &mut inflight, conn).await {
            Ok(Ok(op)) => op,
            Ok(Err(ringline_memcache::Error::ConnectionClosed)) => {
                requeue_drained_append(&state.task_state, key_buf, in_flight.drain(..));
                return Err(DisconnectReason::Eof);
            }
            Ok(Err(_)) => {
                requeue_drained_append(&state.task_state, key_buf, in_flight.drain(..));
                return Err(DisconnectReason::RecvError);
            }
            Err(reason) => {
                requeue_drained_append(&state.task_state, key_buf, in_flight.drain(..));
                return Err(reason);
            }
        };
        inflight.pop();
        let result = map_memcache_op(op);

        if result.append {
            let key_id = in_flight
                .pop_front()
                .or(result.key_id)
                .expect("append response without key_id");
            if result.success {
                confirm_append_key(state);
            } else {
                metrics::APPEND_SET_ERRORS.increment();
                queues.push_back(endpoint_idx, key_id);
            }
        }
    }
}

// ── Request timeout ──────────────────────────────────────────────────────
//
// `connection.request_timeout` was parsed and documented but never consulted:
// a request could sit in a pipeline for the whole run and the reported tail
// latency would include it (see #127, p999 of 8-12 s under a 1 s timeout).
// Replies on a pipelined connection are ordered, so a stalled head stalls
// every request behind it, and the only recovery is to abandon the connection:
// count everything in flight as timed out, close, reconnect.

/// Fire times of the requests in flight on one connection, oldest first.
/// Replies are FIFO, so the front entry is always the request the next reply
/// answers, and its age is the age of the oldest outstanding request.
///
/// Also owns the connection's deadline timer. The timer outlives individual
/// recvs: it is armed for the oldest request's deadline and left running when
/// that request's reply arrives. See `recv_with_timeout` for why.
struct InFlight {
    fired: VecDeque<Instant>,
    timeout: Duration,
    timer: Option<ringline::SleepFuture>,
}

impl InFlight {
    fn new(timeout: Duration, capacity: usize) -> Self {
        Self {
            fired: VecDeque::with_capacity(capacity),
            timeout,
            timer: None,
        }
    }

    #[inline]
    fn push(&mut self, fired_at: Instant) {
        self.fired.push_back(fired_at);
    }

    #[inline]
    fn pop(&mut self) {
        self.fired.pop_front();
    }

    fn len(&self) -> usize {
        self.fired.len()
    }

    /// Arm the deadline timer to fire after `after`. Returns false if the
    /// timer pool is exhausted.
    fn arm(&mut self, after: Duration) -> bool {
        match ringline::try_sleep(after) {
            Ok(t) => {
                self.timer = Some(t);
                true
            }
            Err(_) => false,
        }
    }

    /// Time left before the oldest in-flight request expires. `None` when no
    /// deadline applies (timeout disabled, or nothing in flight); `Some(ZERO)`
    /// when it has already expired.
    fn remaining(&self, now: Instant) -> Option<Duration> {
        if self.timeout.is_zero() {
            return None;
        }
        let oldest = *self.fired.front()?;
        Some(
            self.timeout
                .saturating_sub(now.saturating_duration_since(oldest)),
        )
    }
}

/// Await one reply under the request timeout. On expiry every request in
/// flight on this connection is counted as timed out (and as an error), the
/// connection is closed, and the caller gets `DisconnectReason::Timeout` so it
/// requeues its own bookkeeping and reconnects.
///
/// The deadline timer is not armed per recv. On io_uring, cancelling a pending
/// timeout (`io_timeout_cancel`) walks the ring's whole pending-timeout list,
/// which holds about one timer per connection, so arming a timeout for every
/// reply and cancelling it when the reply arrived made each reply cost
/// O(connections per worker). Profiled at 4096 closed-loop connections on 8
/// workers, that cancel was 69% of generator CPU and pinned every worker; with
/// the timeout disabled the same cell used 2.16 cores and ran 22% faster.
///
/// Instead the timer lives in `InFlight` across recvs. It is armed for the
/// oldest request's deadline and left running when that reply arrives. The
/// oldest request only gets younger as replies arrive, so a running timer never
/// fires later than the true deadline. When it fires, the deadline is checked
/// against the current oldest request: expired means timeout, otherwise the
/// timer is re-armed for what remains. A healthy connection re-arms about once
/// per `timeout` and never cancels.
async fn recv_with_timeout<F, T>(
    recv: F,
    inflight: &mut InFlight,
    conn: ConnCtx,
) -> Result<T, DisconnectReason>
where
    F: Future<Output = T>,
{
    // No request in flight or the timeout is disabled. A timer left armed from
    // an earlier recv keeps running; it is checked on the next bounded recv.
    let Some(remaining) = inflight.remaining(Instant::now()) else {
        return Ok(recv.await);
    };
    if inflight.timer.is_none() && !inflight.arm(remaining) {
        // Timer pool exhausted. The pool is sized from the connection count in
        // the runner, so this should not happen. Without a timer the recv
        // cannot be bounded: drop the connection, let the caller reconnect,
        // and say why once.
        timer_exhausted_warn_once();
        conn.close();
        return Err(DisconnectReason::RecvError);
    }

    let mut recv = std::pin::pin!(recv);
    let expired = std::future::poll_fn(|cx| {
        if let Poll::Ready(v) = recv.as_mut().poll(cx) {
            return Poll::Ready(Ok(v));
        }
        loop {
            let Some(timer) = inflight.timer.as_mut() else {
                // Re-arming failed below; treat as a recv error, as above.
                return Poll::Ready(Err(false));
            };
            if Pin::new(timer).poll(cx).is_pending() {
                return Poll::Pending;
            }
            inflight.timer = None;
            // `InFlight` does not change while this recv is pending, so there
            // is always a request in flight here.
            match inflight.remaining(Instant::now()) {
                Some(left) if !left.is_zero() => {
                    if !inflight.arm(left) {
                        timer_exhausted_warn_once();
                    }
                }
                _ => return Poll::Ready(Err(true)),
            }
        }
    })
    .await;

    match expired {
        Ok(v) => Ok(v),
        Err(true) => {
            let n = inflight.len() as u64;
            metrics::REQUEST_TIMEOUTS.add(n);
            metrics::REQUEST_ERRORS.add(n);
            conn.close();
            Err(DisconnectReason::Timeout)
        }
        Err(false) => {
            conn.close();
            Err(DisconnectReason::RecvError)
        }
    }
}

fn timer_exhausted_warn_once() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        tracing::warn!(
            "ringline timer pool exhausted: a request timeout could not be armed and the \
             connection was dropped; this indicates timer_slots is undersized for the \
             connection count"
        );
    });
}

/// Connect path when the timer pool is exhausted: warn once, then report the
/// attempt as failed so the caller's retry loop tries again.
fn connect_timer_exhausted() -> Result<ConnCtx, std::io::Error> {
    timer_exhausted_warn_once();
    Err(std::io::Error::other("timer pool exhausted"))
}

#[cfg(test)]
mod inflight_tests {
    use super::InFlight;
    use std::time::{Duration, Instant};

    #[test]
    fn no_deadline_when_disabled_or_idle() {
        let now = Instant::now();
        let mut idle = InFlight::new(Duration::from_secs(1), 4);
        assert_eq!(idle.remaining(now), None, "nothing in flight");
        idle.push(now);
        let disabled = InFlight::new(Duration::ZERO, 4);
        let mut disabled = disabled;
        disabled.push(now);
        assert_eq!(disabled.remaining(now), None, "zero timeout disables");
    }

    #[test]
    fn deadline_tracks_the_oldest_request() {
        let t0 = Instant::now();
        let mut f = InFlight::new(Duration::from_millis(1000), 4);
        f.push(t0);
        f.push(t0 + Duration::from_millis(600));
        assert_eq!(
            f.remaining(t0 + Duration::from_millis(300)),
            Some(Duration::from_millis(700))
        );
        // Oldest answered: the deadline now belongs to the second request.
        f.pop();
        assert_eq!(
            f.remaining(t0 + Duration::from_millis(300)),
            Some(Duration::from_millis(1300).min(Duration::from_millis(1000)))
        );
        assert_eq!(f.len(), 1);
    }

    #[test]
    fn expired_request_reports_zero_not_underflow() {
        let t0 = Instant::now();
        let mut f = InFlight::new(Duration::from_millis(100), 4);
        f.push(t0);
        assert_eq!(
            f.remaining(t0 + Duration::from_secs(5)),
            Some(Duration::ZERO)
        );
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
        DisconnectReason::Timeout => metrics::DISCONNECTS_TIMEOUT.increment(),
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

/// Upper bound on random-key draws when searching for one that routes to this
/// endpoint. Expected draws to find a match is `num_endpoints` under uniform
/// hashing; the multiplier provides enough headroom that hitting the cap is
/// vanishingly unlikely in a healthy setup, while still bounding CPU if the
/// hash distribution is degenerate (e.g. a tiny keyspace).
fn max_routing_attempts(num_endpoints: usize) -> usize {
    num_endpoints.saturating_mul(64).max(64)
}

/// Route a key to an endpoint index.
fn route_key(state: &TaskSharedState, key: &[u8]) -> usize {
    let slot_table = state.slot_table.read().unwrap();
    route_partition(&slot_table, &state.ring, state.endpoints.len(), key)
}

/// Drain in-flight prefill keys on disconnect and re-route each one against
/// the current slot table before pushing back to the owning endpoint's
/// queue. Mid-flight topology changes can mean the original endpoint no
/// longer owns the slot; pushing back blindly would let the key stall in
/// a queue that's never going to drain on this endpoint.
fn requeue_drained_prefill(
    state: &TaskSharedState,
    key_buf: &mut [u8],
    drained: impl IntoIterator<Item = usize>,
) {
    for key_id in drained {
        write_key(key_buf, key_id);
        let routed = route_key(state, key_buf);
        state.prefill_queues.push_back(routed, key_id);
    }
}

/// Route a key using the supplied slot table / ring directly. Used at init
/// time when partitioning the prefill queue, before `TaskSharedState` exists.
fn route_partition(
    slot_table: &Option<Vec<u16>>,
    ring: &ketama::Ring,
    num_endpoints: usize,
    key: &[u8],
) -> usize {
    if let Some(table) = slot_table.as_ref() {
        let slot = resp_proto::hash_slot(key);
        table[slot as usize] as usize
    } else if num_endpoints <= 1 {
        0
    } else {
        ring.route(key)
    }
}

/// Build the shared per-endpoint prefill queues for the full keyspace.
///
/// Called once by the runner. Each key id `0..key_count` is routed to its
/// owning endpoint (slot table in cluster mode, ketama ring otherwise) and
/// pushed to that endpoint's queue. All workers then share the result via
/// `Arc`, so any worker's connection-task can drain the queue for the
/// endpoint it serves — fixing the stall when connections-per-worker is fewer
/// than the cluster node count.
pub(crate) fn build_prefill_queues(
    key_count: usize,
    key_len: usize,
    endpoints: &[SocketAddr],
    slot_table: &Option<Vec<u16>>,
) -> PrefillQueues {
    let n = endpoints.len().max(1);
    let ring = if endpoints.is_empty() {
        ketama::Ring::build(&["_"])
    } else {
        let ids: Vec<String> = endpoints.iter().map(|a| a.to_string()).collect();
        ketama::Ring::build(&ids.iter().map(|s| s.as_str()).collect::<Vec<_>>())
    };
    let queues = PrefillQueues::new(n);
    let mut key_buf = vec![0u8; key_len];
    for key_id in 0..key_count {
        write_key(&mut key_buf, key_id);
        let ep = route_partition(slot_table, &ring, n, &key_buf);
        queues.push_back(ep, key_id);
    }
    queues
}

/// Partition the full keyspace into per-endpoint key-id lists for steady-state
/// key selection. `result[ep]` holds every key-id whose slot is owned by
/// endpoint `ep`, letting a connection task pick a key for its endpoint by
/// indexing a precomputed list (O(1)) instead of rejection-sampling random
/// keys and re-routing each one — the latter made CRC16 slot routing ~40% of
/// CPU at high pipeline depth on a multi-shard cluster.
///
/// Returns an empty vector for the single-endpoint case; callers fall back to a
/// plain random key-id over the whole keyspace (no routing needed). Routing
/// uses the same slot table / ketama ring as `build_prefill_queues`, so the
/// partition matches the prefill partition.
pub(crate) fn build_endpoint_keys(
    key_count: usize,
    key_len: usize,
    endpoints: &[SocketAddr],
    slot_table: &Option<Vec<u16>>,
) -> Vec<Vec<u32>> {
    let n = endpoints.len();
    if n <= 1 {
        return Vec::new();
    }
    let ring = {
        let ids: Vec<String> = endpoints.iter().map(|a| a.to_string()).collect();
        ketama::Ring::build(&ids.iter().map(|s| s.as_str()).collect::<Vec<_>>())
    };
    let mut buckets: Vec<Vec<u32>> = vec![Vec::new(); n];
    let mut key_buf = vec![0u8; key_len];
    for key_id in 0..key_count {
        write_key(&mut key_buf, key_id);
        let ep = route_partition(slot_table, &ring, n, &key_buf);
        buckets[ep].push(key_id as u32);
    }
    buckets
}

/// Write a numeric key ID into the buffer as hex.
/// Run-scoped key rendering format (0 = hex, 1 = uuid), set from config in
/// `run_benchmark_full` before any key is generated. See `config::KeyFormat`.
pub(crate) static KEY_FORMAT: AtomicU8 = AtomicU8::new(0);

fn write_key(buf: &mut [u8], id: usize) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    // Canonical UUID form: 8-4-4-4-12 hex with dashes at indices 8/13/18/23.
    // A typical UUID parser accepts any hex in those positions, and the trailing
    // hex chars stay hex, so servers that shard their directories by the key's
    // last hex chars spread keys evenly across them.
    if crate::config::KeyFormat::from_u8(KEY_FORMAT.load(Ordering::Relaxed))
        == crate::config::KeyFormat::Uuid
        && buf.len() == 36
    {
        let mut n = id as u128;
        for (i, byte) in buf.iter_mut().enumerate().rev() {
            if matches!(i, 8 | 13 | 18 | 23) {
                *byte = b'-';
            } else {
                *byte = HEX[(n & 0xf) as usize];
                n >>= 4;
            }
        }
        return;
    }
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

/// CPU pinning is Linux-only (relies on `sched_setaffinity`).
#[cfg(not(target_os = "linux"))]
fn pin_to_cpu(_cpu_id: usize) -> std::io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "CPU pinning is only supported on Linux",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A dispatcher with a slot already queued, without needing an executor:
    /// `dispatch` and `drain` are synchronous, and they are where every bug in
    /// this type has been so far.
    fn queued(d: &TokenDispatcher, want: u64) -> Rc<TokenSlot> {
        let slot = Rc::new(TokenSlot {
            want,
            granted: Cell::new(0),
            waker: RefCell::new(None),
        });
        d.queue.borrow_mut().push_back(Rc::clone(&slot));
        slot
    }

    fn limiter(rate: u64) -> Ratelimiter {
        Ratelimiter::builder(rate)
            .max_tokens(rate.max(1))
            .initial_available(rate)
            .build()
            .expect("limiter")
    }

    #[test]
    fn dispatch_funds_what_the_limiter_can_pay_for() {
        let d = TokenDispatcher::default();
        let a = queued(&d, 1);
        let b = queued(&d, 1);
        d.dispatch(&limiter(100));
        assert_eq!(a.granted.get(), 1);
        assert_eq!(b.granted.get(), 1);
        assert!(
            d.queue.borrow().is_empty(),
            "funded slots must leave the queue"
        );
    }

    #[test]
    fn an_unfundable_head_keeps_its_place() {
        // The head is what the limiter cannot pay for, so nothing behind it is
        // reachable either -- service is FIFO. It must stay queued rather than
        // be dropped, or the waiter never gets funded at all.
        let d = TokenDispatcher::default();
        let head = queued(&d, 10);
        let rl = limiter(10);
        assert!(rl.try_wait_n(10).is_ok(), "drain the bucket");
        let nap = d.dispatch(&rl);
        assert_eq!(head.granted.get(), 0);
        assert_eq!(d.queue.borrow().len(), 1, "head must be put back");
        assert!(nap.is_some(), "caller needs to know how long to wait");
    }

    #[test]
    fn an_empty_queue_reports_nothing_to_wait_for() {
        let d = TokenDispatcher::default();
        assert!(d.dispatch(&limiter(100)).is_none());
    }

    #[test]
    fn drain_releases_every_waiter_and_closes_for_good() {
        // `drain` can only reach whoever is queued at that instant. Closing has
        // to be sticky, or a connection arriving afterwards queues behind a
        // dispatcher that has exited and -- the wait being untimed -- hangs
        // shutdown.
        let d = TokenDispatcher::default();
        let slot = queued(&d, 1);
        d.drain();
        assert_eq!(slot.granted.get(), 0, "released, not funded");
        assert!(d.queue.borrow().is_empty());
        assert!(d.closed.get(), "close must persist past the drain");
    }

    #[test]
    fn a_claim_never_outlives_its_funding() {
        // The inverse of the token leak: the limiter is debited only for slots
        // that actually get granted, so a run cannot be charged for requests it
        // never sends.
        let d = TokenDispatcher::default();
        let rl = limiter(4);
        for _ in 0..6 {
            queued(&d, 1);
        }
        d.dispatch(&rl);
        let funded: u64 = d
            .queue
            .borrow()
            .iter()
            .map(|s| s.granted.get())
            .sum::<u64>();
        assert_eq!(funded, 0, "anything still queued must be unfunded");
        assert_eq!(d.queue.borrow().len(), 2, "4 of 6 fundable at rate 4");
    }

    /// Build a minimal TaskSharedState + SharedWorkerState for testing prefill logic.
    fn make_worker_state(
        shared: Arc<SharedState>,
        worker_id: usize,
        prefill_total: usize,
    ) -> Arc<SharedWorkerState> {
        let config: Config = toml::from_str(
            r#"
            [target]
            endpoints = ["127.0.0.1:6379"]
            "#,
        )
        .unwrap();

        let task_state = Arc::new(TaskSharedState {
            key_dist: Arc::new(KeyDist::uniform(1)),
            config,
            shared,
            ratelimiter: None,
            value_pool: Arc::new(vec![0u8; 64]),
            endpoints: vec!["127.0.0.1:6379".parse().unwrap()],
            ring: ketama::Ring::build(&["127.0.0.1:6379"]),
            slot_table: RwLock::new(None),
            prefill_queues: Arc::new(PrefillQueues::new(1)),
            append_queues: Arc::new(PrefillQueues::new(1)),
            append_ratelimiter: None,
            endpoint_keys: Arc::new(Vec::new()),
            worker_id,
            backfill_on_miss: false,
            tls_enabled: false,
            tls_server_name: None,
        });

        Arc::new(SharedWorkerState {
            task_state,
            prefill_total,
            prefill_confirmed: AtomicUsize::new(0),
            prefill_done: AtomicU8::new(0),
            precheck_done: AtomicU8::new(0),
        })
    }

    #[test]
    fn confirm_prefill_key_single_worker() {
        let shared = Arc::new(SharedState::new());
        let worker = make_worker_state(Arc::clone(&shared), 0, 3);

        // Confirm 2 of 3 keys — worker should not be done yet.
        confirm_prefill_key(&worker);
        confirm_prefill_key(&worker);
        assert!(!worker.is_prefill_done());
        assert_eq!(shared.prefill_complete_count(), 0);

        // Confirm the 3rd key — worker should now be done.
        confirm_prefill_key(&worker);
        assert!(worker.is_prefill_done());
        assert_eq!(shared.prefill_complete_count(), 1);
        assert_eq!(shared.prefill_keys_confirmed(), 3);
    }

    #[test]
    fn confirm_prefill_key_multiple_workers_independent() {
        // Two workers, each with 50 keys. Interleaved confirmations must not
        // cause one worker to finish early due to the other's progress.
        let shared = Arc::new(SharedState::new());
        let worker0 = make_worker_state(Arc::clone(&shared), 0, 50);
        let worker1 = make_worker_state(Arc::clone(&shared), 1, 50);

        // Worker 1 confirms all 50 of its keys first.
        for _ in 0..50 {
            confirm_prefill_key(&worker1);
        }
        assert!(worker1.is_prefill_done());
        assert_eq!(shared.prefill_complete_count(), 1);

        // Worker 0 has confirmed 0 keys — must NOT be done even though
        // global confirmed (50) >= worker 0's total (50).
        assert!(!worker0.is_prefill_done());

        // Worker 0 confirms 49 of its 50 keys — still not done.
        for _ in 0..49 {
            confirm_prefill_key(&worker0);
        }
        assert!(!worker0.is_prefill_done());
        assert_eq!(shared.prefill_complete_count(), 1);

        // Worker 0 confirms its last key — now done.
        confirm_prefill_key(&worker0);
        assert!(worker0.is_prefill_done());
        assert_eq!(shared.prefill_complete_count(), 2);
        assert_eq!(shared.prefill_keys_confirmed(), 100);
    }

    #[test]
    fn confirm_prefill_key_marks_complete_only_once() {
        let shared = Arc::new(SharedState::new());
        let worker = make_worker_state(Arc::clone(&shared), 0, 2);

        confirm_prefill_key(&worker);
        confirm_prefill_key(&worker);
        assert!(worker.is_prefill_done());
        assert_eq!(shared.prefill_complete_count(), 1);

        // Extra confirmations (e.g. late responses) should not double-count.
        confirm_prefill_key(&worker);
        assert_eq!(shared.prefill_complete_count(), 1);
    }

    #[test]
    fn confirm_prefill_key_zero_total_never_completes() {
        let shared = Arc::new(SharedState::new());
        let worker = make_worker_state(Arc::clone(&shared), 0, 0);

        // A worker with prefill_total=0 should never trigger completion
        // via confirm_prefill_key (it's handled at init time instead).
        confirm_prefill_key(&worker);
        assert_eq!(shared.prefill_complete_count(), 0);
    }
}
