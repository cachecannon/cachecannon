//! The three measured phases: load, index, query.

use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use resp_proto::{Request, Value};

use super::Job;
use super::dataset::Dataset;
use super::report::{PhaseTimings, QueryStats, RunReport};

const KEY_PREFIX: &str = "doc:";
const VECTOR_FIELD: &[u8] = b"vec";
const INDEX_POLL_INTERVAL: Duration = Duration::from_millis(10);
/// A pipeline batch is issued as a single `send()`, so this caps how deep the
/// load pipeline runs. Sends larger than one send-copy pool slot (16 KiB) used
/// to hang the client (a multi-chunk send woke its waiter on the first chunk
/// with a short count); fixed in ringline 0.5.3 (ringline-rs/ringline#299), so
/// batches may now exceed one slot. The cap is byte-based so vector
/// dimensionality cannot blow up the batch size.
const LOAD_BATCH_BYTE_BUDGET: usize = 256 * 1024;
/// Approximate per-command RESP overhead (verb, argument headers, CRLFs).
const HSET_OVERHEAD_BYTES: usize = 64;

/// Connect, run load -> index -> query, and assemble the report.
pub(super) async fn run_all(job: &'static Job) -> Result<RunReport, String> {
    let conn = ringline::connect(job.endpoint)
        .map_err(|e| format!("connect {}: {e}", job.endpoint))?
        .await
        .map_err(|e| format!("connect {}: {e:?}", job.endpoint))?;
    let mut client = ringline_redis::Client::builder(conn).build();
    client
        .ping()
        .await
        .map_err(|e| format!("PING {}: {e}", job.endpoint))?;

    // Reuse mode: verify the index is ready and the keyspace matches the
    // dataset, then go straight to the query phase. Load and build report as
    // zero items / zero elapsed (skipped).
    if job.args.reuse_index {
        verify_reusable_index(&mut client, job).await?;
        eprintln!("reusing existing index; querying");
        let query = query_phase(&mut client, job).await?;
        let skipped = || PhaseTimings {
            items: 0,
            elapsed: Duration::ZERO,
            batch: 0,
        };
        return Ok(RunReport {
            load: skipped(),
            index: skipped(),
            query,
        });
    }

    // A previous run's index would make load throughput and build time lies,
    // and stale `doc:*` hashes beyond this dataset's size would be backfilled
    // into the index and poison recall. Drop the index (missing-index errors
    // are expected) and delete every key under the prefix before loading.
    let _ = client
        .cmd(&Request::cmd(b"FT.DROPINDEX").arg(job.args.index.as_bytes()))
        .await;
    delete_prefix(&mut client, KEY_PREFIX).await?;

    eprintln!(
        "cleanup done; loading {} vectors",
        job.dataset.train.nrows()
    );
    let load = load_phase(&mut client, job).await?;
    eprintln!(
        "load done in {:.2}s; building index",
        load.elapsed.as_secs_f64()
    );
    let index = index_phase(&mut client, job).await?;
    eprintln!(
        "index ready in {:.2}s; querying",
        index.elapsed.as_secs_f64()
    );
    let query = query_phase(&mut client, job).await?;

    if !job.args.keep_index {
        let _ = client
            .cmd(&Request::cmd(b"FT.DROPINDEX").arg(job.args.index.as_bytes()))
            .await;
    }

    Ok(RunReport { load, index, query })
}

/// Preconditions for `--reuse-index`: the index exists and is fully ready,
/// and the keyspace holds exactly the dataset's train vectors.
async fn verify_reusable_index(
    client: &mut ringline_redis::Client,
    job: &Job,
) -> Result<(), String> {
    let info = client
        .cmd(&Request::cmd(b"FT.INFO").arg(job.args.index.as_bytes()))
        .await
        .map_err(|e| format!("FT.INFO: {e}"))?;
    let fields = match info {
        Value::Array(items) => items,
        Value::Error(e) => {
            return Err(format!(
                "--reuse-index: FT.INFO {}: {}",
                job.args.index,
                String::from_utf8_lossy(&e)
            ));
        }
        other => {
            return Err(format!(
                "--reuse-index: FT.INFO: unexpected reply {other:?}"
            ));
        }
    };
    if !index_ready(&fields)? {
        return Err("--reuse-index: index exists but is not fully ready".into());
    }
    let dbsize = client
        .cmd(&Request::cmd(b"DBSIZE"))
        .await
        .map_err(|e| format!("DBSIZE: {e}"))?;
    match dbsize {
        Value::Integer(n) if n as usize == job.dataset.train.nrows() => Ok(()),
        Value::Integer(n) => Err(format!(
            "--reuse-index: DBSIZE {} does not match dataset train count {}",
            n,
            job.dataset.train.nrows()
        )),
        other => Err(format!("--reuse-index: DBSIZE: unexpected reply {other:?}")),
    }
}

/// Load every `train` vector as `HSET doc:<row> vec <f32-LE>`, pipelined.
async fn load_phase(
    client: &mut ringline_redis::Client,
    job: &Job,
) -> Result<PhaseTimings, String> {
    let train = &job.dataset.train;
    let batch = job.args.load_batch.max(1);
    let start = Instant::now();

    // Rows per batch: capped by --load-batch and by the byte budget.
    let row_cost = 4 * train.ncols() + KEY_PREFIX.len() + 20 + HSET_OVERHEAD_BYTES;
    if row_cost > LOAD_BATCH_BYTE_BUDGET {
        return Err(format!(
            "a single {}-dim vector ({row_cost} bytes as an HSET) exceeds the \
             per-send budget of {LOAD_BATCH_BYTE_BUDGET} bytes; the send path \
             cannot carry it (see the ceiling note above)",
            train.ncols(),
        ));
    }
    let batch = batch.min((LOAD_BATCH_BYTE_BUDGET / row_cost).max(1));

    let mut row = 0usize;
    while row < train.nrows() {
        let end = (row + batch).min(train.nrows());
        // Blobs and keys must outlive the Request borrows in this batch.
        let mut keys: Vec<Vec<u8>> = Vec::with_capacity(end - row);
        let mut blobs: Vec<Vec<u8>> = Vec::with_capacity(end - row);
        for i in row..end {
            keys.push(format!("{KEY_PREFIX}{i}").into_bytes());
            blobs.push(Dataset::f32_le_blob(train.row(i)));
        }
        let mut pipeline = client.pipeline();
        for (key, blob) in keys.iter().zip(blobs.iter()) {
            pipeline = pipeline.cmd(&Request::cmd(b"HSET").arg(key).arg(VECTOR_FIELD).arg(blob));
        }
        let replies = pipeline
            .execute()
            .await
            .map_err(|e| format!("load batch at row {row}: {e}"))?;
        for reply in &replies {
            if let Value::Error(e) = reply {
                return Err(format!(
                    "HSET error during load: {}",
                    String::from_utf8_lossy(e)
                ));
            }
        }
        row = end;
    }

    Ok(PhaseTimings {
        items: train.nrows(),
        elapsed: start.elapsed(),
        batch,
    })
}

/// `FT.CREATE` the HNSW index, then poll `FT.INFO` until the backfill is
/// complete and the mutation queue has drained. The reported build time is
/// create-to-ready; the `FT.CREATE` reply itself returns immediately.
async fn index_phase(
    client: &mut ringline_redis::Client,
    job: &Job,
) -> Result<PhaseTimings, String> {
    let dim = job.dataset.dim().to_string();
    let m = job.args.m.to_string();
    let ef_construction = job.args.ef_construction.to_string();
    let metric = job.dataset.distance_metric_arg()?;

    let create = Request::cmd(b"FT.CREATE")
        .arg(job.args.index.as_bytes())
        .arg(b"ON")
        .arg(b"HASH")
        .arg(b"PREFIX")
        .arg(b"1")
        .arg(KEY_PREFIX.as_bytes())
        .arg(b"SCHEMA")
        .arg(VECTOR_FIELD)
        .arg(b"VECTOR")
        .arg(b"HNSW")
        .arg(b"10")
        .arg(b"TYPE")
        .arg(b"FLOAT32")
        .arg(b"DIM")
        .arg(dim.as_bytes())
        .arg(b"DISTANCE_METRIC")
        .arg(metric)
        .arg(b"M")
        .arg(m.as_bytes())
        .arg(b"EF_CONSTRUCTION")
        .arg(ef_construction.as_bytes());

    let start = Instant::now();
    match client.cmd(&create).await {
        Ok(Value::Error(e)) => {
            return Err(format!("FT.CREATE: {}", String::from_utf8_lossy(&e)));
        }
        Ok(_) => {}
        Err(e) => return Err(format!("FT.CREATE: {e}")),
    }

    loop {
        let info = client
            .cmd(&Request::cmd(b"FT.INFO").arg(job.args.index.as_bytes()))
            .await
            .map_err(|e| format!("FT.INFO: {e}"))?;
        let fields = match info {
            Value::Array(items) => items,
            Value::Error(e) => return Err(format!("FT.INFO: {}", String::from_utf8_lossy(&e))),
            other => return Err(format!("FT.INFO: unexpected reply {other:?}")),
        };
        // Valkey Search reports `state` / `backfill_in_progress` /
        // `mutation_queue_size` (there is no RediSearch-style
        // `percent_indexed` field).
        if index_ready(&fields)? {
            break;
        }
        ringline::sleep(INDEX_POLL_INTERVAL).await;
    }

    Ok(PhaseTimings {
        items: job.dataset.train.nrows(),
        elapsed: start.elapsed(),
        batch: 0,
    })
}

/// Prebuilt per-run query strings shared by every query connection.
struct QueryCtx {
    knn: String,
    k_arg: String,
    ef_arg: String,
}

impl QueryCtx {
    fn new(job: &Job) -> Self {
        let k = job.args.k;
        QueryCtx {
            knn: format!(
                "*=>[KNN {k} @{} $BLOB EF_RUNTIME $EF]",
                String::from_utf8_lossy(VECTOR_FIELD)
            ),
            k_arg: k.to_string(),
            ef_arg: job.args.ef_search.to_string(),
        }
    }
}

/// Issue query `qi`, returning (latency_ns, recall@k for this query).
async fn run_one_query(
    client: &mut ringline_redis::Client,
    job: &Job,
    ctx: &QueryCtx,
    qi: usize,
    returned_ids: &mut Vec<i64>,
) -> Result<(u64, f64), String> {
    let k = job.args.k;
    let blob = Dataset::f32_le_blob(job.dataset.test.row(qi));
    let request = Request::cmd(b"FT.SEARCH")
        .arg(job.args.index.as_bytes())
        .arg(ctx.knn.as_bytes())
        .arg(b"PARAMS")
        .arg(b"4")
        .arg(b"BLOB")
        .arg(&blob)
        .arg(b"EF")
        .arg(ctx.ef_arg.as_bytes())
        .arg(b"NOCONTENT")
        .arg(b"LIMIT")
        .arg(b"0")
        .arg(ctx.k_arg.as_bytes())
        .arg(b"DIALECT")
        .arg(b"2");

    let t0 = Instant::now();
    let reply = client
        .cmd(&request)
        .await
        .map_err(|e| format!("FT.SEARCH query {qi}: {e}"))?;
    let latency_ns = t0.elapsed().as_nanos() as u64;

    returned_ids.clear();
    parse_knn_reply(&reply, returned_ids).map_err(|e| format!("FT.SEARCH query {qi}: {e}"))?;

    // recall@k = |returned ∩ true_k| / k (ann-benchmarks definition).
    // Ground truth is integer indices; returned keys are `doc:<i>` and
    // were parsed back to integers above. Never intersect raw keys.
    // Set semantics: a duplicated id in the reply must not count twice.
    returned_ids.sort_unstable();
    returned_ids.dedup();
    let truth = job.dataset.neighbors.row(qi);
    let hits = returned_ids
        .iter()
        .filter(|id| truth.iter().take(k).any(|t| i64::from(*t) == **id))
        .count();
    Ok((latency_ns, hits as f64 / k as f64))
}

/// Run the test set (`--query-loops` times) through `FT.SEARCH` KNN.
///
/// With `--query-clients 1` (default) all queries run on the caller's
/// connection, one request in flight: the single-client latency measurement.
/// With more, that many connections are opened and each runs closed-loop with
/// one request in flight, dealing query indices from a shared counter: a
/// closed-loop throughput measurement with per-request latencies pooled
/// across connections.
async fn query_phase(
    client: &mut ringline_redis::Client,
    job: &'static Job,
) -> Result<QueryStats, String> {
    let n = job.dataset.test.nrows();
    let total = n * job.args.query_loops.max(1);
    let clients = job.args.query_clients.max(1);

    if clients == 1 {
        let ctx = QueryCtx::new(job);
        let mut latencies_ns: Vec<u64> = Vec::with_capacity(total);
        let mut recall_sum = 0.0f64;
        let mut returned_ids: Vec<i64> = Vec::with_capacity(job.args.k);

        let started = Instant::now();
        for idx in 0..total {
            let (lat, recall) =
                run_one_query(client, job, &ctx, idx % n, &mut returned_ids).await?;
            latencies_ns.push(lat);
            recall_sum += recall;
        }
        let elapsed = started.elapsed();

        return Ok(QueryStats {
            queries: total,
            elapsed,
            latencies_ns,
            mean_recall: recall_sum / total as f64,
        });
    }

    // Concurrent: one task per connection, shared dealer over query indices.
    let dealer = Rc::new(AtomicUsize::new(0));
    let started = Instant::now();
    let mut handles = Vec::with_capacity(clients);
    for _ in 0..clients {
        let dealer = Rc::clone(&dealer);
        let handle = ringline::spawn_with_handle(async move {
            let conn = match ringline::connect(job.endpoint) {
                Ok(f) => match f.await {
                    Ok(c) => c,
                    Err(e) => return Err(format!("connect {}: {e:?}", job.endpoint)),
                },
                Err(e) => return Err(format!("connect {}: {e}", job.endpoint)),
            };
            let mut client = ringline_redis::Client::builder(conn).build();
            let ctx = QueryCtx::new(job);
            let mut latencies_ns: Vec<u64> = Vec::new();
            let mut recall_sum = 0.0f64;
            let mut returned_ids: Vec<i64> = Vec::with_capacity(job.args.k);
            loop {
                let idx = dealer.fetch_add(1, Ordering::Relaxed);
                if idx >= total {
                    break;
                }
                let (lat, recall) =
                    run_one_query(&mut client, job, &ctx, idx % n, &mut returned_ids).await?;
                latencies_ns.push(lat);
                recall_sum += recall;
            }
            Ok::<_, String>((latencies_ns, recall_sum))
        })
        .map_err(|e| format!("spawn query worker: {e}"))?;
        handles.push(handle);
    }

    let mut latencies_ns: Vec<u64> = Vec::with_capacity(total);
    let mut recall_sum = 0.0f64;
    for handle in handles {
        let (lat, recall) = handle.await?;
        latencies_ns.extend(lat);
        recall_sum += recall;
    }
    let elapsed = started.elapsed();

    Ok(QueryStats {
        queries: total,
        elapsed,
        latencies_ns,
        mean_recall: recall_sum / total as f64,
    })
}

/// Extract the doc ids from a `FT.SEARCH ... NOCONTENT` RESP2 reply:
/// `[count, "doc:<i>", "doc:<j>", ...]`.
fn parse_knn_reply(reply: &Value, out: &mut Vec<i64>) -> Result<(), String> {
    let items = match reply {
        Value::Array(items) => items,
        Value::Error(e) => return Err(String::from_utf8_lossy(e).into_owned()),
        other => return Err(format!("unexpected reply {other:?}")),
    };
    for item in items.iter().skip(1) {
        let key = match item {
            Value::BulkString(b) | Value::SimpleString(b) => String::from_utf8_lossy(b),
            other => return Err(format!("unexpected key element {other:?}")),
        };
        let id = key
            .strip_prefix(KEY_PREFIX)
            .and_then(|s| s.parse::<i64>().ok())
            .ok_or_else(|| format!("key {key:?} is not {KEY_PREFIX}<integer>"))?;
        out.push(id);
    }
    Ok(())
}

/// Look up a named field in an `FT.INFO` reply (alternating name/value list),
/// normalizing the value to a string.
fn info_field(fields: &[Value], name: &str) -> Option<String> {
    let mut iter = fields.iter();
    while let Some(field_name) = iter.next() {
        let value = iter.next()?;
        let matches = match field_name {
            Value::BulkString(b) | Value::SimpleString(b) => b.as_ref() == name.as_bytes(),
            _ => false,
        };
        if matches {
            return Some(match value {
                Value::BulkString(b) | Value::SimpleString(b) => {
                    String::from_utf8_lossy(b).into_owned()
                }
                Value::Integer(i) => i.to_string(),
                other => format!("{other:?}"),
            });
        }
    }
    None
}

/// Delete every key matching `<prefix>*`. Stale keys from a previous
/// (possibly larger) run would otherwise be backfilled into the new index and
/// corrupt recall scoring.
///
/// The scan and the deletes both happen server-side in a short script, so the
/// client only ever parses the returned cursor. A client-side SCAN is a trap
/// here: COUNT is a per-bucket hint the server can overshoot severalfold, and
/// a reply over 1024 elements trips the reply parser's collection cap (which
/// closes the connection rather than truncating).
async fn delete_prefix(client: &mut ringline_redis::Client, prefix: &str) -> Result<(), String> {
    // COUNT 1000 keeps the DEL argument list far below Lua's unpack limit.
    const SCRIPT: &[u8] =
        b"local r = redis.call('SCAN', ARGV[1], 'MATCH', ARGV[2], 'COUNT', 1000) \
         if #r[2] > 0 then redis.call('DEL', unpack(r[2])) end \
         return r[1]";
    let pattern = format!("{prefix}*");
    let mut cursor: Vec<u8> = b"0".to_vec();
    loop {
        let reply = client
            .cmd(
                &Request::cmd(b"EVAL")
                    .arg(SCRIPT)
                    .arg(b"0")
                    .arg(&cursor)
                    .arg(pattern.as_bytes()),
            )
            .await
            .map_err(|e| format!("cleanup EVAL {pattern}: {e}"))?;
        cursor = match reply {
            Value::BulkString(b) | Value::SimpleString(b) => b.to_vec(),
            Value::Error(e) => {
                return Err(format!(
                    "cleanup EVAL {pattern}: {}",
                    String::from_utf8_lossy(&e)
                ));
            }
            other => {
                return Err(format!(
                    "cleanup EVAL {pattern}: unexpected reply {other:?}"
                ));
            }
        };
        if cursor == b"0" {
            return Ok(());
        }
    }
}

/// Readiness from an `FT.INFO` reply, supporting both server families.
///
/// Valkey Search reports `state` / `backfill_in_progress` /
/// `mutation_queue_size`; RediSearch (Redis Query Engine) reports
/// `percent_indexed` / `indexing`. Treating an unknown shape as ready would
/// report a fictitious build time, so anything else is an error.
fn index_ready(fields: &[Value]) -> Result<bool, String> {
    let state = info_field(fields, "state");
    let backfill = info_field(fields, "backfill_in_progress");
    let queue = info_field(fields, "mutation_queue_size");
    if let (Some(state), Some(backfill), Some(queue)) = (state, backfill, queue) {
        return Ok(state == "ready" && backfill == "0" && queue == "0");
    }
    if let Some(pct) = info_field(fields, "percent_indexed") {
        let done = pct
            .parse::<f64>()
            .map_err(|_| format!("percent_indexed not numeric: {pct:?}"))?
            >= 1.0;
        let indexing = info_field(fields, "indexing").unwrap_or_else(|| "0".to_string());
        return Ok(done && indexing == "0");
    }
    Err(
        "FT.INFO reply has neither the Valkey Search fields (state/backfill_in_progress/\
         mutation_queue_size) nor the RediSearch fields (percent_indexed/indexing); \
         unsupported server?"
            .to_string(),
    )
}
