//! Object-level trace replay (`[workload.trace_replay]`).
//!
//! Replays a cachesim object-level trace (the `cachetrace` Parquet format: one
//! row per cache request with an object id, an object size and an operation)
//! against a RESP server. Rows are replayed in file order at the run's own
//! pace: `workload.rate_limit` rows per second, or closed loop. The trace's
//! timestamps are not used.
//!
//! Rows go to whichever connection has room for them, not to a connection
//! chosen by key. A worker claims rows from the reader thread in batches of
//! [`CLAIM_BATCH`] and its connections take them in order, so a hot key's
//! requests are in flight on several connections at once. Rows can then reach
//! the server out of file order: rows in flight on one worker's connections at
//! once (up to its connections x `pipeline_depth`), and rows claimed by
//! different workers, can arrive in either order. A worker whose connections stall or
//! reconnect keeps its claimed rows while other workers continue, so how far
//! apart in the file two reordered rows can be is not bounded.
//!
//! Operations follow libCacheSim's `req_op_e`, as `cachetrace::Op` decodes
//! them: reads (GET, GETS, READ, NOP) send GET, DELETE sends DEL, and every
//! write (SET, ADD, CAS, REPLACE, APPEND, PREPEND, INCR, DECR, WRITE, UPDATE)
//! sends a SET of `obj_size` bytes. A row with no op is a GET. With
//! `workload.backfill_on_miss`, a GET that misses is followed by a SET of the
//! row's `obj_size`, as libCacheSim's demand fill does. A row whose op code
//! is not one of these (15 and up), or whose SET would be larger than the
//! value pool, is skipped and counted in `trace_rows_skipped`.

use crate::config::KvKeyFormat;
use crate::limiters::WorkerLimiter;
use crate::metrics;
use crate::output::TraceReplayResults;
use crate::worker::{
    DisconnectReason, IDLE_SLEEP_MAX, IDLE_SLEEP_MIN, InFlight, Phase, SharedState,
    ZC_VALUE_THRESHOLD, dispatcher, make_value_guard, map_respmeta, note_discarded,
    note_failed_fire, record_counters, recv_with_timeout, value_slice,
};
use cachetrace::{Op, TraceEntry, TraceReader};
use parquet::file::reader::{FileReader, SerializedFileReader};
use parquet::file::statistics::Statistics;
use rand_xoshiro::Xoshiro256PlusPlus;
use ringline::ConnCtx;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Rows a worker claims from the reader thread at once. The claim is one
/// channel operation, so this sets the channel's cost per row.
pub const CLAIM_BATCH: usize = 64;

/// Sends a row may fail with ringline's `InvalidInput` before it is counted
/// failed instead of being sent again. ringline returns `InvalidInput` when
/// the send pool refuses a buffer outright (see [`never_sendable`]) and
/// `Other` when the pool cannot admit it yet; only the first counts. Without
/// the cap a row that can never be sent would be retried forever ahead of
/// every row after it.
const MAX_SEND_ATTEMPTS: u8 = 3;

/// Which unsent rows a send failure counts against [`MAX_SEND_ATTEMPTS`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Charge {
    /// A retryable failure: none.
    Nothing,
    /// A `fire_*` call that can never succeed: the row it was given.
    Failed,
    /// A flush that can never succeed: every row in the discarded batch.
    All,
}

/// Whether a send error is ringline's `InvalidInput`: the send pool refused
/// the buffer outright rather than for now. That is a buffer wider than the
/// whole pool, a batch with a zero-copy value whose copied bytes exceed one
/// pool slot (which [`must_flush_first`] keeps the replay from building), or
/// a TLS record limit.
fn never_sendable(e: &ringline_redis::Error) -> bool {
    matches!(e, ringline_redis::Error::Io(io) if io.kind() == std::io::ErrorKind::InvalidInput)
}

/// Whether the batch buffered so far must be flushed before the next request
/// is added. On io_uring ringline refuses (`InvalidInput`) a send that carries
/// a zero-copy value when the batch's copied bytes exceed one send-pool slot,
/// and ringline-redis splits batches only by guard and iovec counts. A trace
/// mixes copied and zero-copy values in one batch, so the replay splits by
/// copied bytes itself. `copied` and `has_guard` describe the batch so far;
/// `next_copied` and `next_guarded` the request to add.
fn must_flush_first(
    copied: usize,
    has_guard: bool,
    next_copied: usize,
    next_guarded: bool,
    slot: usize,
) -> bool {
    copied > 0 && (has_guard || next_guarded) && copied + next_copied > slot
}

/// Batches the reader thread may hold ready ahead of the replay, so at most
/// `READ_AHEAD_BATCHES * CLAIM_BATCH` (16,384) rows, whatever the file's
/// length.
const READ_AHEAD_BATCHES: usize = 256;

/// Per-connection cap on GET misses waiting for their backfill SET, as for
/// the synthetic workload. A miss beyond it is not backfilled.
const BACKFILL_QUEUE_CAP: usize = 1024;

/// How often an idle connection polls a reader that is behind.
const READER_POLL: Duration = Duration::from_millis(1);

/// The command a trace row sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowOp {
    Get,
    Set,
    Del,
}

impl RowOp {
    /// The command for a trace row's op, or `None` for an op code this replay
    /// does not send (15 and up). A row with no op is a GET.
    pub fn from_trace(op: Option<u8>) -> Option<Self> {
        let Some(code) = op else {
            return Some(Self::Get);
        };
        let op = Op::from_u8(code);
        if op.is_delete() {
            Some(Self::Del)
        } else if op.is_write() {
            Some(Self::Set)
        } else if op.is_read() {
            Some(Self::Get)
        } else {
            None
        }
    }
}

/// One trace row, as the replay sends it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row {
    pub obj_id: u64,
    pub size: u32,
    pub op: RowOp,
    /// Sends of this row that the send pool refused outright (ringline's
    /// `InvalidInput`); the row is counted failed after three.
    pub failed_sends: u8,
}

impl Row {
    /// The row for `entry`, or `None` when it is skipped: an op this replay
    /// does not send, or a SET larger than `max_value`.
    pub fn from_entry(entry: &TraceEntry, max_value: usize) -> Option<Self> {
        let op = RowOp::from_trace(entry.op)?;
        if op == RowOp::Set && entry.obj_size as usize > max_value {
            return None;
        }
        Some(Self {
            obj_id: entry.obj_id,
            size: entry.obj_size,
            op,
            failed_sends: 0,
        })
    }
}

/// A trace being replayed, shared by every worker.
pub struct TraceReplay {
    /// Rows in file order, in batches of at most [`CLAIM_BATCH`], from the
    /// reader thread. Disconnected once the reader has stopped and every
    /// batch it sent has been taken.
    batches: crossbeam_channel::Receiver<Vec<Row>>,
    reader: Arc<ReaderState>,
    path: PathBuf,
    /// Rows in the file, from its footer, skipped ones included.
    trace_rows: usize,
    /// The largest `obj_size` in the file, from its footer statistics.
    max_obj_size: u32,
    key_prefix: Vec<u8>,
    key_format: KvKeyFormat,
    /// Rows answered or abandoned. The replay is over when this reaches the
    /// rows the reader sent.
    done: AtomicUsize,
    /// Set by the runner when rows may be sent; see [`TraceReplay::start`].
    started: AtomicBool,
    /// Claimed rows given up by a worker whose connection failed, for any
    /// worker to send before claiming more: each worker's rows in file order,
    /// the latest failure's first.
    returned: Mutex<VecDeque<Row>>,
}

impl TraceReplay {
    /// Check the file's columns, read the largest object size from its
    /// footer, and start the thread that reads the rows.
    pub fn load(cfg: &crate::config::TraceReplay) -> Result<Self, String> {
        let path = cfg.trace.clone();
        let (trace_rows, max_obj_size) = footer(&path)?;
        if trace_rows == 0 {
            return Err(format!("trace_replay trace {}: no rows", path.display()));
        }
        let reader = TraceReader::open(&path)
            .map_err(|e| format!("trace_replay trace {}: {e}", path.display()))?;
        tracing::info!(
            rows = trace_rows,
            max_obj_size,
            "trace replay: trace opened"
        );
        let (batches, state) = spawn_reader(path.clone(), reader)?;
        Ok(Self {
            batches,
            reader: state,
            path,
            trace_rows,
            max_obj_size,
            key_prefix: cfg.key_prefix.as_bytes().to_vec(),
            key_format: cfg.key_format,
            done: AtomicUsize::new(0),
            started: AtomicBool::new(false),
            returned: Mutex::new(VecDeque::new()),
        })
    }

    /// Rows in the trace file, skipped ones included.
    pub fn len(&self) -> usize {
        self.trace_rows
    }

    pub fn is_empty(&self) -> bool {
        self.trace_rows == 0
    }

    /// The largest object in the trace, in bytes, capped at the value pool:
    /// the SET value length the send pool is sized for.
    pub fn max_value_len(&self) -> usize {
        (self.max_obj_size as usize).min(crate::runner::VALUE_POOL_SIZE)
    }

    /// Length of the longest key.
    pub fn max_key_len(&self) -> usize {
        let digits = match self.key_format {
            KvKeyFormat::Hex => 16,
            KvKeyFormat::Decimal => 20,
        };
        self.key_prefix.len() + digits
    }

    /// Block until the reader thread has filled its read-ahead or stopped,
    /// so a read error in the first rows is known before any row is sent.
    /// Returns early once `running` is false.
    pub fn wait_read_ahead(&self, running: &AtomicBool) {
        while !self.reader.stopped.load(Ordering::Acquire)
            && !self.batches.is_full()
            && running.load(Ordering::SeqCst)
        {
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// The error that stopped the reader thread before the end of the trace,
    /// if one did. The rows after it were not replayed.
    pub fn trace_error(&self) -> Option<String> {
        self.reader.error.get().cloned()
    }

    /// True once the reader thread has stopped and every row it sent has been
    /// answered or abandoned.
    pub fn is_finished(&self) -> bool {
        self.reader.stopped.load(Ordering::Acquire)
            && self.done.load(Ordering::Acquire) >= self.reader.sent.load(Ordering::Acquire)
    }

    /// Let connections send rows. The runner calls it as warmup starts when
    /// there is a warmup, so the first rows warm the cache unrecorded, and
    /// otherwise as recording starts: started with a zero-length warmup, a
    /// short trace could finish before the runner, which checks every
    /// 100 ms, begins recording. Later calls do nothing.
    pub fn start(&self) {
        self.started.store(true, Ordering::Release);
    }

    fn started(&self) -> bool {
        self.started.load(Ordering::Acquire)
    }

    fn mark_done(&self, n: usize) {
        if n > 0 {
            self.done.fetch_add(n, Ordering::Release);
        }
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

    /// The results of the recording phase, from the counters now and at its
    /// start.
    pub fn results(&self, baseline: &TraceBaseline) -> TraceReplayResults {
        let now = TraceBaseline::take();
        let d = |i: usize| now.counters[i].saturating_sub(baseline.counters[i]);
        TraceReplayResults {
            trace: self.path.display().to_string(),
            trace_rows: self.trace_rows as u64,
            rows: d(0),
            rows_failed: d(1),
            rows_skipped: metrics::TRACE_ROWS_SKIPPED.value(),
            gets: d(2),
            sets: d(3),
            dels: d(4),
            read_waits: d(5),
            // A read error also stops the reader, but the rows after it were
            // never sent.
            finished: self.is_finished() && self.trace_error().is_none(),
            all_skipped: self.reader.sent.load(Ordering::Acquire) == 0
                && self.reader.stopped.load(Ordering::Acquire)
                && self.trace_error().is_none(),
        }
    }
}

/// The row count and largest `obj_size` from the file's footer, after
/// checking that the columns the replay reads have cachetrace's types:
/// `obj_id` UInt64, `obj_size` UInt32 and, when present, `op` UInt8.
/// cachetrace's reader treats a column of another type as missing and fills
/// in a default, so a file with an Int64 `obj_id` (pyarrow's default integer)
/// would otherwise replay every row as a GET of key 0.
fn footer(path: &Path) -> Result<(usize, u32), String> {
    use arrow::datatypes::DataType;
    let err = |msg: String| format!("trace_replay trace {}: {msg}", path.display());
    let file = File::open(path).map_err(|e| err(e.to_string()))?;
    let arrow_schema = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(
        file.try_clone().map_err(|e| err(e.to_string()))?,
    )
    .map_err(|e| err(e.to_string()))?
    .schema()
    .clone();
    for (name, want, required) in [
        ("obj_id", DataType::UInt64, true),
        ("obj_size", DataType::UInt32, true),
        ("op", DataType::UInt8, false),
    ] {
        match arrow_schema.field_with_name(name) {
            Ok(f) if *f.data_type() == want => {}
            Ok(f) => {
                return Err(err(format!(
                    "column {name} is {}; the replay reads {want} (cachetrace's schema)",
                    f.data_type()
                )));
            }
            Err(_) if required => return Err(err(format!("no {name} column"))),
            Err(_) => {}
        }
    }
    let reader = SerializedFileReader::new(file).map_err(|e| err(e.to_string()))?;
    let meta = reader.metadata();
    let schema = meta.file_metadata().schema_descr();
    let Some(size_col) = (0..schema.num_columns()).find(|&i| schema.column(i).name() == "obj_size")
    else {
        return Err(err("no obj_size column".into()));
    };
    let rows = usize::try_from(meta.file_metadata().num_rows())
        .map_err(|_| err("negative row count in the footer".into()))?;
    let mut max = 0u32;
    for rg in meta.row_groups() {
        match rg.column(size_col).statistics() {
            // `obj_size` is UInt32, stored as INT32; the statistics hold the
            // bits.
            Some(Statistics::Int32(s)) if s.max_opt().is_some() => {
                max = max.max(*s.max_opt().unwrap() as u32);
            }
            _ => {
                return Err(err(
                    "no obj_size statistics in the footer, which the replay needs to size its \
                     send buffers; rewrite the file with statistics enabled"
                        .into(),
                ));
            }
        }
    }
    Ok((rows, max))
}

/// What the reader thread has done, for deciding when the replay is over.
#[derive(Default)]
struct ReaderState {
    /// Rows sent into the channel, counted before each send.
    sent: AtomicUsize,
    /// Set once the thread has sent its last batch.
    stopped: AtomicBool,
    /// The read error that stopped it before the end of the trace.
    error: OnceLock<String>,
}

/// Start the thread that reads `reader` into a channel of batches. It stops
/// at the end of the trace, at a read error (recorded in
/// [`ReaderState::error`] and logged), or when the receiver is dropped.
fn spawn_reader(
    path: PathBuf,
    reader: TraceReader,
) -> Result<(crossbeam_channel::Receiver<Vec<Row>>, Arc<ReaderState>), String> {
    let (tx, rx) = crossbeam_channel::bounded(READ_AHEAD_BATCHES);
    let state = Arc::new(ReaderState::default());
    let thread_state = Arc::clone(&state);
    std::thread::Builder::new()
        .name("trace-reader".into())
        .spawn(move || {
            let state = thread_state;
            // Sets `stopped` on every exit, a panic included, before `tx` is
            // dropped, so a connection that sees the channel disconnected can
            // rely on it; a panic is recorded as the error.
            struct Stopped<'a>(&'a ReaderState);
            impl Drop for Stopped<'_> {
                fn drop(&mut self) {
                    if std::thread::panicking() {
                        let _ = self
                            .0
                            .error
                            .set("trace_replay: the trace reader thread panicked".into());
                    }
                    self.0.stopped.store(true, Ordering::Release);
                }
            }
            let _stopped = Stopped(&state);
            let max_value = crate::runner::VALUE_POOL_SIZE;
            let mut batch = Vec::with_capacity(CLAIM_BATCH);
            let send = |batch: Vec<Row>| {
                state.sent.fetch_add(batch.len(), Ordering::AcqRel);
                tx.send(batch).is_ok()
            };
            for entry in reader {
                match entry {
                    Ok(entry) => match Row::from_entry(&entry, max_value) {
                        Some(row) => {
                            batch.push(row);
                            if batch.len() == CLAIM_BATCH
                                && !send(std::mem::replace(
                                    &mut batch,
                                    Vec::with_capacity(CLAIM_BATCH),
                                ))
                            {
                                // The replay was dropped: the run is over.
                                return;
                            }
                        }
                        None => metrics::TRACE_ROWS_SKIPPED.increment(),
                    },
                    Err(e) => {
                        let msg = format!("trace_replay trace {}: {e}", path.display());
                        tracing::error!("{msg}; the rows after it are not replayed");
                        let _ = state.error.set(msg);
                        break;
                    }
                }
            }
            if !batch.is_empty() {
                send(batch);
            }
        })
        .map_err(|e| format!("trace_replay: cannot start the trace reader thread: {e}"))?;
    Ok((rx, state))
}

// ── Worker-local rows ────────────────────────────────────────────────────

thread_local! {
    /// Rows this worker claimed and has not sent yet, in file order. Every
    /// connection on the worker takes from the front.
    static CLAIMED: RefCell<VecDeque<Row>> = const { RefCell::new(VecDeque::new()) };
}

/// The result of [`next_row`].
enum Next {
    Row(Row),
    /// The reader thread has not read the next batch yet.
    NotReady,
    /// Every row has been handed out.
    Exhausted,
}

fn next_row(trace: &TraceReplay) -> Next {
    CLAIMED.with(|c| {
        let mut claimed = c.borrow_mut();
        if let Some(row) = claimed.pop_front() {
            return Next::Row(row);
        }
        {
            let mut returned = trace.returned.lock().unwrap();
            if !returned.is_empty() {
                let n = returned.len().min(CLAIM_BATCH);
                claimed.extend(returned.drain(..n));
                return Next::Row(claimed.pop_front().unwrap());
            }
        }
        match trace.batches.try_recv() {
            Ok(batch) => {
                claimed.extend(batch);
                claimed.pop_front().map_or(Next::NotReady, Next::Row)
            }
            Err(crossbeam_channel::TryRecvError::Empty) => Next::NotReady,
            Err(crossbeam_channel::TryRecvError::Disconnected) => Next::Exhausted,
        }
    })
}

/// Put rows that were not sent back at the front of this worker's claimed
/// rows, so the next connection to ask sends them first. `rows` is in file
/// order.
fn unclaim(rows: impl DoubleEndedIterator<Item = Row>) {
    CLAIMED.with(|c| {
        let mut claimed = c.borrow_mut();
        for row in rows.rev() {
            claimed.push_front(row);
        }
    });
}

/// Hand this worker's claimed rows to the shared returned queue, ahead of
/// rows returned before, so any worker can send them. Called when a
/// connection fails: the worker may have no other connection to send them.
fn return_claimed(trace: &TraceReplay) {
    CLAIMED.with(|c| {
        let mut claimed = c.borrow_mut();
        if claimed.is_empty() {
            return;
        }
        let mut returned = trace.returned.lock().unwrap();
        for row in claimed.drain(..).rev() {
            returned.push_front(row);
        }
    });
}

// ── Connection side ──────────────────────────────────────────────────────

/// What a request in flight on a connection was, oldest first, parallel to
/// the connection's `InFlight` (RESP replies are in request order).
#[derive(Debug, Clone, Copy)]
enum Sent {
    /// A trace row.
    Row(Row),
    /// A backfill SET after a GET miss.
    Backfill { obj_id: u64, size: u32 },
}

/// What the connection task hands the replay loop.
pub(crate) struct TraceConn<'a> {
    pub trace: &'a Arc<TraceReplay>,
    pub shared: &'a Arc<SharedState>,
    pub ratelimiter: Option<&'a Arc<WorkerLimiter>>,
    pub value_pool: &'a Arc<Vec<u8>>,
    pub pipeline_depth: usize,
    pub batch_size: usize,
    pub request_timeout: Duration,
    pub backfill_on_miss: bool,
}

/// Replay trace rows on one connected RESP client until the run stops or the
/// connection fails. Rows in flight when the connection fails are counted as
/// failed, not retried: the server may have applied them. The worker's
/// claimed rows that were not sent go to the shared returned queue.
pub(crate) async fn drive(
    client: &mut ringline_redis::Client,
    conn: ConnCtx,
    ctx: &TraceConn<'_>,
    rng: &mut Xoshiro256PlusPlus,
) -> Result<(), DisconnectReason> {
    let mut sent: VecDeque<Sent> = VecDeque::with_capacity(ctx.pipeline_depth);
    let result = run(client, conn, ctx, rng, &mut sent).await;
    // At a stop the rows in flight are left out of every count, whether the
    // connection sees the stop or is closed by shutdown first.
    if !ctx.shared.phase().should_stop() {
        let failed = sent.iter().filter(|s| matches!(s, Sent::Row(_))).count();
        metrics::TRACE_ROWS_FAILED.add(failed as u64);
        ctx.trace.mark_done(failed);
    }
    if result.is_err() {
        return_claimed(ctx.trace);
    }
    result
}

async fn run(
    client: &mut ringline_redis::Client,
    conn: ConnCtx,
    ctx: &TraceConn<'_>,
    rng: &mut Xoshiro256PlusPlus,
    sent: &mut VecDeque<Sent>,
) -> Result<(), DisconnectReason> {
    let trace = ctx.trace;
    let pool_len = ctx.value_pool.len();
    let token_dispatch = dispatcher();
    // Tokens the dispatcher already charged to this connection, spent before
    // polling the limiter again; see `drive_resp_workload`.
    let mut carried_tokens: u64 = 0;
    let mut inflight = InFlight::new(ctx.request_timeout, ctx.pipeline_depth);
    let mut backfill: VecDeque<(u64, u32)> = VecDeque::new();
    let mut key = Vec::with_capacity(trace.max_key_len());
    // The batch buffered since the last flush, for `must_flush_first`. Reset
    // at every flush; every iteration ends with one. A flush the client makes
    // on its own only lowers what it holds, so the estimate stays an upper
    // bound.
    let slot = crate::runner::RINGLINE_SEND_SLOT_SIZE as usize;
    let mut batch_copied = 0usize;
    let mut batch_guard = false;

    loop {
        let phase = ctx.shared.phase();
        if phase.should_stop() {
            return Ok(());
        }
        if client.pending_count() == 0 && !client.is_alive() {
            return Err(DisconnectReason::Eof);
        }
        let fired_at = Instant::now();
        let mut fired_count: u64 = 0;
        let mut starved = false;
        let mut reader_behind = false;
        let mut exhausted = false;
        let waiting_start = !trace.started();

        let want_fire = ctx.pipeline_depth.saturating_sub(client.pending_count()) >= ctx.batch_size;
        if (phase == Phase::Warmup || phase == Phase::Running) && want_fire && !waiting_start {
            let mut token_budget = match ctx.ratelimiter {
                Some(rl) => {
                    if carried_tokens > 0 {
                        let n = carried_tokens as usize;
                        carried_tokens = 0;
                        n
                    } else {
                        let n = (ctx.batch_size as u64).min(rl.max_tokens());
                        if n > 0 && rl.try_wait_n(n).is_ok() {
                            n as usize
                        } else {
                            starved = true;
                            0
                        }
                    }
                }
                None => ctx.batch_size,
            };
            let slip_ns = ctx.ratelimiter.map(|rl| rl.slip_ns());
            if let Some(slip) = slip_ns {
                metrics::set_worker_slip_ns(slip);
            }

            while client.pending_count() < ctx.pipeline_depth && token_budget > 0 {
                let next = match backfill.pop_front() {
                    Some((obj_id, size)) => Sent::Backfill { obj_id, size },
                    None => match next_row(trace) {
                        Next::Row(row) => Sent::Row(row),
                        Next::NotReady => {
                            metrics::TRACE_READ_WAITS.increment();
                            reader_behind = true;
                            break;
                        }
                        Next::Exhausted => {
                            exhausted = true;
                            break;
                        }
                    },
                };
                let (op, obj_id, size) = match next {
                    Sent::Row(row) => (row.op, row.obj_id, row.size as usize),
                    Sent::Backfill { obj_id, size } => (RowOp::Set, obj_id, size as usize),
                };
                key.clear();
                trace.push_key(&mut key, obj_id);

                let guarded = op == RowOp::Set && size >= ZC_VALUE_THRESHOLD;
                let copied = key.len()
                    + crate::runner::REQUEST_FRAMING
                    + if op == RowOp::Set && !guarded {
                        size
                    } else {
                        0
                    };
                if must_flush_first(batch_copied, batch_guard, copied, guarded, slot) {
                    let before = client.pending_count();
                    let flushed = client.flush();
                    batch_copied = 0;
                    batch_guard = false;
                    if let Err(e) = flushed {
                        let discarded =
                            note_discarded(before, client.pending_count(), &mut inflight, true);
                        fired_count -= discarded as u64;
                        let charge = if never_sendable(&e) {
                            Charge::All
                        } else {
                            Charge::Nothing
                        };
                        note_given_up(trace, requeue_with_next(sent, discarded, next, charge));
                        break;
                    }
                }
                token_budget -= 1;

                let before = client.pending_count();
                let res = match op {
                    RowOp::Get => client.fire_get(&key, 0),
                    RowOp::Del => client.fire_del(&key, 0),
                    RowOp::Set if size >= ZC_VALUE_THRESHOLD => {
                        let guard = make_value_guard(rng, ctx.value_pool, size, pool_len);
                        client.fire_set_with_guard(&key, guard, 0)
                    }
                    RowOp::Set => {
                        let value = value_slice(rng, ctx.value_pool, size, pool_len);
                        client.fire_set(&key, value, 0)
                    }
                };
                if let Err(e) = res {
                    // The flush below resets the batch state.
                    let discarded =
                        note_failed_fire(before, client.pending_count(), &mut inflight, true);
                    fired_count -= discarded as u64;
                    let charge = if never_sendable(&e) {
                        Charge::Failed
                    } else {
                        Charge::Nothing
                    };
                    note_given_up(trace, requeue(sent, discarded, Some(next), charge));
                    break;
                }
                fired_count += 1;
                if let Some(slip) = slip_ns {
                    let _ = metrics::SCHEDULE_SLIP.increment(slip);
                }
                inflight.push(fired_at);
                sent.push_back(next);
                batch_copied += copied;
                batch_guard |= guarded;
            }
            // Tokens funded for rows that were not there to send.
            if (reader_behind || exhausted) && token_budget > 0 {
                carried_tokens += token_budget as u64;
            }
        }

        // Send what this iteration buffered.
        let before = client.pending_count();
        batch_copied = 0;
        batch_guard = false;
        if let Err(e) = client.flush() {
            let discarded = note_discarded(before, client.pending_count(), &mut inflight, true);
            fired_count -= discarded as u64;
            let charge = if never_sendable(&e) {
                Charge::All
            } else {
                Charge::Nothing
            };
            note_given_up(trace, requeue(sent, discarded, None, charge));
        }
        metrics::REQUESTS_SENT.add(fired_count);
        if let Some(rl) = ctx.ratelimiter {
            metrics::set_worker_slip_ns(rl.slip_ns());
        }

        if client.pending_count() == 0 {
            match token_dispatch {
                Some(ref d) if starved => {
                    carried_tokens += d.acquire(ctx.batch_size as u64).await;
                }
                _ => {
                    let wait = if exhausted {
                        // Nothing left to send; wait for the run to stop.
                        IDLE_SLEEP_MAX
                    } else if reader_behind || waiting_start {
                        READER_POLL
                    } else {
                        IDLE_SLEEP_MIN
                    };
                    ringline::sleep(wait).await;
                }
            }
            continue;
        }

        let result = match recv_with_timeout(client.recv_meta(), &mut inflight, conn).await {
            Ok(Ok(meta)) => map_respmeta(meta),
            Ok(Err(ringline_redis::Error::ConnectionClosed)) => return Err(DisconnectReason::Eof),
            Ok(Err(_)) => return Err(DisconnectReason::RecvError),
            Err(reason) => return Err(reason),
        };
        inflight.pop();
        let done = sent
            .pop_front()
            .expect("a reply with nothing recorded in flight");
        record_counters(&result);
        match done {
            Sent::Row(row) => {
                trace.mark_done(1);
                metrics::TRACE_ROWS.increment();
                match row.op {
                    RowOp::Get => metrics::TRACE_GETS.increment(),
                    RowOp::Set => metrics::TRACE_SETS.increment(),
                    RowOp::Del => metrics::TRACE_DELS.increment(),
                }
                if ctx.backfill_on_miss
                    && row.op == RowOp::Get
                    && result.success
                    && result.hit == Some(false)
                    && backfill.len() < BACKFILL_QUEUE_CAP
                    && row.size as usize <= crate::runner::VALUE_POOL_SIZE
                {
                    backfill.push_back((row.obj_id, row.size));
                }
            }
            Sent::Backfill { .. } => {
                if result.success {
                    metrics::BACKFILL_SET_COUNT.increment();
                    let _ = metrics::BACKFILL_SET_LATENCY.increment(result.latency_ns);
                }
            }
        }
    }
}

/// After a failed send: the `discarded` newest requests in `sent` never left,
/// and neither did `failed`, the one being sent when it failed. Trace rows go
/// back to the front of this worker's claimed rows, in order, to be sent
/// again. `charge` says which of them the failure counts against
/// [`MAX_SEND_ATTEMPTS`]; a row that reaches it is not requeued, and the count
/// of those is returned for the caller to record as failed. Backfill SETs are
/// dropped, as a miss beyond the backfill queue's cap is.
fn requeue(
    sent: &mut VecDeque<Sent>,
    discarded: usize,
    failed: Option<Sent>,
    charge: Charge,
) -> usize {
    let keep = sent.len().saturating_sub(discarded);
    let mut rows = Vec::new();
    let mut given_up = 0;
    let n_discarded = sent.len() - keep;
    for (i, s) in sent.drain(keep..).chain(failed).enumerate() {
        if let Sent::Row(mut row) = s {
            let charged = match charge {
                Charge::Nothing => false,
                Charge::Failed => i >= n_discarded,
                Charge::All => true,
            };
            if charged {
                row.failed_sends += 1;
            }
            if row.failed_sends >= MAX_SEND_ATTEMPTS {
                given_up += 1;
            } else {
                rows.push(row);
            }
        }
    }
    unclaim(rows.into_iter());
    given_up
}

/// [`requeue`] after a failed flush made before `next` was fired: `next` was
/// not sent and is not at fault, so it goes back uncharged, after the rows the
/// flush discarded.
fn requeue_with_next(
    sent: &mut VecDeque<Sent>,
    discarded: usize,
    next: Sent,
    charge: Charge,
) -> usize {
    if let Sent::Row(row) = next {
        unclaim(std::iter::once(row));
    }
    requeue(sent, discarded, None, charge)
}

/// Record rows that [`requeue`] gave up on.
fn note_given_up(trace: &TraceReplay, n: usize) {
    metrics::TRACE_ROWS_FAILED.add(n as u64);
    trace.mark_done(n);
}

// ── Results ──────────────────────────────────────────────────────────────

/// Trace replay counters at the start of the recording phase, subtracted
/// from the final values as the runner does for every other metric.
pub struct TraceBaseline {
    counters: [u64; 6],
}

impl TraceBaseline {
    pub fn take() -> Self {
        Self {
            counters: [
                metrics::TRACE_ROWS.value(),
                metrics::TRACE_ROWS_FAILED.value(),
                metrics::TRACE_GETS.value(),
                metrics::TRACE_SETS.value(),
                metrics::TRACE_DELS.value(),
                metrics::TRACE_READ_WAITS.value(),
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(op: Option<u8>, size: u32) -> TraceEntry {
        TraceEntry {
            timestamp: 0,
            obj_id: 7,
            obj_size: size,
            next_access_vtime: -1,
            op,
            ttl: None,
        }
    }

    #[test]
    fn ops_map_to_get_set_and_del() {
        assert_eq!(RowOp::from_trace(None), Some(RowOp::Get));
        for code in [0, 1, 2, 12] {
            assert_eq!(RowOp::from_trace(Some(code)), Some(RowOp::Get), "op {code}");
        }
        for code in [3, 4, 5, 6, 7, 8, 10, 11, 13, 14] {
            assert_eq!(RowOp::from_trace(Some(code)), Some(RowOp::Set), "op {code}");
        }
        assert_eq!(RowOp::from_trace(Some(9)), Some(RowOp::Del));
        for code in [15, 16, 255] {
            assert_eq!(RowOp::from_trace(Some(code)), None, "op {code}");
        }
    }

    #[test]
    fn rows_skip_unknown_ops_and_oversized_sets() {
        assert_eq!(
            Row::from_entry(&entry(Some(3), 100), 100),
            Some(Row {
                obj_id: 7,
                size: 100,
                op: RowOp::Set,
                failed_sends: 0,
            })
        );
        assert_eq!(Row::from_entry(&entry(Some(3), 101), 100), None);
        // Only a SET carries its size: a GET or DEL of a large object is sent.
        assert!(Row::from_entry(&entry(Some(1), 101), 100).is_some());
        assert!(Row::from_entry(&entry(Some(9), 101), 100).is_some());
        assert_eq!(Row::from_entry(&entry(Some(15), 1), 100), None);
    }

    fn row(id: u64) -> Row {
        Row {
            obj_id: id,
            size: 1,
            op: RowOp::Get,
            failed_sends: 0,
        }
    }

    /// Write a three-row Parquet file with the given columns.
    fn parquet(
        name: &str,
        cols: &[(&str, arrow::datatypes::DataType)],
        statistics: bool,
    ) -> PathBuf {
        use arrow::array::{
            ArrayRef, Int8Array, Int32Array, Int64Array, UInt8Array, UInt32Array, UInt64Array,
        };
        use arrow::datatypes::{DataType, Field, Schema};
        use arrow::record_batch::RecordBatch;
        use parquet::arrow::ArrowWriter;
        use parquet::file::properties::{EnabledStatistics, WriterProperties};
        let path = std::env::temp_dir().join(format!(
            "cachecannon-tracereplay-{name}-{}.parquet",
            std::process::id()
        ));
        let mut fields = Vec::new();
        let mut arrays: Vec<ArrayRef> = Vec::new();
        for (col, ty) in cols {
            fields.push(Field::new(*col, ty.clone(), false));
            arrays.push(match ty {
                DataType::UInt64 => Arc::new(UInt64Array::from(vec![1, 2, 3])),
                DataType::UInt32 => Arc::new(UInt32Array::from(vec![10, 3_000_000_000, 7])),
                DataType::UInt8 => Arc::new(UInt8Array::from(vec![1, 3, 9])),
                DataType::Int64 => Arc::new(Int64Array::from(vec![1, 2, 3])),
                DataType::Int32 => Arc::new(Int32Array::from(vec![10, 20, 30])),
                DataType::Int8 => Arc::new(Int8Array::from(vec![1, 3, 9])),
                other => panic!("no test column of type {other}"),
            });
        }
        let schema = Arc::new(Schema::new(fields));
        let props = WriterProperties::builder()
            .set_statistics_enabled(if statistics {
                EnabledStatistics::Chunk
            } else {
                EnabledStatistics::None
            })
            .build();
        let mut w = ArrowWriter::try_new(File::create(&path).unwrap(), schema.clone(), Some(props))
            .unwrap();
        w.write(&RecordBatch::try_new(schema, arrays).unwrap())
            .unwrap();
        w.close().unwrap();
        path
    }

    #[test]
    fn footer_reads_rows_and_the_largest_size() {
        use arrow::datatypes::DataType::{UInt8, UInt32, UInt64};
        let path = parquet(
            "ok",
            &[("obj_id", UInt64), ("obj_size", UInt32), ("op", UInt8)],
            true,
        );
        let got = footer(&path);
        std::fs::remove_file(&path).unwrap();
        // 3,000,000,000 is above i32::MAX: the statistics are read as u32.
        assert_eq!(got, Ok((3, 3_000_000_000)));
    }

    #[test]
    fn footer_refuses_missing_and_mistyped_columns_and_missing_statistics() {
        use arrow::datatypes::DataType::{Int8, Int32, Int64, UInt32, UInt64};
        type Columns = Vec<(&'static str, arrow::datatypes::DataType)>;
        let cases: [(&str, Columns, bool, &str); 6] = [
            (
                "no-id",
                vec![("obj_size", UInt32)],
                true,
                "no obj_id column",
            ),
            (
                "no-size",
                vec![("obj_id", UInt64)],
                true,
                "no obj_size column",
            ),
            (
                "no-stats",
                vec![("obj_id", UInt64), ("obj_size", UInt32)],
                false,
                "no obj_size statistics",
            ),
            // pyarrow's default integer types.
            (
                "signed-id",
                vec![("obj_id", Int64), ("obj_size", UInt32)],
                true,
                "column obj_id is Int64",
            ),
            (
                "signed-size",
                vec![("obj_id", UInt64), ("obj_size", Int32)],
                true,
                "column obj_size is Int32",
            ),
            (
                "signed-op",
                vec![("obj_id", UInt64), ("obj_size", UInt32), ("op", Int8)],
                true,
                "column op is Int8",
            ),
        ];
        for (name, cols, stats, want) in cases {
            let path = parquet(name, &cols, stats);
            let got = footer(&path);
            std::fs::remove_file(&path).unwrap();
            let err = got.unwrap_err();
            assert!(err.contains(want), "{name}: {err}");
        }
    }

    /// Rows a worker claimed and then gave up, after its connection failed,
    /// are the next rows any worker takes.
    #[test]
    fn returned_rows_go_to_the_next_worker_that_asks() {
        let path = std::env::temp_dir().join(format!(
            "cachecannon-tracereplay-returned-{}.parquet",
            std::process::id()
        ));
        let mut w = cachetrace::TraceWriter::create(&path, 16).unwrap();
        for id in 0..200u64 {
            w.write(&TraceEntry {
                timestamp: id as i64,
                obj_id: id,
                obj_size: 8,
                next_access_vtime: -1,
                op: Some(1),
                ttl: None,
            })
            .unwrap();
        }
        w.finish().unwrap();
        let trace = Arc::new(
            TraceReplay::load(&crate::config::TraceReplay {
                trace: path.clone(),
                key_prefix: String::new(),
                key_format: KvKeyFormat::Hex,
            })
            .unwrap(),
        );
        std::fs::remove_file(&path).unwrap();
        trace.wait_read_ahead(&AtomicBool::new(true));
        // Worker A claims a batch, sends row 0, then its connection fails.
        let a = Arc::clone(&trace);
        std::thread::spawn(move || {
            let Next::Row(first) = next_row(&a) else {
                panic!("no row");
            };
            assert_eq!(first.obj_id, 0);
            return_claimed(&a);
        })
        .join()
        .unwrap();
        // Worker B gets rows 1.. of A's batch before claiming its own.
        let b = Arc::clone(&trace);
        let ids: Vec<u64> = std::thread::spawn(move || {
            (0..CLAIM_BATCH)
                .map(|_| match next_row(&b) {
                    Next::Row(r) => r.obj_id,
                    _ => panic!("no row"),
                })
                .collect()
        })
        .join()
        .unwrap();
        assert_eq!(ids, (1..=CLAIM_BATCH as u64).collect::<Vec<_>>());
    }

    #[test]
    fn requeue_returns_unsent_rows_in_order() {
        CLAIMED.with(|c| {
            c.borrow_mut().clear();
            c.borrow_mut().push_back(row(9));
        });
        let mut sent: VecDeque<Sent> = [
            Sent::Row(row(1)),
            Sent::Row(row(2)),
            Sent::Backfill { obj_id: 3, size: 5 },
            Sent::Row(row(4)),
        ]
        .into();
        // The newest two never left, nor did row 5, which could never be
        // sent. Only row 5 is charged; the backfill is dropped.
        assert_eq!(
            requeue(&mut sent, 2, Some(Sent::Row(row(5))), Charge::Failed),
            0
        );
        assert_eq!(sent.len(), 2);
        let claimed: Vec<(u64, u8)> = CLAIMED.with(|c| {
            c.borrow()
                .iter()
                .map(|r| (r.obj_id, r.failed_sends))
                .collect()
        });
        assert_eq!(claimed, vec![(4, 0), (5, 1), (9, 0)]);
    }

    #[test]
    fn only_invalid_input_is_never_sendable() {
        use std::io::{Error, ErrorKind};
        let io = |k| ringline_redis::Error::Io(Error::new(k, "x"));
        assert!(never_sendable(&io(ErrorKind::InvalidInput)));
        assert!(!never_sendable(&io(ErrorKind::Other)));
        assert!(!never_sendable(&io(ErrorKind::NotConnected)));
        assert!(!never_sendable(&ringline_redis::Error::TooManyInFlight));
    }

    #[test]
    fn flushes_before_a_batch_with_a_guard_outgrows_one_slot() {
        let slot = 16_448;
        // Four copied ~4,000-byte SETs fill most of a slot; a zero-copy SET
        // whose framing would overflow it needs the batch flushed first.
        assert!(must_flush_first(16_400, false, 100, true, slot));
        // A batch with a guard flushes before copied bytes overflow a slot.
        assert!(must_flush_first(16_400, true, 100, false, slot));
        // Without any guard, ringline copies across slots: no flush needed.
        assert!(!must_flush_first(16_400, false, 4_100, false, slot));
        // Nothing buffered, or room left: no flush.
        assert!(!must_flush_first(0, true, 20_000, true, slot));
        assert!(!must_flush_first(1_000, true, 100, true, slot));
    }

    #[test]
    fn failed_preflush_puts_next_after_the_discarded_rows() {
        CLAIMED.with(|c| {
            c.borrow_mut().clear();
            c.borrow_mut().push_back(row(9));
        });
        let mut sent: VecDeque<Sent> =
            [Sent::Row(row(1)), Sent::Row(row(2)), Sent::Row(row(3))].into();
        assert_eq!(
            requeue_with_next(&mut sent, 2, Sent::Row(row(4)), Charge::Nothing),
            0
        );
        let claimed: Vec<u64> = CLAIMED.with(|c| c.borrow().iter().map(|r| r.obj_id).collect());
        assert_eq!(claimed, vec![2, 3, 4, 9]);
    }

    #[test]
    fn requeue_charges_by_the_failure_kind() {
        let charged = |charge| {
            CLAIMED.with(|c| c.borrow_mut().clear());
            let mut sent: VecDeque<Sent> = [Sent::Row(row(1))].into();
            requeue(&mut sent, 1, Some(Sent::Row(row(2))), charge);
            CLAIMED.with(|c| {
                c.borrow()
                    .iter()
                    .map(|r| r.failed_sends)
                    .collect::<Vec<_>>()
            })
        };
        assert_eq!(charged(Charge::Nothing), vec![0, 0]);
        assert_eq!(charged(Charge::Failed), vec![0, 1]);
        assert_eq!(charged(Charge::All), vec![1, 1]);
    }

    #[test]
    fn requeue_gives_up_on_a_row_after_its_last_attempt() {
        CLAIMED.with(|c| c.borrow_mut().clear());
        let mut sent = VecDeque::new();
        let mut r = row(1);
        r.failed_sends = MAX_SEND_ATTEMPTS - 1;
        assert_eq!(requeue(&mut sent, 0, Some(Sent::Row(r)), Charge::Failed), 1);
        assert!(CLAIMED.with(|c| c.borrow().is_empty()));
    }
}
