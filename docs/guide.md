# Benchmark Practical Guide

This guide walks through common benchmarking tasks — from first run to production-grade SLO validation.

## End-to-End Walkthrough

This walkthrough takes you from zero to analyzed results. It assumes you have a Valkey or Redis-compatible server running on `127.0.0.1:6379`.

### 1. Start the server

```bash
# Use any Valkey/Redis-compatible server running on port 6379
valkey-server  # or: redis-server
```

### 2. Run a benchmark with Parquet output

```bash
./target/release/cachecannon config/valkey.toml \
    --parquet results.parquet
```

The benchmark goes through these phases automatically:

1. **Connect** — Establishes TCP connections to the server
2. **Prefill** — *(if enabled)* Writes every key once to populate the cache
3. **Warmup** — Runs the workload without recording metrics, letting caches stabilize
4. **Running** — Records metrics for the configured duration
5. **Stop** — Drains in-flight requests and writes the Parquet file

During the Running phase, you'll see per-second output like:

```
  Time   Req/s    Err/s   Hit%     P50     P90     P99   P99.9  P99.99     Max
─────────────────────────────────────────────────────────────────────────────────
  1.0s   523847       0  79.2%    48µs    89µs   156µs   312µs   891µs  1.24ms
  2.0s   531294       0  79.4%    47µs    87µs   152µs   298µs   756µs  1.12ms
```

### 3. View results in the dashboard

```bash
./target/release/cachecannon view results.parquet
```

This launches a local web server and opens an interactive dashboard in your browser. See the [Results Viewer](reference.md#results-viewer) section in the reference for details on each chart.

### 4. Interpreting results

Key things to look for:

- **Throughput stability** — Is req/s consistent or oscillating? Oscillation may indicate GC pauses, eviction storms, or connection churn.
- **Tail latency** — P99.9 and P99.99 reveal worst-case behavior. A P99.9 that is 10x the P50 suggests queuing or contention.
- **Hit rate** — With a uniform keyspace and 80/20 read/write mix, expect ~80% hit rate at steady state. Lower values may mean the cache is undersized for the keyspace.
- **Error rate** — Should be 0%. Non-zero errors indicate server overload, timeouts, or connection failures.

### Correlating with Rezolus

For deeper analysis, capture system-level metrics with [Rezolus](https://github.com/brayniac/rezolus) during the benchmark:

```bash
# Terminal 1: Record server-side telemetry
rezolus record http://localhost:4241 server-rezolus.parquet &

# Terminal 2: Record client-side telemetry
rezolus record http://localhost:4241 client-rezolus.parquet &

# Terminal 3: Run the benchmark
./target/release/cachecannon config/valkey.toml \
    --parquet results.parquet

# View everything together
./target/release/cachecannon view results.parquet \
    --server server-rezolus.parquet \
    --client client-rezolus.parquet
```

This lets you correlate cache performance with CPU utilization, network statistics, scheduler behavior, and other system metrics.

#### One archive per run

Rezolus can also record the benchmark itself. With `[admin] listen` set, the
benchmark serves its metrics endpoint, so a single invocation captures all
three sources as labelled recordings inside one `.dendro` archive:

```bash
# Terminal 1: one recorder, three endpoints
rezolus record \
    --endpoint http://localhost:9090,source=cachecannon,role=loadgen \
    --endpoint http://server:4241,role=service \
    --endpoint http://client:4241 \
    -o run.dendro

# Terminal 2: run the benchmark with its metrics endpoint enabled
./target/release/cachecannon config/valkey.toml
```

rezolus 6 records the benchmark from `/metrics/stream`: rows stamped by the
benchmark when it read them, with the latency histograms as histograms, and
cachecannon's KPI template in the recording's metadata. It writes `.dendro`
only (the default with no `-o`) and refuses `-o run.rez`. This needs a recorder
that accepts the `application/vnd.metriken.replication.v1+dendro` content type
(a rezolus built from main at 6.0.0-alpha.45 or later; no 6.0 release is
published yet); an older 6.0 recorder refuses the stream and
the whole run at startup. A rezolus 5.x recorder writes `.rez` instead, from
the msgpack snapshot on `/metrics/binary`. Keep the `source=cachecannon` label:
Rezolus finds its built-in copy of the template by that name for a recording
that carries none, and a 5.x recorder that reached the benchmark through
`/metrics/binary` names the recording `rezolus` otherwise.

Every recording shares one timeline, so benchmark and system metrics line up
without matching timestamps across files by hand. Analysis happens in Rezolus
(`rezolus view run.dendro`); `cachecannon view` reads Parquet, so keep
`--parquet results.parquet` if you also want the built-in dashboards.

## Workload Patterns

### Read-Heavy (Default)

```toml
[workload.commands]
get = 80
set = 20
```

Typical for caching workloads where most requests are reads.

### Write-Heavy

```toml
[workload.commands]
get = 20
set = 80
```

Tests cache write performance and eviction behavior.

### Read-Only

```toml
[workload]
prefill = true

[workload.commands]
get = 100
set = 0
```

Requires prefill to populate the cache first. Tests pure read throughput.

### Zipfian Distribution

```toml
[workload.keyspace]
distribution = "zipf"
count = 10000000
```

Realistic access pattern where some keys are "hot" (accessed frequently). Better simulates production traffic.

### Append Stream

```toml
[workload]
prefill = true
backfill_on_miss = true

[workload.keyspace]
count = 10000000
distribution = "recency"
hot_generations = 8
hot_decay = 0.5
hot_fraction = 0.8

[workload.append]
batch = 100000
every = "60s"
pace = "burst"
connections = 2

[workload.commands]
get = 100
```

Models a keyspace that grows over the run and whose hot set moves with it: an
append-only store where new records arrive in batches and the most recent
records are the ones being read. The stationary distributions cannot express
this, because with them the same keys are hot for the whole run.

How it fits together:

- **Key ids are a monotonic sequence.** Prefill writes the initial body,
  `0..count`. Each append batch writes the next `batch` ids. Old ids are never
  reused, so a cache can never serve a stale hit for a "regenerated" key; the
  tail simply ages out of the hot window and the server's own eviction reclaims
  it.
- **The head advances by writes, not by the clock.** A batch opens every
  `every` (measured from the start of warmup) and the keyspace head moves only
  once every SET in the batch is confirmed. Reads never target an unwritten key,
  and the drift rate equals the write rate by construction. If a batch takes
  longer than the interval to drain, the next one waits rather than stacking.
- **Hotness attaches to the batch, not the key.** A read picks one of the
  `hot_generations` newest batches with weight `hot_decay^age`, then a key
  uniformly within it. `hot_fraction` of reads go to that window; the rest go
  uniformly to the body below it. Before the first batch lands, the hot window
  is the newest `hot_generations × batch` ids of the prefilled body.
- **Writers have their own connections.** `append.connections` are a separate
  pool with their own pipeline depth. Sharing the reader pipelines would queue
  reads behind a burst of SETs inside the client, so read latency would carry an
  artifact of the load generator rather than the server's behaviour under write
  load. Append SETs are reported on their own line (`APPEND SET`) and do not
  count toward the read workload's request, response, or latency totals.
- **`pace`** chooses between firing the batch as fast as the writers allow
  (`burst`) or pacing it evenly across the interval (`spread`).

Combined with `backfill_on_miss`, a read that misses on an evicted key refills
it from the reader side, which is where a read-through cache does that work.

Runs against this model are usually long, because the interesting timescale is
the cache's fill time against the append cadence. Shrinking `every` to compress
a run changes what is being measured: fill time does not scale with it. Prefer
real cadence with Parquet output and the viewer.

## KV-Cache Prefix Replay

```toml
[general]
duration = "3600s"   # an upper bound; the run ends with the trace
warmup = "0s"

[connection]
connections = 64     # trace requests in flight
pipeline_depth = 32  # GETs or SETs in flight within one trace request

[workload.kv_replay]
trace = "requests.parquet"
speedup = 1.0
model_config = "config.json"
requests_out = "kv-requests.csv"
```

Replays a request-level KV-cache trace against the server as an LLM serving
engine uses a remote KV-cache store for prefix caching, and reports how many
prompt tokens were served from the cache (cached tokens) and how many had to be
computed (new tokens), with how long the lookups, loads and stores took.
Results are in tokens; converting them to GPU time or cost is left to the
reader. `config/kv-replay.toml` is a complete example and `docs/reference.md`
"KV-Cache Replay Settings" lists every key and output field.

### The trace and the objects

The trace is the kvtrace request format (`crates/kvtrace` in
[pelikan-io/cachesim](https://github.com/pelikan-io/cachesim)): one Parquet row
per inference request with its timestamp, `req_id`, `input_tokens`,
`output_tokens`, the ids of its full prefix chunks, and a session id, plus the
tokens per chunk in the file metadata. Equal chunk ids mean equal prefix
content. Tokens after the last full chunk are not represented, because LMCache
does not store a partial chunk.

Rows are in timestamp order; kvtrace's writer and reader both enforce it. A
reader thread streams the file during the run, at most 16,384 requests ahead
of the replay, so memory does not grow with trace length. At startup the
replay reads the first request and the most chunks in any request (the
footer's `kvtrace.max_chunks`, or a pass over the `chunks` column for a file
without it), which sizes the send pool, and the reader thread starts filling
its read-ahead. When precheck completes the run waits for the read-ahead to
fill. A read error found while filling it, including a row out of timestamp
order, ends the run with that error before any trace request is sent. A read
error found later stops the replay at that row: the requests already read
finish, the results print, and the run exits with the error.
`kv_trace_read_waits` counts dispatcher polls (one per worker per
millisecond) that found a connection idle and the reader behind. In open
loop that wait is part of `kv_schedule_lag`; in closed loop it lowers the
request rate.

kvtrace's `ObjectMapper` turns a request's chunks into the objects the engine
stores, for a model geometry: one object per chunk, keyed `key_prefix` + the
object id (`chunk << 16 | slot << 12 | rank`, rank 0), sized chunk tokens ×
KV bytes per token. Following LMCache v0.5.5
(05a013b) and vLLM v0.31.0 (db9527a), as kvtrace documents:

- One object per chunk, of the whole KV: `tp` must be 1. With tensor
  parallelism each rank stores its own shard as a separate object (LMCache's
  key includes the worker id, `lmcache/utils.py:389-441`) over its own
  connection, and the replay does not open a connection per rank yet
  (cachecannon#214).
- An MLA model's object is the full latent (LMCache stores it from rank 0
  only, `save_only_first_rank`, `lmcache/v1/cache_engine.py:116-119`).
- Hybrid models (linear-attention layers with a recurrent state) are rejected.
  kvtrace's geometry for them is an approximation: object sizes, the chunk
  length vLLM and LMCache allow, and the number of state snapshots differ from
  what LMCache stores.

### One trace request

Each trace request runs on one connection, in three dependent steps:

1. **Lookup.** One `EXISTS` per full chunk's object, all sent in one
   pipeline. No multi-key `EXISTS` is used. The hit prefix is the chunks
   before the first missing object.
2. **Load.** `GET` of every object in the hit prefix, at most
   `connection.pipeline_depth` in flight. A `GET` that returns nil (the object
   was evicted between its `EXISTS` and its `GET`) is counted in `get_nil`, and
   the cached prefix ends at that chunk. A `GET` answered with an error ends it
   the same way.
3. **Store.** `SET` of the objects the write rule selects, at most
   `pipeline_depth` in flight, each value the object's size:
   - `write_rule = "set-from-miss"` (default): every object from the end of the
     lookup's prefix to the last full chunk, present or not. LMCache's
     in-process remote backends put every object they are given without
     checking the store (`lmcache/v1/storage_backend/remote_backend.py:263-270`,
     `323-331`). That holds for the default, non-layerwise store; with
     `use_layerwise` the cache engine skips a chunk whose first layer's key is
     already present (`lmcache/v1/cache_engine.py:676`, `store_layer`), which
     is closer to insert-if-absent.
   - `write_rule = "insert-if-absent"`: every object whose `EXISTS` returned 0.
     LMCache's multiprocess store reserves only keys that are not present
     (`reserve_write(..., "new")`,
     `lmcache/v1/distributed/storage_manager.py:182-200`).

   The store starts where the lookup's prefix ended, not where the load did:
   LMCache records the lookup result as already saved
   (`num_saved_tokens = lmcache_cached_tokens`,
   `lmcache/integration/vllm/vllm_v1_adapter.py:193`, `:247`) and skips that
   many leading tokens, rounded down to a chunk, when it stores (`:326`,
   `:1059-1065`). An object that came back nil is therefore not written again
   by the request that found it missing.

### Token accounting

For a request with `input_tokens` prompt tokens whose first `h` chunks of
`chunk_tokens` were loaded:

- `cached_tokens = h × chunk_tokens`, except when that is at least
  `input_tokens`: then `cached_tokens = input_tokens - 1`. vLLM must compute
  at least the last prompt token to produce output, so LMCache reports one
  token fewer on a full-prompt hit
  (`lmcache/integration/vllm/vllm_v1_adapter.py:1443-1445`, "In full-prompt-hit
  case, we need to recompute the last token"). A full-prompt hit needs a prompt
  that is a whole number of chunks, since the partial tail is never stored.
  It is counted in `full_hits`.
- `new_tokens = input_tokens - cached_tokens`.
- `input_tokens = 0` gives 0 cached and 0 new tokens, whatever was found.
  kvtrace keeps the prompt length as the source gives it, so a row can have
  full chunks and `input_tokens = 0`; its operations are still sent.

A request shorter than one chunk has no objects, sends nothing, and counts all
its tokens as new (`no_full_chunk`).

### Timing

- **Open loop** (`speedup > 0`, default 1): each trace request is due at its
  trace time, measured from the first request's, divided by `speedup`, and
  starts then whether or not earlier ones have finished, as long as a
  connection is free. Each connection replays one trace request at a time, so
  `connections` is the most that can be in flight. A request that starts late
  because none was free, or because the generator fell behind, starts at once
  and `schedule_lag` records by how much: read it before the other numbers.
- **Closed loop** (`speedup = 0`): every connection starts its next trace
  request as soon as its previous one finished, so `connections` is the
  concurrency. Trace timestamps are ignored. Results say `"timing":
  "closed-loop"` and have no `schedule_lag`. Closed loop finds how fast the
  server can serve the trace; it does not show how the server behaves at the
  trace's own arrival rate.

Each worker has one dispatcher task that hands the next trace request to one of
its idle connections and sleeps until the request is due, so the schedule costs
one timer per worker rather than one per connection or request.

With `general.warmup > 0` the replay starts with warmup, and trace requests that
finish during it warm the cache without being recorded. With `warmup = "0s"`
the replay starts at the beginning of recording and every request is recorded.
The run ends when every trace request has finished, or after warmup +
`duration`, whichever is first.

### Which LMCache connector this matches

LMCache picks its remote connector from the URL scheme of `remote_url`. The
replay's shape is one key per object and one `EXISTS`, one `GET` and one `SET`
per object, sent pipelined on one connection. Checked against LMCache v0.5.5
(05a013b):

- **`resp://`** (`RESPConnector`, `lmcache/v1/storage_backend/connector/redis_adapter.py:18-70`,
  `redis_connector.py:38-235`): matches the key and operation counts. Each
  object is one key, and `EXISTS`, `GET` and `SET` use that key
  (`redis_connector.py:65-66`, `88`, `101`). The lookup checks every key and
  counts the prefix up to the first missing one (`redis_connector.py:164-188`).
  It differs in concurrency: the native client splits a batch into up to
  `resp_num_threads` tiles (default 8, `redis_adapter.py:44`), each on its own
  connection doing one request at a time
  (`csrc/storage_backends/connector_base.h:313-315`, `335-340`;
  `csrc/storage_backends/redis/connector.cpp:315-337`).
- **`valkey://`** (`ValkeyConnector`, `valkey_connector.py`): matches the key and
  operation counts; one key per object (module docs, `valkey_connector.py:19-20`).
  It sends each `EXISTS`, `GET` and `SET` as its own request through a thread
  pool of clients (`valkey_connector.py:222-234`, `253-319`, `335-347`, `357-370`), not as
  one pipeline, and supports cluster mode, which the replay does not.
- **`redis://`** (`RedisConnector`, `redis_connector.py:238-420`): does not
  match. Each object is two keys, `<key>kv_bytes` and `<key>metadata`. The
  lookup checks `EXISTS <key>metadata` one key at a time and stops at the
  first miss (`redis_connector.py:265-267`, `399-411`), so a lookup sends hit
  chunks + 1 `EXISTS` rather than one per object. A load is two `GET`s per
  object, metadata then `kv_bytes` (`278-300`), and a store two `SET`s
  (`378-379`). The server therefore holds twice the keys, and each cached
  object costs twice the `GET`s and `SET`s the replay sends.

The lookup, load and store times here are those of one pipelined connection
per trace request; for `resp://` and `valkey://` they would be spread over
several connections or threads.

### What is not modelled

- LMCache's local CPU tier. `local_cpu` defaults to true with
  `max_local_cpu_size` 5 GB (`lmcache/v1/config.py:91-96`), and a lookup that
  finds a chunk there never reaches the remote store. The replay sends every
  lookup and load to the server, so for prefixes hot enough to stay in that
  tier it overstates the remote operations.
- The engine's own GPU prefix cache. Every request does the lookup, so a
  prefix that vLLM would find in its own cache is looked up and loaded from the
  server here.
- Retries, timeouts and failure handling of the engine. A trace request whose
  connection fails is abandoned and counted in `requests_failed`, except when
  the connection closed before the server answered any of the request's
  operations, as when a server closes idle connections. Then the request goes
  back to its worker's dispatcher and runs on the next idle connection, ahead
  of any request claimed after it, while the closed connection reconnects:
  - found closed when the request reached it, with nothing sent: at most once
    per connection on the worker, so an idle timeout that closes every
    connection at once costs no request;
  - closed after the lookup was sent and before any reply: once per request.

  A handed-back request is counted as failed when it has used those up, or
  when none of its worker's connections is connected and a reconnect attempt
  has failed (refused, a failed `HELLO 3` or precheck, or a new connection
  closed before it ran anything). An open-loop request already claimed and
  waiting for its start time is treated the same way once it is due.
  Without that it could keep the run going until `duration`.
- Multiple endpoints and cluster mode: every key of a request must be on the
  server its connection reaches.

## Prefill

Prefill populates the cache with every key in the keyspace before the benchmark begins. This is useful for read-only workloads, hit-rate measurements, or any test where you need the cache to be warm from the start.

### Enabling Prefill

```toml
[workload]
prefill = true
```

### How It Works

1. After connections are established, each worker thread receives a deterministic slice of the keyspace to write. Keys are distributed proportionally based on each worker's connection count.
2. Workers issue SET commands for each assigned key, throttled to 16 in-flight requests per connection to avoid overwhelming the server.
3. The benchmark monitors global progress. Once all workers have confirmed their assigned keys, the prefill phase ends and warmup begins.

### Timeout and Stall Detection

Prefill has a default timeout of **300 seconds**. If no progress is made for 30 consecutive seconds (stall detection), the benchmark aborts with diagnostic information including active workers, connection failures, and bytes transferred. You can adjust the timeout:

```toml
[workload]
prefill = true
prefill_timeout = "600s"   # Increase for very large keyspaces
# prefill_timeout = "0s"   # Disable timeout entirely
```

### Pacing Prefill

By default prefill is unpaced: every connection writes as fast as the server
answers. On a fast network that can exceed what the host or the network will
carry, and dropped packets and retransmissions during prefill leave TCP state
behind for the measurement that follows. `prefill_rate_limit` paces prefill in
requests per second. All workers draw on one limiter, as they draw on the same
prefill queues, so a worker whose endpoints have run out of keys leaves the
rate to the others. The limiter starts empty when prefill begins and holds at
most 10 ms of the rate or one batch, whichever is more, plus one batch, so
prefill starts at the limit rather than with a burst. A connection the limiter turns away sleeps longer the more
connections are already waiting, so a large connection count does not poll it
at the token rate.

```toml
[workload]
prefill = true
prefill_rate_limit = 10000   # requests/second; omit or 0 for unpaced
```

Choose the rate from the value size: a limit of `R` requests per second writes
about `R * value_length` bytes per second. A paced prefill takes about
`keyspace.count / R` seconds; a config where that exceeds `prefill_timeout` is
rejected, so raise the timeout (or set it to `"0s"`) for a slow prefill of a
large keyspace. The limit has no effect when `prefill` is off.

### When to Use Prefill

- **Read-only benchmarks** (`get = 100, set = 0`) — without prefill, every GET will miss
- **Hit-rate testing** — start from a known cache state instead of random population
- **Latency comparisons** — eliminate the warmup variability of gradual cache filling
- **Cluster mode** — ensure all hash slots have data before measurement

## Backfill on Miss

Backfill implements the **cache-aside** pattern: when a GET misses, the benchmark automatically issues a SET for that key, populating the cache for future reads.

### Enabling Backfill

```toml
[workload]
backfill_on_miss = true
```

### How It Works

1. When a GET response indicates a cache miss, the key ID is added to a per-worker backfill queue.
2. On each event loop tick, the backfill queue is drained before normal workload requests, ensuring misses are filled promptly.
3. Backfill SETs use the same value generation as regular SETs.

### Metrics

Backfill operations are tracked separately from regular SETs:

| Metric | Description |
|--------|-------------|
| `backfill_set_count` | Number of backfill SET operations issued |
| `backfill_set_latency` | Latency histogram for backfill SETs (nanoseconds) |

This lets you distinguish organic write traffic from cache-filling writes when analyzing results.

### When to Use Backfill

- **Gradual cache warming** — the cache fills naturally as reads discover misses, similar to production behavior
- **Testing convergence** — observe how hit rate improves over time as the cache populates
- **Alternative to prefill** — when you want the cache to warm during the measured period rather than before it

## Saturation Search

Saturation search automatically finds the **maximum throughput that meets your latency SLO**. Instead of manually tuning rate limits, the benchmark starts at a low rate and geometrically increases until the SLO is consistently violated.

### Enabling Saturation Search

```toml
[workload.saturation_search]
# Latency SLO thresholds — at least one must be specified
# All specified thresholds must be met for the SLO to pass
slo = { p999 = "1ms" }

# Starting request rate (req/s)
start_rate = 1000

# Increase rate by this factor each step (1.05 = 5% increase)
step_multiplier = 1.05

# Duration to sample at each rate level
sample_window = "5s"

# Stop after this many consecutive SLO violations
stop_after_failures = 3

# Maximum rate to try (absolute ceiling)
max_rate = 100000000

# Minimum ratio of achieved/target throughput (0.0-1.0)
# When achieved throughput falls below this fraction of the target,
# the step fails regardless of latency (detects saturation)
min_throughput_ratio = 0.9
```

### Algorithm

1. Set the initial rate to `start_rate` (default: 1000 req/s)
2. Run the workload at the current rate for `sample_window` (default: 5s)
3. Collect the latency histogram and achieved throughput for the window
4. Check the dual SLO:
   - **Latency**: All specified percentile thresholds must be met (e.g., p99.9 < 1ms), measured against **perceived** latency — see [Perceived latency is the criterion](#perceived-latency-is-the-criterion)
   - **Throughput**: Achieved throughput must be at least `min_throughput_ratio` of the target rate (default: 90%). This detects when the server can't keep up regardless of latency.
5. If the SLO passes: record this rate as the current maximum, multiply by `step_multiplier`, continue
6. If the SLO fails: re-measure the same rate `confirm_failures` more times (default 1) before acting on it; a retry that passes clears the failure
7. If consecutive failures reach `stop_after_failures` (default: 3): stop and report
8. Otherwise: continue stepping up
9. The rate is capped at `max_rate` (default: 100M req/s)

### SLO Options

You can specify any combination of latency percentiles:

```toml
# Single threshold
slo = { p999 = "1ms" }

# Multiple thresholds (all must be met)
slo = { p99 = "500us", p999 = "2ms" }

# Available percentiles: p50, p99, p999
```

### Perceived latency is the criterion

**The SLO is evaluated against `perceived` latency, not `response` latency.**
This is deliberate and it is the difference between a saturation search and a
throughput number that hides coordinated omission, but it has to be known to
read a result.

    response_latency   fire to reply. What the server did.
    schedule_slip      how far behind its own schedule the generator was,
                       measured as the rate-limiter tokens beyond one claim
                       (one batch) divided by rate; less than a claim cannot
                       be sent yet, so it is not counted.
    perceived_latency  response_latency + schedule_slip. What a client that
                       wanted to send at the target rate experienced.

A generator that falls behind and then reports only `response_latency` flatters
the server: requests it never managed to send have no latency at all. Judging on
`perceived` prevents that — a step that could not offer the rate fails even if
every request it did send was answered quickly.

The consequence for reading a result: **a step can fail with `response` latency
well inside the SLO.** That is not a server result. Every step prints both
figures at the SLO's percentile, so they can be compared directly:

```
STEP 10 — FAIL — Latency Exceeded

SLO:    28.1K @ p999 ≤ 10ms
Result: 28.3K @ p999=2.10 ms        <- response latency, well inside the SLO
Perceived: p999=27.8 ms             <- what the SLO was judged against
Transitions: slip onset, first SLO breach
Latency: p999 27787us > 10000us SLO
```

`Result` is response latency; `Perceived` is response plus slip, and the fail
reason quotes the perceived figure. If the two are equal, the generator kept up
and the step's verdict is the server's. If `Perceived` is much larger — as here,
27.8 ms against a response latency of 2.10 ms — the step is carrying generator
debt and the verdict is at least partly the generator's. Note that this step
also delivered its target rate (28.3K against 28.1K requested), so nothing in
the `Result` line suggests the generator was behind.

`Transitions: slip onset` marks the first step where slip p99 exceeds 1 ms.
**Read it against the step that prints `first SLO breach`:**

- Slip onset at or after the breach, or never — the breach is the server. The
  result is a server knee.
- Slip onset several steps before the breach — the criterion was already
  carrying milliseconds of generator debt when it failed. Treat the reported
  rate as a floor on the server, not a knee, and see
  [Is the generator the bottleneck?](#is-the-generator-the-bottleneck).

The per-step table is printed by the clean formatter only; the JSON output
carries whole-run `schedule_slip` and `perceived_latency` rather than per-step
figures. For a time series, both are in the Parquet snapshot.

### Confirming failures

`confirm_failures` (default 1) requires a rate to fail twice in a row before the
search acts on it. A retry that passes clears the count and the rate counts as
passing.

It exists because the search is otherwise anchored by its first failing sample:
a failure during the climb fixes the bisection ceiling permanently and nothing
above that rate is retried again. One transient — a writeback burst, a noisy
neighbour, a step-onset herd — then caps the whole run, and the output looks
like a clean convergence rather than an error.

**Raise it when the run has a known source of transients.** Each additional
confirmation costs one sample window per failure and does not bias a real
ceiling upward, since a genuine limit fails every time. At 3, a spurious
termination needs four consecutive transients instead of two.

One symptom worth knowing, because it is what the default does not always
catch: if two consecutive steps fail at the same rate with `response` latency
well inside the SLO and only `perceived` over it, the climb ended on generator
debt and the result is capped below the server's real knee. Re-run at a higher
`confirm_failures` before believing the number.

### Results

The benchmark reports the maximum rate that met the SLO, along with a table of each step:

```
Saturation Search Results:
  Maximum compliant rate: 45,000 req/s (P99.9 = 890µs)

  Step  Target    Achieved    P50     P99    P99.9   Pass
  ────────────────────────────────────────────────────────
     1   1,000      1,000    32µs    45µs     67µs   ✓
     2   1,050      1,050    32µs    46µs     71µs   ✓
    ...
    42  45,000     45,000    41µs   210µs    890µs   ✓
    43  47,250     47,250    48µs   340µs   1.2ms    ✗
    44  49,612     46,100    55µs   410µs   1.5ms    ✗
    45  52,093     44,800    62µs   520µs   1.8ms    ✗
```

See `config/saturation.toml` for a ready-to-use configuration.

## Cluster Mode

Cluster mode supports **Valkey/Redis Cluster** by discovering topology via `CLUSTER SLOTS` at startup and routing keys by hash slot.

### Enabling Cluster Mode

```toml
[target]
endpoints = ["127.0.0.1:7000"]  # One or more seed nodes
protocol = "resp"
cluster = true
```

### How It Works

**Topology Discovery:**
1. Before launching worker threads, the benchmark connects to a seed node over plain TCP and sends `CLUSTER SLOTS`. Discovery does not use TLS, and node addresses in the reply must be IPv4 addresses.
2. The response maps hash slot ranges (0-16383) to primary node endpoints.
3. A 16384-entry slot table is built: `slot -> endpoint`.

The node list is read once. Nodes added to the cluster during a run receive no traffic, and connections to a removed node keep reconnecting to its address.

**Request Routing:**
1. Each connection serves one node and sends keys whose CRC16 hash slot that node owns: by the startup slot table for `uniform` and `zipf` keys, and by the worker's current slot table for `recency` keys.
2. Prefill and append SETs are checked against the worker's current slot table before they are sent.

**MOVED Redirects:**
1. On a main-workload connection, a `MOVED <slot> <host>:<port>` reply is counted in `cluster_redirects` and as a response, not as an error, and its latency is recorded in the GET/SET/DEL and response latency histograms. On an append writer it is counted in `append_set_errors` and its latency in `append_set_latency`.
2. If the named node is one of the startup nodes, the worker's slot table is updated for that slot. Each worker keeps its own slot table, so each one learns the move separately.
3. If the named node was not found at startup, the redirect is logged as a warning (when the address is an IPv4 `ip:port`) and the slot table is not changed.
4. A GET, SET or DEL in the main workload is not retried. A prefill or append SET that gets `MOVED` is queued again and resent: to the new node if the worker's slot table now names it, otherwise to the same node, which answers `MOVED` again. A slot moved to a node not found at startup therefore stops prefill from finishing: the run aborts when prefill stalls (30 s without a newly confirmed key) or at `prefill_timeout`, whichever comes first. It also stops the append batch from committing.

Because `uniform` and `zipf` keys follow the startup slot ownership, after a slot moves those keys keep going to the old node, and every request for them returns `MOVED`. A rising `cluster_redirects` count means part of the reported throughput and latency is redirect replies.

**ASK Redirects:**
ASK redirects (indicating a slot migration in progress) are counted in `cluster_redirects` but do not update the slot table, since the migration is transient. A main-workload request is not retried. A prefill or append SET is queued again and resent to the same node until the migration finishes, after which it gets `MOVED` and follows item 4 above.

### Cluster Mode with Prefill

Prefill works with cluster mode — keys are routed to the correct shard during the prefill phase:

```toml
[target]
endpoints = ["127.0.0.1:7000"]
protocol = "resp"
cluster = true

[workload]
prefill = true
```

See `config/valkey-cluster.toml` for a ready-to-use configuration.

## Routing across nodes

With several nodes (a list of endpoints, cluster mode, or auto discovery),
`workload.routing` sets how a request reaches a node.

```toml
[workload]
rate_limit = 100000
routing = "routed"   # default "pinned"
```

**Pinned** (default): each connection serves one node and draws only keys that
node owns. Each node is loaded as if it were a single-server benchmark: with
`endpoint_allocation = "even"` every node gets the same share of the rate, or
the same number of closed-loop connections, whatever share of the key
distribution it owns. The result is the sum of per-node capacities.

**Routed**: each request draws its key from the whole keyspace and is sent to
the node that owns it, the way a client population loads a cluster. With zipf
keys the node that owns the hottest keys receives more of the rate than the
others.

How a routed run works:

1. Each worker runs a dispatcher. For each token the rate limiter grants, it
   draws a key, routes it, and queues the request for the owning node.
2. A connection to that node takes queued requests when its pipeline has room
   for a batch (`connection.batch_size`, which in routed mode defaults to 1).
   With replies pending and room in its pipeline, it sends a request queued
   for its node as soon as one is, without waiting for the next reply. A
   request queued while a reply is being read waits for that reply to finish.
   With `batch_size` set to `pipeline_depth`, a connection with anything in
   flight has no room for a batch, so it sends nothing until its replies are
   in.

   The default costs generator capacity: at saturation a connection that keeps
   its pipeline full sends about one request per reply instead of a pipeline's
   worth per send. In one loopback measurement (one saturated worker, four
   valkey nodes, one connection per node, five rounds each) batch 1 sent about
   45% fewer requests than `batch_size = pipeline_depth`: 141k against 257k
   req/s at pipeline 8, 394k against 734k at pipeline 32. Below saturation the
   cost has not been resolved: at 50k req/s (6 connections, pipeline 8) batch
   1 used 0.7-1.5 s more client CPU per 25 s run than batch 8 in each of three
   rounds, but batch 1 ran first in every round, so the difference is not
   separated from run order. A routed prefill also sends one SET per send, so
   a prefill limited by the generator takes longer, and a routed saturation
   search limited by the generator finds a lower knee. If a run is short of
   generator capacity, add threads, or set `batch_size = pipeline_depth` and
   accept the queue wait in step 3. A batch of 1 also sizes the send pool for
   one send per request, so a large routed run can reach the send-pool cap
   that the startup warning names.
3. Latency is measured from the moment the dispatcher funded the request. It
   includes the time the request waited for a connection to its node to take
   it: the node answering slowly, and every one of this worker's connections
   to the node waiting on a reply. The waiting part alone is in
   `routed_queue_wait`, recorded when the request is sent (including requests
   a failed send then discards). With few connections per node, that wait is
   mostly the generator's. With `batch_size` at `pipeline_depth`, once this
   worker's connections to the node can carry the rate, a deeper pipeline does
   not bring the wait below about one reply time; more connections per node
   do. Keep `routed_queue_wait` small next to the latency you are measuring,
   or the result describes the generator as much as the server.
4. Requests routed to a node whose connections on a worker are all
   reconnecting stay in the queue until it fills, and count the reconnect time
   as latency.
5. Each node's queue on a worker holds 16 times what that worker's
   connections to the node can have in flight (`connections x
   pipeline_depth`). A request funded while the queue is full is dropped and
   counted in `routed_queue_dropped`, and the run summary reports it as part
   of the offered load. A saturation search counts responses, so drops lower
   the achieved rate and fail the step when it falls below
   `min_throughput_ratio`.
6. Backfill SETs (`backfill_on_miss`) are sent on the connection that saw the
   miss and take their own token from the limiter.

Requirements, checked at startup:

- A rate limit: `workload.rate_limit > 0` or `[workload.saturation_search]`.
  Closed-loop routing is not supported yet.
- Protocol `resp`, `resp3`, `memcache` or `memcache-binary`.
- At least `threads x nodes` connections, so every worker has a connection to
  every node. In cluster and auto discovery modes the nodes are the ones found
  by discovery.
- Not with `connection.endpoint_allocation = "key_share"`, which approximates
  the same per-node load by giving hot nodes more connections.

The node list is read once at startup in both modes.

## Memcached Auto Discovery

`auto_discovery` runs against every node of an ElastiCache for Memcached
cluster, given its configuration endpoint
([AWS documentation](https://docs.aws.amazon.com/AmazonElastiCache/latest/dg/AutoDiscovery.AddingToYourClientLibrary.html)).

```toml
[target]
endpoints = ["10.0.1.25:11211"]  # the configuration endpoint, resolved
protocol = "memcache"            # or "memcache-binary"
auto_discovery = true
```

Before the workers start, cachecannon sends `config get cluster` to the first
endpoint over a separate blocking connection (TLS when `tls = true`), and
replaces `endpoints` with the nodes in the reply. If an endpoint fails, the next
one is tried. If the server answers `ERROR`, the node list is read from the
`AmazonElastiCache:cluster` key, which AWS documents for engines before
memcached 1.4.14. A plain memcached has neither, and the run fails with an
error naming the endpoint.

Keys are routed to the discovered nodes by ketama consistent hashing over their
`ip:port`, as with any list of several endpoints. Discovery runs once: nodes
added or removed during the run are not picked up.

`endpoints` takes IP addresses, so resolve the configuration endpoint's DNS name
first, from inside the VPC (`dig +short <configuration endpoint>`). Each node is
reached at the IP in the reply; a node listed with no IP is resolved from its
hostname.

With `tls = true`, every node connection uses the one `tls_hostname` (or the
node's IP when unset) for SNI and certificate verification. Whether one name
verifies every node depends on the names in the cluster's certificates.

## TLS

TLS encrypts connections between cachecannon and the target server. Enable it with a single flag:

```toml
[target]
endpoints = ["cache.example.com:6380"]
protocol = "resp"
tls = true
```

By default cachecannon verifies the server against the public CA roots (via `webpki-roots`).

### Private CAs and mutual TLS

A server whose certificate chains to a private CA, or that requires a client
certificate, needs the certificate options:

```toml
[target]
endpoints = ["cache.internal:11212"]
protocol = "memcache-binary"
tls = true

# CA certificates that verify the server.
tls_ca_file = "ca.pem"

# Client certificate presented for mutual TLS.
tls_cert_file = "client.crt"
tls_key_file  = "client.key"
```

All three are PEM. `tls_ca_file` **replaces** the public roots rather than
adding to them: when set, only those CAs are trusted. `tls_cert_file` and
`tls_key_file` must be set together, and the key must be unencrypted — rustls
cannot read password-protected keys.

`tls_verify = false` skips server verification entirely. It is for self-signed
certificates in local testing; prefer `tls_ca_file`, which still authenticates
the server. Client certificates work with either setting.

Two rustls requirements are worth checking before debugging a failed
handshake:

- **The server certificate must carry `subjectAltName`.** rustls has no
  fallback to the Common Name, so a CN-only certificate fails with
  `NotValidForName` regardless of the CA configuration; the fix is reissuing it
  with a SAN. Check with
  `openssl x509 -in server.crt -text -noout | grep -A1 "Subject Alternative Name"`.
- **Only TLS 1.2/1.3 with modern cipher suites** — no RSA key exchange, no
  3DES or CBC-SHA1. An older server may have nothing to negotiate.

TLS works with all protocols (RESP, Memcache, Ping) and with memcached auto discovery. Cluster mode does not work with TLS: discovery sends `CLUSTER SLOTS` over plain TCP. TLS adds some latency overhead from the TLS handshake and encryption, so enable it when your server requires it rather than for local benchmarking. On memcache SET, TLS also costs the zero-copy value path: encryption has to copy, so `SendGuard` no longer avoids a copy.

## Memcache Protocol Selection

Cachecannon supports both Memcache protocol variants:

```toml
[target]
protocol = "memcache"         # ASCII text protocol (default)
# protocol = "memcache-binary" # Binary protocol
```

**ASCII protocol** (`memcache`) is the original text-based protocol. It's human-readable, widely supported, and the default for most Memcache deployments.

**Binary protocol** (`memcache-binary`) is a more compact binary encoding. It has slightly lower parsing overhead and is used by some high-performance deployments, but not all servers support it.

Use ASCII unless your server specifically requires or benefits from the binary protocol.

## Output Formats

Cachecannon supports four output formats, selectable in the config or via CLI:

```toml
[admin]
format = "clean"    # Default
```

### Clean (default)

Human-readable table with ANSI colors. Shows per-second samples during the run and a summary at the end. Best for interactive terminal use.

### JSON

Newline-delimited JSON (NDJSON). Each line is a self-contained JSON object with a `type` field (`config`, `sample`, or `result`). Best for piping into other tools, dashboards, or automated analysis.

```toml
[admin]
format = "json"
```

### Verbose

Uses Rust's `tracing` framework to emit structured log output. Shows detailed internal state including per-worker events. Useful for debugging configuration issues or understanding benchmark behavior.

```toml
[admin]
format = "verbose"
```

### Quiet

Single-line summary when the benchmark finishes. No per-second output. Best for scripted use where you only need the final result.

```toml
[admin]
format = "quiet"
```

### Color Control

By default, ANSI colors are auto-detected based on whether stdout is a terminal:

```toml
[admin]
color = "auto"      # Default: colors if stdout is a TTY
# color = "always"  # Force colors (useful for piping to tools that support ANSI)
# color = "never"   # Disable colors (useful for log files)
```

## Request Distribution

The `request_distribution` setting controls how requests are assigned to connections:

```toml
[connection]
request_distribution = "roundrobin"   # Default
```

**Round-robin** distributes requests evenly across all connections. Each connection gets roughly the same number of in-flight requests. This provides balanced load and is the right choice for most benchmarks.

**Greedy** fills one connection's pipeline to `pipeline_depth` before moving to the next. This can achieve slightly higher throughput in some scenarios by maximizing batching, but it creates uneven load across connections.

Use round-robin unless you're specifically testing pipeline batching behavior.

## Tuning

### High-Throughput Configuration

```toml
[general]
# `threads` is deliberately absent: it defaults to the machine's CPU count,
# which is what you want here. Setting it explicitly caps the generator, and
# nothing in the output tells you that you did — see "Is the generator the
# bottleneck?" below.
io_engine = "uring"

[connection]
connections = 64           # see "Sizing connections" below
pipeline_depth = 64        # High pipelining

[workload.keyspace]
length = 16                # Small keys
count = 100000             # Smaller keyspace = more hits

[workload.values]
length = 64                # Small values
```

### Low-Latency Configuration

```toml
[general]
threads = 4                # Deliberate: must match the cpu_list below
cpu_list = "0-3"           # Pin to dedicated cores
io_engine = "uring"

[connection]
connections = 4            # 1 per thread
pipeline_depth = 1         # No pipelining
```

Pinning is the main reason to set `threads` by hand: the count has to match the
`cpu_list`. Everywhere else, leave it at the default.

### Sizing the recv buffer ring

Each worker receives responses through a ring of provided buffers, shared by
every connection on that worker. Both halves are exposed:

```toml
[general]
recv_ring_size = 256       # buffers per worker, power of two
recv_buffer_size = 16384   # bytes per buffer
```

Unset, both take ringline's defaults, which is almost certainly what you want.
Check what a run resolved to with `format = "verbose"`:

```
recv_buffer: 256 x 16384 bytes per worker (4 buffers per response)
```

A response spanning several buffers costs one recv CQE per buffer, and more
than one buffer per response is worth noticing — but it is not, on its own, a
problem. A buffer is held only between a completion and the client draining it,
not for the life of a connection, so a 256-buffer ring serves thousands of
connections without running dry. Measured on a 16-worker generator at 10,000
connections and 20,000 req/s of 56 KiB values — four buffers per response —
every worker reported zero `ENOBUFS` parks.

So reach for these keys to *test* a hypothesis, not to fix a slow run. If you
do change them, confirm the effect with `ringline/pool{op="recv_parked"}` and
client CPU rather than by reading latency, for the reason in the next section.

### Sizing connections

`connections` is a total, split evenly across workers — each worker carries
`connections / threads`. That per-worker number, not the total, is what governs
generator cost: every connection is its own task, and the runtime's per-worker
pools are sized from it.

Connections are not free. Raising the count raises generator CPU whether or not
it raises offered load, so a wide sweep can walk the generator into saturation
while the request rate stays flat. Sweep connections with the generator-bound
check below in the loop, not after it.

## Troubleshooting

### Connection Refused

```
Error: connection refused
```

Ensure the target server is running and listening on the configured endpoint.

### Prefill Stalls or Times Out

If prefill appears stuck:
1. Check that the server is accepting writes — verify with `valkey-cli SET test value` (or `redis-cli`)
2. Ensure the server has enough memory for the full keyspace (`keyspace.count × (key_length + value_length)`)
3. Increase the timeout for very large keyspaces: `prefill_timeout = "600s"`
4. Check the diagnostic output for connection failures or zero bytes received

### Rate Limit Not Achieved

If actual throughput is lower than `rate_limit`:
1. Confirm `threads` is unset, so it picks up the full CPU count
2. Increase `pipeline_depth`, then `connections`
3. Check target server capacity

Take these in order. Reaching for `connections` first adds generator CPU
whether or not it adds throughput, and can convert a shortfall into a
generator-bound measurement — see the next entry.

### Is the generator the bottleneck?

A load generator that cannot keep up does not report an error — it reports
latency. Time spent waiting for cachecannon to read a response that already
arrived is indistinguishable, in the benchmark's own numbers, from time the
server spent producing it.

Check this before attributing any latency to the server under test. With
client-side Rezolus telemetry (see [Correlating with Rezolus](#correlating-with-rezolus)),
compare `tcp_packet_latency` against the latency cachecannon reports:

- `tcp_packet_latency` measures socket-becomes-readable to userspace-reads-it.
- That is pure generator delay. Subtract it from reported latency; what is left
  is the server's share.

If `tcp_packet_latency` is a large fraction of reported latency, the
measurement is generator-bound and the server numbers are not usable. In one
characterisation run, 72% of reported p50 at 10,000 connections was this
reaping delay, while TCP srtt stayed flat — most of the "latency collapse"
under load was the generator, not the server.

Corroborating signals, all from client-side Rezolus:

- **TCP srtt flat while reported latency climbs** — the network is fine, so the
  delay is above it.
- **Generator CPU plateaus** as connections rise, especially below the core
  count. A plateau at roughly one core per worker thread *may* be a thread-count
  ceiling rather than a machine ceiling — but a worker at a full core can also
  be spinning with capacity to spare, so this reading has to be tested rather
  than acted on. See
  [Per-worker CPU, and why 1.00 is ambiguous](#per-worker-cpu-and-why-100-is-ambiguous).
- **Context switches collapse** as connections rise. Worker threads that stop
  sleeping are saturated, not idle.

And one signal that needs no Rezolus at all, because cachecannon reports it
itself (see
[Perceived latency is the criterion](#perceived-latency-is-the-criterion)):

- **`perceived` latency materially above `response` latency.** The difference is
  `schedule_slip`: time the generator was behind its own schedule. Under a rate
  limit this is the cheapest generator check there is, and in a saturation
  search it decides pass and fail.

What to do about it:

1. Leave `threads` unset so it picks up the full CPU count.
2. Re-run the point that looked bad and confirm `tcp_packet_latency` dropped.
3. Only then compare servers.

### Separating server time from reaping delay

`perceived` above `response` catches a generator that is behind on *issuing*
requests. It does not catch one behind on *reading* replies.

**`response_latency` includes reaping delay.** The measurement is
`Instant::elapsed()` sampled after the response is parsed, and parsing happens
in userspace after cachecannon notices the reply. A generator late to read a
reply that already arrived charges the wait to the server.

**`get_ttfb` stops earlier.** ringline logs, for each connection, the time of
the completion batch in which the worker handled each receive (one clock read
per batch), and the client reports the time from the send to the batch that
carried the reply's first byte. Time between that batch and the client parsing
the reply is in `get_latency` but not in `get_ttfb`, so a gap between the two
at the same percentile is time between the worker handling that receive and
the client finishing the reply: the connection task waiting to run, parsing,
or the rest of a reply that spans several receives. Under
`workload.routing = "routed"`, `get_latency` also includes the routed queue
wait, which comes before the send.

`get_ttfb` still includes time between the kernel queuing the data and the
worker handling the receive. For that, use `tcp_packet_latency` from
client-side Rezolus — see
[Is the generator the bottleneck?](#is-the-generator-the-bottleneck).

`get_ttfb` is recorded for GETs only.

### Per-worker CPU, and why 1.00 is ambiguous

Read generator CPU per worker rather than summed — the summed figure and the
machine's load average can both look fine while every worker is pinned. But
**a worker at 1.00 core does not by itself mean the generator is the limit**,
and acting on that reading alone can waste most of a machine.

A ringline worker blocks only when it has nothing runnable
(`backend/uring/event_loop.rs`):

```rust
if self.executor.ready_queue.is_empty() {
    self.driver.ring.submit_and_wait(1)?;      // blocks
} else {
    self.driver.ring.submit_and_get_events()?; // returns immediately
}
```

At high connection counts something is runnable at almost every instant — a
rate-limiter grant landing, a recv completing, a task woken — so the queue
rarely empties, the loop spins, and the worker burns a core whether or not it is
short of capacity. **1.00 means "never idle", which is saturation at low
connection counts and a busy loop at high ones.**

Measured on one rig at 512 connections: 8 workers at 1.01 core each and 24
workers at 1.00 core each delivered the **same** throughput at the same server
CPU. Tripling the workers tripled generator CPU and moved nothing. The same
generator at 64 connections read 0.65-0.72 per worker, where the metric is
informative because the ready queue does empty.

**So treat 1.00 as a prompt to test, not a conclusion.** The discriminating
check is two runs:

1. Raise `threads` — double it, or set it to the machine's core count.
2. Compare achieved throughput, not CPU.

- **Throughput rises** — the generator was the limit and now has headroom.
- **Throughput is flat** — the workers were spinning, not short of capacity. The
  ceiling is elsewhere (the server, the network, or the device), and the extra
  workers are pure cost. Put `threads` back.

Below the connection count where the ready queue stops emptying, the simple
reading still holds: workers well under 1.00 mean the generator is not the
limit.

A corollary for SMT machines, which only applies while the metric is
informative at all: if roughly half the workers sit near 1.00 and half
materially lower under the same offered load, the workers are paired on
hyperthread siblings and the effective core count is half the thread count.
cachecannon does not pin unless `cpu_list` is set, so this normally only happens
with an explicit `cpu_list` — and
consecutive CPU IDs are frequently siblings of each other, so
`cpu_list = "0-23"` can mean twelve cores rather than twenty-four. Check the
host's `thread_siblings_list` before writing one. Where every worker reads 1.00
the clustering check cannot distinguish placements either, so it is moot there
too.

### Server stalls from pinning with no spare cores

A server pinned to exactly as many cores as it runs busy threads has no room
for anything else on those cores, and the tail shows it. Measured against Valkey
9.0.1 with `--io-threads 8` pinned to 8 cores, at 64 connections, pipeline 1 and
100K req/s:

- Valkey's io-threads spin while waiting for work, so all 8 cores stay busy.
- Whenever another task needs one of those cores (kernel softirq, a metrics
  agent, a packet capture), a Valkey thread is preempted for a scheduler slice,
  measured at up to 7 ms.
- The main thread waits on every io-thread each cycle, so one preempted thread
  stalls every client at once. Server-side packet captures show 5-8 ms stalls
  hitting all connections together.

How often this happens depends on the arrival pattern. Requests that arrive on
schedule keep Valkey's main thread busy, which removes the last idle time on
those cores; bursty arrivals leave gaps that absorb the preemption. So an
open-loop client that keeps its schedule (cachecannon 0.0.27 and later) reads a
higher p999 from such a server than one that sends in bursts: 6.3-6.9 ms
against 2.4-3.2 ms from 0.0.26 in the run above, with memtier_benchmark in
between.

With the same 8 threads pinned to 12 cores, the stalls go away and p999 is
0.4-0.7 ms for all three clients. memcached with 8 threads on 8 cores showed no
such stalls.

If a server's tail looks worse than expected, check its thread count against its
pinned cores before blaming the client or the network. Leave spare cores beyond
the server's busy threads, or report the tail as a property of that
configuration.

### High Latency Variance


For consistent measurements:
1. Use `cpu_list` to pin threads to dedicated cores
2. Disable CPU frequency scaling
3. Use `io_engine = "uring"` on Linux 6.0+
4. Increase warmup period

### Memory Usage

Memory usage scales with:
- `connections × pipeline_depth × (key_length + value_length)`
- Buffer pools (configurable via io-driver)

For high connection counts, ensure sufficient memory.
