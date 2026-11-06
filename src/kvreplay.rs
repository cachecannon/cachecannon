//! KV-cache prefix replay (`[workload.kv_replay]`).
//!
//! Replays a kvtrace request trace against the server as an LLM serving
//! engine's KV-cache prefix store would use it, and reports how many prompt
//! tokens were served from the cache. The request-to-object mapping (object
//! ids and sizes for a model geometry) is kvtrace's `ObjectMapper`.
//!
//! One trace request on one connection, modelling LMCache v0.5.5 (05a013b)
//! with a Redis-protocol remote backend under vLLM v0.31.0 (db9527a):
//!
//! 1. Lookup: one EXISTS per object of every full chunk on every storing
//!    rank, all sent in one pipeline. The hit prefix is the chunks before the
//!    first chunk with a missing object on any rank, which is the minimum of
//!    the per-rank prefixes.
//! 2. Load: GET every object in the hit prefix, at most
//!    `connection.pipeline_depth` in flight. A GET that returns nil (the
//!    object was evicted after its EXISTS) is counted in `kv_get_nil`, and the
//!    cached prefix ends at its chunk; a GET answered with an error ends it
//!    the same way and is counted in `request_errors`.
//! 3. Store: SET the objects the write rule selects
//!    ([`KvWriteRule`]), at most `pipeline_depth` in flight. The store starts
//!    at the lookup's prefix, not the loaded one: LMCache records the lookup
//!    result as already saved (`num_saved_tokens = lmcache_cached_tokens`,
//!    `lmcache/integration/vllm/vllm_v1_adapter.py:193,247`) and skips that
//!    many leading tokens when it stores (`:326`).
//!
//! Token accounting is in [`cached_tokens`].
//!
//! Timing: the trace's timestamps divided by `speedup` (open loop), or as
//! fast as connections free up (`speedup = 0`, closed loop). Each worker runs
//! one dispatcher task that hands trace requests to its idle connections and
//! sleeps until a request's scheduled time, so the schedule costs one timer
//! per worker rather than one per connection (see `CLAUDE.md`, "Price any
//! per-request timer").

use crate::config::{KvKeyFormat, KvWriteRule};
use crate::metrics;
use crate::output::{KvReplayResults, LatencyStats};
use crate::worker::{
    DisconnectReason, IDLE_SLEEP_MAX, InFlight, Phase, SharedState, ZC_VALUE_THRESHOLD,
    make_value_guard, map_respmeta, note_discarded, note_failed_fire, record_counters,
    recv_with_timeout, value_slice,
};
use kvtrace::{Geometry, KvObject, ModelSizes, ObjectMapper, Request, RequestReader};
use metriken::histogram::Histogram;
use rand_xoshiro::Xoshiro256PlusPlus;
use ringline::ConnCtx;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::fs::File;
use std::future::Future;
use std::io::{BufWriter, Write};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

/// Header of the `requests_out` CSV. One row per trace request that
/// completed; see `docs/reference.md` "KV-cache replay" for the columns.
pub const REQUESTS_OUT_HEADER: &str = "req_id,timestamp_ns,lag_ns,input_tokens,chunks,\
lookup_hit_chunks,hit_chunks,cached_tokens,new_tokens,gets,get_nil,sets,lookup_ns,load_ns,\
store_ns,recorded";

/// Trace requests the reader thread may hold ready ahead of the replay.
/// The replay holds at most this many requests in the channel, plus one in
/// the reader's blocked send, its current decoded batch, one held by each
/// worker's dispatcher, those handed back after a closed connection, and one
/// in flight per connection, whatever the file's length.
const READ_AHEAD: usize = 16_384;

/// A trace being replayed and the state of its replay, shared by every
/// worker.
pub struct KvReplay {
    /// Requests in file order, which kvtrace requires to be arrival order,
    /// from the reader thread (`spawn_reader`). Disconnected once the reader
    /// has stopped and every request it sent has been taken.
    requests: crossbeam_channel::Receiver<Request>,
    /// What the reader thread has done; see [`ReaderState`].
    reader: Arc<ReaderState>,
    /// Requests in the file, from its footer.
    trace_requests: usize,
    /// Timestamp of the first request; trace time 0.
    first_ts: i64,
    chunk_tokens: u64,
    mapper: ObjectMapper,
    key_prefix: Vec<u8>,
    key_format: KvKeyFormat,
    write_rule: KvWriteRule,
    /// 0 for closed loop.
    speedup: f64,
    /// Objects of the request with the most, for sizing the send pool.
    max_objects: usize,
    /// When trace time 0 is, set by the runner as warmup starts.
    start: OnceLock<Instant>,
    /// Requests finished, replayed or failed.
    finished: AtomicUsize,
    log: Option<Mutex<BufWriter<File>>>,
}

impl KvReplay {
    /// Read the model geometry and the trace's first request, open
    /// `requests_out`, and start the thread that reads the rest of the trace.
    pub fn load(cfg: &crate::config::KvReplay) -> Result<Self, String> {
        let geometry = geometry(cfg)?;
        let trace_err = |e: kvtrace::Error| format!("kv_replay trace {}: {e}", cfg.trace.display());
        let mut reader = RequestReader::open(&cfg.trace).map_err(trace_err)?;
        let chunk_tokens = u64::from(reader.chunk_tokens());
        let trace_requests = reader.total_requests();
        let max_chunks = reader.max_chunks().map_err(trace_err)?;
        // With nothing to replay the run would end at once and report
        // nothing recorded, which reads as a timing problem.
        let Some(first) = reader.next().transpose().map_err(trace_err)? else {
            return Err(format!(
                "kv_replay trace {}: no requests",
                cfg.trace.display()
            ));
        };
        let mapper = ObjectMapper::new(&geometry, chunk_tokens)
            .map_err(|e| format!("kv_replay geometry: {e}"))?;

        let object_size = mapper.max_object_size();
        if object_size == 0 || object_size > crate::runner::VALUE_POOL_SIZE as u64 {
            return Err(format!(
                "kv_replay: an object is {object_size} bytes ({} tokens per chunk); it must be \
                 between 1 and the value pool size ({} bytes)",
                chunk_tokens,
                crate::runner::VALUE_POOL_SIZE
            ));
        }
        let ranks = mapper.sharding().ranks as usize;
        let max_objects = max_chunks.saturating_mul(ranks);

        let log = match cfg.requests_out {
            Some(ref path) => {
                let mut w = BufWriter::new(
                    File::create(path)
                        .map_err(|e| format!("kv_replay requests_out {}: {e}", path.display()))?,
                );
                writeln!(w, "{REQUESTS_OUT_HEADER}")
                    .map_err(|e| format!("kv_replay requests_out {}: {e}", path.display()))?;
                Some(Mutex::new(w))
            }
            None => None,
        };

        tracing::info!(
            requests = trace_requests,
            chunk_tokens,
            ranks,
            object_bytes = object_size,
            timing = if cfg.speedup == 0.0 {
                "closed-loop".to_string()
            } else {
                format!("open-loop at {}x", cfg.speedup)
            },
            write_rule = cfg.write_rule.as_str(),
            "kv replay trace opened"
        );

        let first_ts = first.timestamp;
        let (requests, state) = spawn_reader(cfg.trace.clone(), reader, first)?;
        Ok(Self {
            requests,
            reader: state,
            trace_requests,
            first_ts,
            chunk_tokens,
            mapper,
            key_prefix: cfg.key_prefix.as_bytes().to_vec(),
            key_format: cfg.key_format,
            write_rule: cfg.write_rule,
            speedup: cfg.speedup,
            max_objects,
            start: OnceLock::new(),
            finished: AtomicUsize::new(0),
            log,
        })
    }

    /// Requests in the trace file.
    pub fn len(&self) -> usize {
        self.trace_requests
    }

    pub fn is_empty(&self) -> bool {
        self.trace_requests == 0
    }

    /// Block until the reader thread has filled its read-ahead or stopped, so
    /// that a read error in the first `READ_AHEAD` (16,384) requests is known before
    /// any request is due. Returns early once `running` is false. Called by
    /// the runner as precheck completes.
    pub fn wait_read_ahead(&self, running: &AtomicBool) {
        while !self.reader.stopped.load(Ordering::Acquire)
            && !self.requests.is_full()
            && running.load(Ordering::SeqCst)
        {
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// The error that stopped the reader thread before the end of the
    /// trace, if one did. The requests after it were not replayed.
    pub fn trace_error(&self) -> Option<String> {
        self.reader.error.get().cloned()
    }

    pub fn closed_loop(&self) -> bool {
        self.speedup == 0.0
    }

    /// The largest object, in bytes: the SET value length the send pool is
    /// sized for.
    pub fn max_object_size(&self) -> usize {
        self.mapper.max_object_size() as usize
    }

    /// EXISTS one request can send at once.
    pub fn max_objects(&self) -> usize {
        self.max_objects
    }

    /// Length of the longest key.
    pub fn max_key_len(&self) -> usize {
        let digits = match self.key_format {
            KvKeyFormat::Hex => 16,
            KvKeyFormat::Decimal => 20,
        };
        self.key_prefix.len() + digits
    }

    /// Start the replay clock. Called once, as warmup starts; later calls do
    /// nothing.
    pub fn start_at(&self, at: Instant) {
        let _ = self.start.set(at);
    }

    /// True once the reader thread has stopped and every request it sent
    /// has finished: the whole trace, or the requests before a read error.
    pub fn is_finished(&self) -> bool {
        self.reader.stopped.load(Ordering::Acquire)
            && self.finished.load(Ordering::Acquire) >= self.reader.sent.load(Ordering::Acquire)
    }

    /// Flush the `requests_out` file.
    pub fn flush_log(&self) {
        if let Some(ref log) = self.log
            && let Err(e) = log.lock().unwrap().flush()
        {
            tracing::error!("kv_replay requests_out: {e}");
        }
    }

    /// Claim the next trace request.
    fn claim(&self) -> Claim {
        match self.requests.try_recv() {
            Ok(req) => Claim::Request(req),
            Err(crossbeam_channel::TryRecvError::Empty) => Claim::NotReady,
            Err(crossbeam_channel::TryRecvError::Disconnected) => Claim::Exhausted,
        }
    }

    /// When `req` is due, in open loop.
    fn scheduled_at(&self, start: Instant, req: &Request) -> Option<Instant> {
        if self.closed_loop() {
            return None;
        }
        let trace_ns = req.timestamp.saturating_sub(self.first_ts) as f64;
        Some(start + Duration::from_nanos((trace_ns / self.speedup) as u64))
    }

    fn mark_finished(&self) {
        self.finished.fetch_add(1, Ordering::Release);
    }

    /// Append `key_prefix` and `id` to `buf`.
    fn push_key(&self, buf: &mut Vec<u8>, id: u64) {
        buf.extend_from_slice(&self.key_prefix);
        match self.key_format {
            KvKeyFormat::Hex => write!(buf, "{id:016x}"),
            KvKeyFormat::Decimal => write!(buf, "{id}"),
        }
        .expect("writing to a Vec cannot fail");
    }

    fn write_row(&self, req: &Request, lag_ns: Option<u64>, o: &Outcome, recorded: bool) {
        let Some(ref log) = self.log else {
            return;
        };
        let lag = lag_ns.map_or(String::new(), |l| l.to_string());
        let mut w = log.lock().unwrap();
        if let Err(e) = writeln!(
            w,
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
            req.req_id,
            req.timestamp,
            lag,
            req.input_tokens,
            req.chunks.len(),
            o.lookup_hit,
            o.hit,
            o.cached_tokens,
            o.new_tokens,
            o.gets,
            o.get_nil,
            o.sets,
            o.lookup_ns,
            o.load_ns,
            o.store_ns,
            u8::from(recorded),
        ) {
            tracing::error!("kv_replay requests_out: {e}");
        }
    }
}

/// What the reader thread has done, for deciding when the replay is over.
#[derive(Default)]
struct ReaderState {
    /// Requests sent into the channel, counted before each send.
    sent: AtomicUsize,
    /// Set once the thread has sent its last request.
    stopped: AtomicBool,
    /// The read error that stopped it before the end of the trace.
    error: OnceLock<String>,
}

/// The result of [`KvReplay::claim`].
enum Claim {
    Request(Request),
    /// The reader thread has not read the next request yet.
    NotReady,
    /// Every request has been handed out.
    Exhausted,
}

/// Start the thread that reads `reader` (positioned after `first`) into a
/// channel of at most [`READ_AHEAD`] requests, `first` first. The thread
/// stops at the end of the trace, at a read error (recorded in
/// [`ReaderState::error`] and logged), or when the receiver is dropped.
fn spawn_reader(
    path: std::path::PathBuf,
    reader: RequestReader,
    first: Request,
) -> Result<(crossbeam_channel::Receiver<Request>, Arc<ReaderState>), String> {
    let (tx, rx) = crossbeam_channel::bounded(READ_AHEAD);
    let state = Arc::new(ReaderState::default());
    let thread_state = Arc::clone(&state);
    std::thread::Builder::new()
        .name("kv-trace-reader".into())
        .spawn(move || {
            let state = thread_state;
            // Sets `stopped` on every exit, a panic included, before `tx` is
            // dropped, so a dispatcher that sees the channel disconnected
            // can rely on it; a panic is recorded as the error.
            struct Stopped<'a>(&'a ReaderState);
            impl Drop for Stopped<'_> {
                fn drop(&mut self) {
                    if std::thread::panicking() {
                        let _ = self
                            .0
                            .error
                            .set("kv_replay: the trace reader thread panicked".into());
                    }
                    self.0.stopped.store(true, Ordering::Release);
                }
            }
            let _stopped = Stopped(&state);
            for req in std::iter::once(Ok(first)).chain(reader) {
                match req {
                    Ok(req) => {
                        state.sent.fetch_add(1, Ordering::AcqRel);
                        if tx.send(req).is_err() {
                            // The replay was dropped: the run is over.
                            break;
                        }
                    }
                    Err(e) => {
                        let msg = format!("kv_replay trace {}: {e}", path.display());
                        tracing::error!("{msg}; the requests after it are not replayed");
                        let _ = state.error.set(msg);
                        break;
                    }
                }
            }
        })
        .map_err(|e| format!("kv_replay: cannot start the trace reader thread: {e}"))?;
    Ok((rx, state))
}

/// The model geometry `cfg` describes.
///
/// Hybrid models (linear-attention layers with a recurrent state) are
/// rejected: kvtrace's hybrid geometry is an approximation whose object
/// sizes, allowed chunk length and snapshot count differ from vLLM and
/// LMCache (kvtrace `objects` module docs, "Hybrid models").
pub fn geometry(cfg: &crate::config::KvReplay) -> Result<Geometry, String> {
    let sizes = match (&cfg.model_config, cfg.kv_bytes_per_token) {
        (Some(path), _) => ModelSizes::from_config_file(path, cfg.kv_dtype.as_deref())
            .map_err(|e| format!("kv_replay model_config {}: {e}", path.display()))?,
        (None, Some(bytes)) => {
            let mut sizes = ModelSizes::explicit(bytes, 0);
            sizes.mla = cfg.mla;
            sizes
        }
        (None, None) => return Err("kv_replay: no model geometry".into()),
    };
    if sizes.state_bytes_per_sequence > 0 {
        return Err(format!(
            "kv_replay: the model is a hybrid with {} recurrent-state bytes per sequence. \
             Hybrid models are not supported: kvtrace's hybrid geometry is an approximation \
             (object sizes, chunk length and snapshot count differ from vLLM and LMCache)",
            sizes.state_bytes_per_sequence
        ));
    }
    let mut geometry = Geometry::new(sizes);
    geometry.tp = cfg.tp;
    // Checks the TP degree against the head counts.
    geometry
        .sharding()
        .map_err(|e| format!("kv_replay geometry: {e}"))?;
    Ok(geometry)
}

/// Chunks of the lookup's prefix hit: the chunks before the first one with a
/// missing object on any rank. `exists` is in `ObjectMapper::objects` order,
/// chunk by chunk with `ranks` objects each.
pub fn lookup_prefix(exists: &[bool], ranks: usize) -> usize {
    exists
        .chunks(ranks.max(1))
        .take_while(|chunk| chunk.iter().all(|&e| e))
        .count()
}

/// Prompt tokens served from the cache when the first `hit_chunks` chunks of
/// a prompt of `input_tokens` were loaded.
///
/// `hit_chunks * chunk_tokens`, except when that covers the whole prompt:
/// vLLM must compute at least the last prompt token to produce output, so
/// LMCache reports one token fewer on a full-prompt hit
/// (`lmcache/integration/vllm/vllm_v1_adapter.py:1443-1445`, "In full-prompt-hit
/// case, we need to recompute the last token"). A trace whose `input_tokens`
/// is below its full chunks' tokens is capped the same way, and a prompt of 0
/// tokens has 0 cached: kvtrace keeps `input_tokens` as the source gives it,
/// so it can disagree with the chunks. The result never exceeds
/// `input_tokens`.
pub fn cached_tokens(hit_chunks: u64, chunk_tokens: u64, input_tokens: u64) -> u64 {
    let hit = hit_chunks.saturating_mul(chunk_tokens);
    if hit >= input_tokens {
        input_tokens.saturating_sub(1)
    } else {
        hit
    }
}

/// What one trace request did.
#[derive(Debug, Default)]
struct Outcome {
    /// Chunks of the lookup's prefix hit.
    lookup_hit: usize,
    /// Chunks of the prefix actually loaded: the lookup's, cut at the first
    /// GET that returned nil or an error.
    hit: usize,
    cached_tokens: u64,
    new_tokens: u64,
    /// The loaded prefix covered the whole prompt.
    full_hit: bool,
    gets: usize,
    get_nil: usize,
    sets: usize,
    lookup_ns: u64,
    load_ns: u64,
    store_ns: u64,
}

/// Why a trace request did not complete.
enum Failure {
    /// The connection must be dropped.
    Disconnect(DisconnectReason),
    /// The server answered the lookup with an error, or the lookup could not
    /// be sent; the connection is still usable.
    Request,
    /// The connection was closed before the server answered any operation of
    /// the request, as when the server closes an idle connection. Nothing was
    /// written, so the request can run again on another connection.
    Closed,
}

// ── Dispatcher ─────────────────────────────────────────────────────────────

/// A trace request handed to a connection.
#[derive(Debug)]
struct KvJob {
    req: Request,
    /// When the trace says it starts; `None` in closed loop.
    scheduled: Option<Instant>,
    /// Handed back once already because its connection closed after its
    /// lookup was sent.
    retried: bool,
    /// Times it was handed to a connection that was already closed, before
    /// anything was sent. Bounded by the worker's connection count.
    dead_handovers: u32,
}

impl KvJob {
    fn new(req: Request, scheduled: Option<Instant>) -> Self {
        Self {
            req,
            scheduled,
            retried: false,
            dead_handovers: 0,
        }
    }
}

/// Why a connection hands a request back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HandBack {
    /// The connection was closed when the request reached it; nothing was
    /// sent.
    Unsent,
    /// The connection closed after the lookup was sent and before any reply.
    LookupSent,
}

/// One idle connection waiting for a trace request.
struct KvSlot {
    job: Cell<Option<KvJob>>,
    /// Set when the dispatcher is done with the slot, with or without a job.
    done: Cell<bool>,
    waker: RefCell<Option<Waker>>,
}

impl KvSlot {
    fn complete(&self, job: Option<KvJob>) {
        self.job.set(job);
        self.done.set(true);
        if let Some(w) = self.waker.borrow_mut().take() {
            w.wake();
        }
    }
}

/// Worker-local queue of idle connections, served by `run_dispatcher`. The
/// same shape as the rate-limit `TokenDispatcher` in `worker.rs`: connections
/// wait untimed on a slot, and the one dispatcher task per worker holds the
/// only timer.
#[derive(Default)]
struct KvDispatcher {
    waiting: RefCell<VecDeque<Rc<KvSlot>>>,
    /// Set once the run stopped, or the dispatcher task failed to start.
    /// Sticky, so a connection that asks later does not wait for a dispatcher
    /// that is gone.
    closed: Cell<bool>,
    /// The dispatcher's waker while no connection is waiting.
    idle: RefCell<Option<Waker>>,
    /// Requests handed back by a connection found closed before the server
    /// answered any of their operations, each with `connect_failures` as it
    /// was when handed back. Served before new ones.
    returned: RefCell<VecDeque<(KvJob, u64)>>,
    /// Set while the dispatcher needs a connection to ask; `next_job` wakes
    /// it only then.
    wants_slot: Cell<bool>,
    /// Connection tasks on this worker.
    connections: Cell<usize>,
    /// Connections on this worker in the replay loop, connected.
    live: Cell<usize>,
    /// Failed connect attempts on this worker so far.
    connect_failures: Cell<u64>,
}

impl KvDispatcher {
    /// Wait for the next trace request. Once the trace is exhausted this
    /// waits until the run stops (or a request is handed back), and returns
    /// `None` when the run stops.
    async fn next_job(&self) -> Option<KvJob> {
        if self.closed.get() {
            return None;
        }
        let slot = Rc::new(KvSlot {
            job: Cell::new(None),
            done: Cell::new(false),
            waker: RefCell::new(None),
        });
        self.waiting.borrow_mut().push_back(Rc::clone(&slot));
        if self.wants_slot.get()
            && let Some(w) = self.idle.borrow_mut().take()
        {
            w.wake();
        }
        SlotWait {
            slot: Rc::clone(&slot),
        }
        .await;
        slot.job.take()
    }

    /// Give a request back to run on the next idle connection. Returns false,
    /// and keeps nothing, when the request has used up its hand-backs: the
    /// caller counts it as failed.
    ///
    /// `Unsent` does not use the one retry: nothing reached the server, and
    /// each such hand-back is one closed connection that then reconnects, so
    /// a server closing every idle connection at once costs at most one
    /// hand-back per connection. It is bounded by the worker's connection
    /// count so a server that closes each new connection at once cannot
    /// bounce a request forever. `LookupSent` uses the one retry.
    fn give_back(&self, job: KvJob, why: HandBack) -> bool {
        let job = match why {
            HandBack::Unsent => {
                if job.dead_handovers as usize >= self.connections.get().max(1) {
                    return false;
                }
                KvJob {
                    dead_handovers: job.dead_handovers + 1,
                    ..job
                }
            }
            HandBack::LookupSent => {
                if job.retried {
                    return false;
                }
                KvJob {
                    retried: true,
                    ..job
                }
            }
        };
        self.returned
            .borrow_mut()
            .push_back((job, self.connect_failures.get()));
        // Always wake: the dispatcher may be sleeping until a later request
        // is due, and this one is due already.
        if let Some(w) = self.idle.borrow_mut().take() {
            w.wake();
        }
        true
    }

    /// Release every waiting connection without a job, for good.
    fn close(&self) {
        self.closed.set(true);
        for slot in self.waiting.borrow_mut().drain(..) {
            slot.complete(None);
        }
    }
}

struct SlotWait {
    slot: Rc<KvSlot>,
}

impl Future for SlotWait {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.slot.done.get() {
            return Poll::Ready(());
        }
        *self.slot.waker.borrow_mut() = Some(cx.waker().clone());
        Poll::Pending
    }
}

thread_local! {
    /// This worker's dispatcher; `None` unless the run replays a KV trace.
    static DISPATCHER: RefCell<Option<Rc<KvDispatcher>>> = const { RefCell::new(None) };
}

/// Install this worker's dispatcher and spawn its task. Called from the
/// worker's `on_start`, before any connection task runs.
pub(crate) fn start_dispatcher(
    kv: Arc<KvReplay>,
    shared: Arc<SharedState>,
    worker_id: usize,
    connections: usize,
) {
    let dispatch = Rc::new(KvDispatcher::default());
    dispatch.connections.set(connections);
    DISPATCHER.with(|d| *d.borrow_mut() = Some(Rc::clone(&dispatch)));
    if let Err(e) = ringline::spawn(run_dispatcher(dispatch, kv, shared)) {
        // Without it no connection on this worker gets a trace request, and
        // the run would wait for requests nobody hands out. A closed
        // dispatcher sends this worker's connections idle instead, and the
        // other workers replay the whole trace.
        tracing::error!(
            worker = worker_id,
            "failed to spawn the kv replay dispatcher: {e}; this worker's connections \
             replay nothing"
        );
        DISPATCHER.with(|d| {
            if let Some(d) = d.borrow().as_ref() {
                d.close();
            }
        });
    }
}

/// How often a dispatcher checks whether the replay clock has started. At
/// `IDLE_SLEEP_MAX` (50 ms) the first request of an open-loop replay started
/// up to that late; 1 ms costs a thousand wakeups a second per worker, only
/// until the replay starts.
const START_POLL: Duration = Duration::from_millis(1);

fn dispatcher() -> Option<Rc<KvDispatcher>> {
    DISPATCHER.with(|d| d.borrow().clone())
}

/// Note a failed connect attempt on this worker (including a failed `HELLO 3`
/// or precheck PING on a new connection), or a connection closed before it
/// ran a request. A request handed back while none of the worker's
/// connections is connected is failed once a reconnect attempt has failed,
/// rather than waiting for one that may never succeed.
pub(crate) fn note_connect_failed() {
    if let Some(d) = dispatcher() {
        d.connect_failures.set(d.connect_failures.get() + 1);
    }
}

/// Marks a connection as in the replay loop for as long as it lives.
struct Live(Rc<KvDispatcher>);

impl Live {
    fn new(d: &Rc<KvDispatcher>) -> Self {
        d.live.set(d.live.get() + 1);
        Self(Rc::clone(d))
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.0.live.set(self.0.live.get() - 1);
    }
}

/// Hand trace requests to this worker's idle connections until the run
/// stops.
///
/// A request is claimed only once a connection is waiting, so a worker whose
/// connections are all busy leaves the next request to another worker. In
/// open loop a claimed request is held until it is due while the dispatcher
/// sleeps, in steps of at most `IDLE_SLEEP_MAX` so a stop is noticed; a
/// request claimed after it was due starts at once, and `kv_schedule_lag`
/// records how late. A handed-back request is due already and goes to the
/// next idle connection before the held one; a hand-back ends the sleep.
/// A held request that comes due while none of the worker's connections is
/// connected is treated as handed back.
async fn run_dispatcher(dispatch: Rc<KvDispatcher>, kv: Arc<KvReplay>, shared: Arc<SharedState>) {
    let mut exhausted = false;
    // Claimed and not yet handed to a connection.
    let mut pending: Option<KvJob> = None;
    loop {
        if shared.phase().should_stop() {
            dispatch.close();
            return;
        }
        // The runner starts the clock (`KvReplay::start_at`). Polled at
        // `START_POLL` until then, which bounds how late the first request
        // can be.
        let Some(&start) = kv.start.get() else {
            ringline::sleep(START_POLL).await;
            continue;
        };
        let has_slot = !dispatch.waiting.borrow().is_empty();

        // Handed-back requests first: they were due before anything claimed
        // since.
        if has_slot && let Some((job, _)) = dispatch.returned.borrow_mut().pop_front() {
            let slot = dispatch.waiting.borrow_mut().pop_front().unwrap();
            slot.complete(Some(job));
            continue;
        }
        // A held request that is due while no connection on this worker is
        // connected joins the handed-back ones: it runs on the first
        // connection back, or fails under the rule below.
        if dispatch.live.get() == 0
            && pending
                .as_ref()
                .is_some_and(|j| j.scheduled.is_none_or(|d| d <= Instant::now()))
        {
            let job = pending.take().unwrap();
            dispatch
                .returned
                .borrow_mut()
                .push_back((job, dispatch.connect_failures.get()));
        }
        // No connection on this worker is connected and a reconnect attempt
        // failed since the request came back: fail it rather than wait for a
        // connection that may never come.
        if dispatch.live.get() == 0 {
            let failures = dispatch.connect_failures.get();
            let mut returned = dispatch.returned.borrow_mut();
            while returned.front().is_some_and(|&(_, at)| failures > at) {
                returned.pop_front();
                kv.mark_finished();
                metrics::KV_REQUESTS_FAILED.increment();
            }
        }
        let mut reader_behind = false;
        if has_slot && pending.is_none() && !exhausted {
            match kv.claim() {
                Claim::Request(req) => {
                    let scheduled = kv.scheduled_at(start, &req);
                    pending = Some(KvJob::new(req, scheduled));
                    continue;
                }
                Claim::Exhausted => {
                    exhausted = true;
                    continue;
                }
                // A connection is idle and the next request has not been
                // read: the generator, not the server, is holding it up.
                Claim::NotReady => {
                    metrics::KV_TRACE_READ_WAITS.increment();
                    reader_behind = true;
                }
            }
        }
        let now = Instant::now();
        let due_in = pending.as_ref().map(|j| {
            j.scheduled
                .map_or(Duration::ZERO, |d| d.saturating_duration_since(now))
        });
        if has_slot && due_in == Some(Duration::ZERO) {
            let slot = dispatch.waiting.borrow_mut().pop_front().unwrap();
            slot.complete(pending.take());
            continue;
        }

        // Wait. A connection asking wakes us only when one is needed now; a
        // hand-back always does. Once the trace is exhausted, idle connections
        // stay parked, untimed, until the run stops.
        let needs_slot = !has_slot
            && (due_in == Some(Duration::ZERO)
                || !dispatch.returned.borrow().is_empty()
                || (pending.is_none() && !exhausted));
        dispatch.wants_slot.set(needs_slot);
        let nap = match due_in {
            Some(d) if !d.is_zero() => d.min(IDLE_SLEEP_MAX),
            // Polled at `START_POLL` until the reader catches up, which
            // bounds how late that makes a request.
            _ if reader_behind => START_POLL,
            _ => IDLE_SLEEP_MAX,
        };
        let mut timer = std::pin::pin!(ringline::sleep(nap));
        std::future::poll_fn(|cx| {
            if !dispatch.returned.borrow().is_empty() && !dispatch.waiting.borrow().is_empty() {
                return Poll::Ready(());
            }
            if needs_slot && !dispatch.waiting.borrow().is_empty() {
                return Poll::Ready(());
            }
            *dispatch.idle.borrow_mut() = Some(cx.waker().clone());
            timer.as_mut().poll(cx)
        })
        .await;
        dispatch.wants_slot.set(false);
        dispatch.idle.borrow_mut().take();
    }
}

// ── Connection side ──────────────────────────────────────────────────────

/// Per-connection buffers reused across trace requests.
struct Scratch {
    objects: Vec<KvObject>,
    /// Every object's key, back to back; `key_ends[i]` is where key `i` ends.
    keys: Vec<u8>,
    key_ends: Vec<usize>,
    exists: Vec<bool>,
    /// Indices into `objects` of the SETs to send.
    sets: Vec<usize>,
}

impl Scratch {
    fn key(&self, i: usize) -> &[u8] {
        let start = if i == 0 { 0 } else { self.key_ends[i - 1] };
        &self.keys[start..self.key_ends[i]]
    }
}

/// What the connection task hands the replay loop.
pub(crate) struct KvConn<'a> {
    pub kv: &'a Arc<KvReplay>,
    pub shared: &'a Arc<SharedState>,
    pub value_pool: &'a Arc<Vec<u8>>,
    pub pipeline_depth: usize,
    pub request_timeout: Duration,
}

/// Replay trace requests on one connected RESP client until the run stops or
/// the connection fails.
pub(crate) async fn drive(
    client: &mut ringline_redis::Client,
    conn: ConnCtx,
    ctx: &KvConn<'_>,
    rng: &mut Xoshiro256PlusPlus,
) -> Result<(), DisconnectReason> {
    let Some(dispatch) = dispatcher() else {
        // `on_start` installs it before spawning connections.
        unreachable!("kv replay connection without a dispatcher");
    };
    let _live = Live::new(&dispatch);
    let mut inflight = InFlight::new(ctx.request_timeout, ctx.pipeline_depth);
    let mut scratch = Scratch {
        objects: Vec::new(),
        keys: Vec::new(),
        key_ends: Vec::new(),
        exists: Vec::new(),
        sets: Vec::new(),
    };
    let mut ran_any = false;
    loop {
        if ctx.shared.phase().should_stop() {
            return Ok(());
        }
        if !client.is_alive() {
            // Closed before it ran anything: a reconnect that did not take
            // (a server closing each new connection at once) counts like a
            // failed connect for failing handed-back requests.
            if !ran_any {
                note_connect_failed();
            }
            return Err(DisconnectReason::Eof);
        }
        let Some(job) = dispatch.next_job().await else {
            // The run stopped. Otherwise this worker's dispatcher failed to
            // start; stay connected until the run stops, so the connection
            // count the runner reports is the one the run had.
            if ctx.shared.phase().should_stop() {
                return Ok(());
            }
            ringline::sleep(IDLE_SLEEP_MAX).await;
            continue;
        };
        // The connection can close while it waits for a request (a server
        // closes idle connections). Nothing was sent: hand the request back
        // and reconnect, so this connection is not offered it again.
        if !client.is_alive() {
            if !dispatch.give_back(job, HandBack::Unsent) {
                ctx.kv.mark_finished();
                metrics::KV_REQUESTS_FAILED.increment();
            }
            return Err(DisconnectReason::Eof);
        }
        ran_any = true;
        let started = Instant::now();
        let lag_ns = job
            .scheduled
            .map(|due| started.saturating_duration_since(due).as_nanos() as u64);
        let result = run_request(
            client,
            conn,
            ctx,
            rng,
            &mut inflight,
            &mut scratch,
            &job.req,
        )
        .await;
        if let Err(Failure::Closed) = result {
            if !dispatch.give_back(job, HandBack::LookupSent) {
                // Closed a second time: counted as failed rather than
                // retried again.
                ctx.kv.mark_finished();
                metrics::KV_REQUESTS_FAILED.increment();
            }
            return Err(DisconnectReason::Eof);
        }
        ctx.kv.mark_finished();
        let req = &job.req;
        match result {
            Ok(outcome) => {
                // Recorded with the rest of the request's samples, when it
                // finishes, so all of them fall in the same phase.
                if let Some(lag) = lag_ns {
                    let _ = metrics::KV_SCHEDULE_LAG.increment(lag);
                }
                record_outcome(req, &outcome);
                let recorded = ctx.shared.phase() == Phase::Running;
                ctx.kv.write_row(req, lag_ns, &outcome, recorded);
            }
            Err(Failure::Request) => metrics::KV_REQUESTS_FAILED.increment(),
            Err(Failure::Closed) => unreachable!("handled above"),
            Err(Failure::Disconnect(reason)) => {
                metrics::KV_REQUESTS_FAILED.increment();
                return Err(reason);
            }
        }
    }
}

fn record_outcome(req: &Request, o: &Outcome) {
    metrics::KV_REQUESTS.increment();
    metrics::KV_INPUT_TOKENS.add(u64::from(req.input_tokens));
    metrics::KV_CACHED_TOKENS.add(o.cached_tokens);
    metrics::KV_NEW_TOKENS.add(o.new_tokens);
    if req.chunks.is_empty() {
        metrics::KV_NO_FULL_CHUNK.increment();
        return;
    }
    metrics::KV_LOOKUPS.increment();
    let _ = metrics::KV_LOOKUP_TIME.increment(o.lookup_ns);
    if o.lookup_hit == 0 {
        metrics::KV_CHUNK0_MISSES.increment();
    }
    if o.full_hit {
        metrics::KV_FULL_HITS.increment();
    }
    if o.gets > 0 {
        metrics::KV_LOADS.increment();
        let _ = metrics::KV_LOAD_TIME.increment(o.load_ns);
        // Only a load that left cached tokens has a time per token. A nil GET
        // at the first chunk loads nothing.
        if let Some(per_1k) = o.load_ns.saturating_mul(1000).checked_div(o.cached_tokens) {
            metrics::KV_LOADS_WITH_CACHED.increment();
            let _ = metrics::KV_LOAD_TIME_PER_1K_TOKENS.increment(per_1k);
        }
    }
    if o.sets > 0 {
        metrics::KV_STORES.increment();
        let _ = metrics::KV_STORE_TIME.increment(o.store_ns);
    }
}

/// Look up, load and store one trace request.
async fn run_request(
    client: &mut ringline_redis::Client,
    conn: ConnCtx,
    ctx: &KvConn<'_>,
    rng: &mut Xoshiro256PlusPlus,
    inflight: &mut InFlight,
    s: &mut Scratch,
    req: &Request,
) -> Result<Outcome, Failure> {
    let kv = ctx.kv;
    let input = u64::from(req.input_tokens);
    let mut o = Outcome {
        new_tokens: input,
        ..Default::default()
    };
    if req.chunks.is_empty() {
        return Ok(o);
    }

    // RequestReader rejects chunk ids that do not fit an object id (2^48 or
    // more) as it reads each row.
    s.objects.clear();
    s.objects.extend(
        kv.mapper
            .objects(&req.chunks)
            .expect("RequestReader rejects chunk ids >= 2^48"),
    );
    s.keys.clear();
    s.key_ends.clear();
    for obj in &s.objects {
        kv.push_key(&mut s.keys, obj.obj_id);
        s.key_ends.push(s.keys.len());
    }
    let ranks = kv.mapper.sharding().ranks as usize;
    let n = s.objects.len();

    // 1. Lookup: every EXISTS in one pipeline.
    let mut pipeline = client.pipeline();
    let mut tx_bytes = 0u64;
    for i in 0..n {
        let key = s.key(i);
        // `*2\r\n$6\r\nEXISTS\r\n` then `$<len>\r\n<key>\r\n`
        tx_bytes += 16 + (1 + decimal_len(key.len()) + 2 + key.len() + 2) as u64;
        pipeline = pipeline.cmd(&resp_proto::Request::cmd(b"EXISTS").arg(key));
    }
    let fired_at = Instant::now();
    for _ in 0..n {
        inflight.push(fired_at);
    }
    let replies = recv_with_timeout(pipeline.execute(), inflight, conn)
        .await
        .map_err(Failure::Disconnect)?;
    inflight.forget_newest(n);
    o.lookup_ns = fired_at.elapsed().as_nanos() as u64;
    let values = match replies {
        Ok(values) => values,
        // ringline refuses a send on a closed or closing connection with
        // NotConnected. Nothing was sent.
        Err(ringline_redis::Error::Io(e)) if e.kind() == std::io::ErrorKind::NotConnected => {
            return Err(Failure::Closed);
        }
        Err(ringline_redis::Error::Io(_)) => {
            // `execute` sends nothing when the send pool cannot take the whole
            // pipeline (`Other`, or `InvalidInput` when wider than the pool).
            metrics::REQUESTS_SEND_FAILED.add(n as u64);
            return Err(Failure::Request);
        }
        Err(ringline_redis::Error::Redis(msg)) => {
            // `execute` read every reply before returning the error.
            tracing::debug!("kv replay: EXISTS error reply: {msg}");
            note_lookup(n, tx_bytes);
            metrics::REQUEST_ERRORS.increment();
            return Err(Failure::Request);
        }
        // Closed with the EXISTS sent but unanswered: the server changed
        // nothing for this request.
        Err(ringline_redis::Error::ConnectionClosed) => return Err(Failure::Closed),
        Err(e) => {
            // `execute` closed the connection: replies are positional, and a
            // failed parse leaves the stream at an unknown position.
            tracing::debug!("kv replay: EXISTS pipeline failed: {e}");
            return Err(Failure::Disconnect(DisconnectReason::RecvError));
        }
    };
    note_lookup(n, tx_bytes);
    s.exists.clear();
    for v in values {
        match v {
            resp_proto::Value::Integer(c) => s.exists.push(c > 0),
            other => {
                tracing::debug!("kv replay: unexpected EXISTS reply {other:?}");
                metrics::REQUEST_ERRORS.increment();
                return Err(Failure::Request);
            }
        }
    }
    o.lookup_hit = lookup_prefix(&s.exists, ranks);
    o.hit = o.lookup_hit;

    // 2. Load: GET every object of the hit prefix.
    let gets = o.lookup_hit * ranks;
    if gets > 0 {
        let t0 = Instant::now();
        let mut next = 0;
        let mut done = 0;
        while done < gets {
            let fired_at = Instant::now();
            let mut fired = 0u64;
            while next < gets && client.pending_count() < ctx.pipeline_depth {
                let before = client.pending_count();
                if client.fire_get(s.key(next), next as u64).is_err() {
                    note_failed_fire(before, client.pending_count(), inflight, true);
                    metrics::REQUESTS_SENT.add(fired);
                    return Err(send_failed(conn));
                }
                inflight.push(fired_at);
                next += 1;
                fired += 1;
            }
            flush(client, conn, inflight, fired)?;
            let meta = recv_with_timeout(client.recv_meta(), inflight, conn)
                .await
                .map_err(Failure::Disconnect)?
                .map_err(recv_failed)?;
            inflight.pop();
            done += 1;
            let chunk = s.objects[meta.user_data as usize].chunk_index as usize;
            let result = map_respmeta(meta);
            record_counters(&result);
            if !result.success || result.hit == Some(false) {
                if result.success {
                    o.get_nil += 1;
                    metrics::KV_GET_NIL.increment();
                }
                o.hit = o.hit.min(chunk);
            }
        }
        o.gets = gets;
        o.load_ns = t0.elapsed().as_nanos() as u64;
    }

    // 3. Store.
    s.sets.clear();
    match kv.write_rule {
        KvWriteRule::SetFromMiss => s.sets.extend(o.lookup_hit * ranks..n),
        KvWriteRule::InsertIfAbsent => {
            s.sets.extend(
                s.exists
                    .iter()
                    .enumerate()
                    .filter(|(_, e)| !**e)
                    .map(|(i, _)| i),
            );
        }
    }
    let sets = s.sets.len();
    if sets > 0 {
        let pool_len = ctx.value_pool.len();
        let t0 = Instant::now();
        let mut next = 0;
        let mut done = 0;
        while done < sets {
            let fired_at = Instant::now();
            let mut fired = 0u64;
            while next < sets && client.pending_count() < ctx.pipeline_depth {
                let i = s.sets[next];
                let len = s.objects[i].size as usize;
                let before = client.pending_count();
                let sent = if len >= ZC_VALUE_THRESHOLD {
                    let guard = make_value_guard(rng, ctx.value_pool, len, pool_len);
                    client.fire_set_with_guard(s.key(i), guard, i as u64)
                } else {
                    let value = value_slice(rng, ctx.value_pool, len, pool_len);
                    client.fire_set(s.key(i), value, i as u64)
                };
                if sent.is_err() {
                    note_failed_fire(before, client.pending_count(), inflight, true);
                    metrics::REQUESTS_SENT.add(fired);
                    return Err(send_failed(conn));
                }
                inflight.push(fired_at);
                next += 1;
                fired += 1;
            }
            flush(client, conn, inflight, fired)?;
            let meta = recv_with_timeout(client.recv_meta(), inflight, conn)
                .await
                .map_err(Failure::Disconnect)?
                .map_err(recv_failed)?;
            inflight.pop();
            done += 1;
            record_counters(&map_respmeta(meta));
        }
        o.sets = sets;
        o.store_ns = t0.elapsed().as_nanos() as u64;
    }

    o.cached_tokens = cached_tokens(o.hit as u64, kv.chunk_tokens, input);
    // `cached_tokens` never exceeds its input; the subtraction is checked so
    // a regression there fails loudly in tests instead of panicking a
    // connection task in a debug build or wrapping the counter in release.
    debug_assert!(
        o.cached_tokens <= input,
        "cached {} > input {input}",
        o.cached_tokens
    );
    o.new_tokens = input.saturating_sub(o.cached_tokens);
    o.full_hit = input > 0 && (o.hit as u64).saturating_mul(kv.chunk_tokens) >= input;
    Ok(o)
}

/// Count `n` EXISTS sent and answered. The pipeline has no result callback,
/// so the bytes are counted here: `tx_bytes` as encoded, and a 4-byte
/// `:0\r\n` or `:1\r\n` reply each.
fn note_lookup(n: usize, tx_bytes: u64) {
    let n = n as u64;
    metrics::REQUESTS_SENT.add(n);
    metrics::RESPONSES_RECEIVED.add(n);
    metrics::KV_EXISTS_COUNT.add(n);
    metrics::BYTES_TX.add(tx_bytes);
    metrics::BYTES_RX.add(4 * n);
}

fn decimal_len(mut v: usize) -> usize {
    let mut digits = 1;
    while v >= 10 {
        v /= 10;
        digits += 1;
    }
    digits
}

/// Send what the fire loop buffered and count it.
fn flush(
    client: &mut ringline_redis::Client,
    conn: ConnCtx,
    inflight: &mut InFlight,
    fired: u64,
) -> Result<(), Failure> {
    let before = client.pending_count();
    if client.flush().is_err() {
        let discarded = note_discarded(before, client.pending_count(), inflight, true);
        metrics::REQUESTS_SENT.add(fired - discarded as u64);
        return Err(send_failed(conn));
    }
    metrics::REQUESTS_SENT.add(fired);
    Ok(())
}

/// A GET or SET of a request could not be sent. Earlier ones of the same
/// request are on the wire, so the request cannot finish in order; drop the
/// connection and let it reconnect.
fn send_failed(conn: ConnCtx) -> Failure {
    conn.close();
    Failure::Disconnect(DisconnectReason::SendError)
}

fn recv_failed(e: ringline_redis::Error) -> Failure {
    match e {
        ringline_redis::Error::ConnectionClosed => Failure::Disconnect(DisconnectReason::Eof),
        _ => Failure::Disconnect(DisconnectReason::RecvError),
    }
}

// ── Results ──────────────────────────────────────────────────────────────

/// KV replay counters and histograms at the start of the recording phase,
/// subtracted from the final values as the runner does for every other
/// metric.
pub struct KvBaseline {
    counters: [u64; 14],
    lookup: Option<Histogram>,
    load: Option<Histogram>,
    store: Option<Histogram>,
    load_per_1k: Option<Histogram>,
    lag: Option<Histogram>,
}

fn kv_counters() -> [u64; 14] {
    [
        metrics::KV_REQUESTS.value(),
        metrics::KV_REQUESTS_FAILED.value(),
        metrics::KV_INPUT_TOKENS.value(),
        metrics::KV_CACHED_TOKENS.value(),
        metrics::KV_NEW_TOKENS.value(),
        metrics::KV_FULL_HITS.value(),
        metrics::KV_CHUNK0_MISSES.value(),
        metrics::KV_NO_FULL_CHUNK.value(),
        metrics::KV_GET_NIL.value(),
        metrics::KV_EXISTS_COUNT.value(),
        metrics::KV_LOOKUPS.value(),
        metrics::KV_LOADS.value(),
        metrics::KV_STORES.value(),
        metrics::KV_LOADS_WITH_CACHED.value(),
    ]
}

impl KvBaseline {
    pub fn take() -> Self {
        Self {
            counters: kv_counters(),
            lookup: metrics::KV_LOOKUP_TIME.load(),
            load: metrics::KV_LOAD_TIME.load(),
            store: metrics::KV_STORE_TIME.load(),
            load_per_1k: metrics::KV_LOAD_TIME_PER_1K_TOKENS.load(),
            lag: metrics::KV_SCHEDULE_LAG.load(),
        }
    }
}

impl KvReplay {
    /// Totals and distributions since `base`.
    pub fn results(&self, base: &KvBaseline) -> KvReplayResults {
        let now = kv_counters();
        let d = |i: usize| now[i].saturating_sub(base.counters[i]);
        let stats = |h: &metriken::AtomicCounterHistogram, b: &Option<Histogram>| -> LatencyStats {
            crate::runner::delta_latency_stats(h, b)
        };
        KvReplayResults {
            closed_loop: self.closed_loop(),
            speedup: self.speedup,
            write_rule: self.write_rule.as_str(),
            trace_requests: self.trace_requests as u64,
            requests: d(0),
            requests_failed: d(1),
            input_tokens: d(2),
            cached_tokens: d(3),
            new_tokens: d(4),
            full_hits: d(5),
            chunk0_misses: d(6),
            no_full_chunk: d(7),
            get_nil: d(8),
            exists: d(9),
            lookups: d(10),
            loads: d(11),
            stores: d(12),
            loads_with_cached_tokens: d(13),
            lookup_time: stats(&metrics::KV_LOOKUP_TIME, &base.lookup),
            load_time: stats(&metrics::KV_LOAD_TIME, &base.load),
            store_time: stats(&metrics::KV_STORE_TIME, &base.store),
            load_time_per_1k_tokens: stats(&metrics::KV_LOAD_TIME_PER_1K_TOKENS, &base.load_per_1k),
            schedule_lag: stats(&metrics::KV_SCHEDULE_LAG, &base.lag),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_prefix_is_the_minimum_over_ranks() {
        // 3 chunks x 2 ranks, chunk-major.
        assert_eq!(lookup_prefix(&[true, true, true, true, true, true], 2), 3);
        // Rank 1 is missing chunk 1: the prefix ends there although rank 0
        // has every chunk.
        assert_eq!(lookup_prefix(&[true, true, true, false, true, true], 2), 1);
        // A chunk after the gap does not extend the prefix.
        assert_eq!(lookup_prefix(&[false, true, true, true], 1), 0);
        assert_eq!(lookup_prefix(&[], 2), 0);
    }

    #[test]
    fn a_full_prompt_hit_leaves_the_last_token_to_compute() {
        // 4 chunks of 16 tokens, prompt of 64: every token is cached but one.
        assert_eq!(cached_tokens(4, 16, 64), 63);
        // A partial tail is never stored, so a hit on every full chunk of a
        // 70-token prompt is not a full-prompt hit.
        assert_eq!(cached_tokens(4, 16, 70), 64);
        assert_eq!(cached_tokens(2, 16, 70), 32);
        assert_eq!(cached_tokens(0, 16, 70), 0);
        // The trace's chunks cover more than its input_tokens.
        assert_eq!(cached_tokens(4, 16, 50), 49);
        assert_eq!(cached_tokens(0, 16, 0), 0);
        // A 0-token prompt with chunks: nothing cached, whatever was found.
        assert_eq!(cached_tokens(2, 16, 0), 0);
        assert_eq!(cached_tokens(1, 16, 1), 0);
        for (h, input) in [(4, 64), (4, 50), (2, 0), (3, 1), (0, 0), (u64::MAX, 7)] {
            assert!(
                cached_tokens(h, 16, input) <= input,
                "{h} chunks, {input} tokens"
            );
        }
    }

    fn cfg(kv_bytes_per_token: Option<u64>) -> crate::config::KvReplay {
        crate::config::KvReplay {
            trace: "unused".into(),
            speedup: 0.0,
            model_config: None,
            kv_dtype: None,
            kv_bytes_per_token,
            mla: false,
            tp: 1,
            write_rule: KvWriteRule::SetFromMiss,
            key_prefix: "kv:".into(),
            key_format: KvKeyFormat::Hex,
            requests_out: None,
        }
    }

    #[test]
    fn an_empty_trace_is_rejected() {
        let path = std::env::temp_dir().join(format!(
            "cachecannon-kv-empty-{}.parquet",
            std::process::id()
        ));
        kvtrace::RequestWriter::create(&path, 16, 1)
            .unwrap()
            .finish()
            .unwrap();
        let mut c = cfg(Some(4));
        c.trace = path.clone();
        let err = KvReplay::load(&c).err().unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(err.ends_with("no requests"), "{err}");
    }

    /// `wait_read_ahead` returns only once the reader has stopped (its error
    /// then visible) or filled the channel, or the run is interrupted.
    #[test]
    fn wait_read_ahead_waits_for_the_reader() {
        let path = std::env::temp_dir().join(format!(
            "cachecannon-kv-wait-{}.parquet",
            std::process::id()
        ));
        let mut w = kvtrace::RequestWriter::create(&path, 16, 1).unwrap();
        w.write(&Request {
            timestamp: 0,
            req_id: 1,
            input_tokens: 16,
            output_tokens: 1,
            chunks: vec![1],
            session_id: None,
        })
        .unwrap();
        w.finish().unwrap();
        let mut c = cfg(Some(4));
        c.trace = path.clone();
        let mut kv = KvReplay::load(&c).unwrap();
        std::fs::remove_file(&path).unwrap();

        // A reader that is still reading, then fails.
        let (_tx, rx) = crossbeam_channel::bounded::<Request>(2);
        kv.requests = rx;
        let state = Arc::new(ReaderState::default());
        kv.reader = Arc::clone(&state);
        let reader = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            let _ = state.error.set("bad row".into());
            state.stopped.store(true, Ordering::Release);
        });
        kv.wait_read_ahead(&AtomicBool::new(true));
        assert_eq!(kv.trace_error().as_deref(), Some("bad row"));
        reader.join().unwrap();

        // Interrupted: returns although the reader has neither stopped nor
        // filled the channel.
        kv.reader = Arc::new(ReaderState::default());
        kv.wait_read_ahead(&AtomicBool::new(false));
        assert_eq!(kv.trace_error(), None);
    }

    /// An empty channel while the reader is still running is "not ready",
    /// not the end of the trace; only a disconnected channel ends it. The
    /// replay is finished only once the reader has stopped.
    #[test]
    fn claim_waits_for_the_reader_and_ends_when_it_stops() {
        let path = std::env::temp_dir().join(format!(
            "cachecannon-kv-claim-{}.parquet",
            std::process::id()
        ));
        let mut w = kvtrace::RequestWriter::create(&path, 16, 1).unwrap();
        w.write(&Request {
            timestamp: 7,
            req_id: 1,
            input_tokens: 16,
            output_tokens: 1,
            chunks: vec![1],
            session_id: None,
        })
        .unwrap();
        w.finish().unwrap();
        let mut c = cfg(Some(4));
        c.trace = path.clone();
        let mut kv = KvReplay::load(&c).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(kv.first_ts, 7);
        // The reader thread sends it; `claim` could run before it has.
        let first = kv.requests.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(first.req_id, 1);

        // A reader that has sent nothing yet.
        let (tx, rx) = crossbeam_channel::bounded(1);
        kv.requests = rx;
        assert!(matches!(kv.claim(), Claim::NotReady));
        drop(tx);
        assert!(matches!(kv.claim(), Claim::Exhausted));

        // Every request sent has finished, but the reader may send more
        // until it stops.
        kv.reader = Arc::new(ReaderState::default());
        kv.reader.sent.store(1, Ordering::Release);
        kv.finished.store(1, Ordering::Release);
        assert!(!kv.is_finished());
        kv.reader.stopped.store(true, Ordering::Release);
        assert!(kv.is_finished());
    }

    #[test]
    fn tp_must_fit_the_geometry() {
        let mut c = cfg(Some(10));
        c.tp = 4;
        // 10 bytes per token do not split four ways.
        let err = geometry(&c).unwrap_err();
        assert!(err.contains("split evenly"), "{err}");
        c.tp = 2;
        assert_eq!(geometry(&c).unwrap().sharding().unwrap().ranks, 2);
        // MLA: one storing rank whatever the TP degree.
        c.tp = 4;
        c.mla = true;
        assert_eq!(geometry(&c).unwrap().sharding().unwrap().ranks, 1);
    }
}
