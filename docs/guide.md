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
benchmark serves a msgpack snapshot on `/metrics/binary` — the path
`rezolus record` probes — so a single invocation captures all three sources as
labelled recordings inside one `.rez` archive:

```bash
# Terminal 1: one recorder, three endpoints
rezolus record \
    --endpoint http://localhost:9090,source=cachecannon,role=loadgen \
    --endpoint http://server:4241,role=service \
    --endpoint http://client:4241 \
    -o run.rez

# Terminal 2: run the benchmark with its metrics endpoint enabled
./target/release/cachecannon config/valkey.toml
```

Every recording shares one timeline, so benchmark and system metrics line up
without matching timestamps across files by hand. Analysis happens in Rezolus
(`rezolus view run.rez`); `cachecannon view` reads Parquet, so keep
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
                       measured as unspent rate-limiter tokens divided by rate.
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

Cluster mode supports **Valkey/Redis Cluster** by discovering topology via `CLUSTER SLOTS` and routing keys to the correct shard by hash slot.

### Enabling Cluster Mode

```toml
[target]
endpoints = ["127.0.0.1:7000"]  # One or more seed nodes
protocol = "resp"
cluster = true
```

### How It Works

**Topology Discovery:**
1. Before launching worker threads, the benchmark connects to seed nodes and sends `CLUSTER SLOTS`
2. The response maps hash slot ranges (0-16383) to primary node endpoints
3. A 16384-entry slot table is built: `slot -> endpoint`

**Request Routing:**
1. Each key is hashed via CRC16 to determine its hash slot (0-16383)
2. The slot table maps to the responsible endpoint
3. The request is sent to a connection for that endpoint

**MOVED Redirect Handling:**
1. If the server responds with `MOVED <slot> <host>:<port>`, the benchmark extracts the new endpoint
2. If the endpoint is unknown, a new connection is established dynamically
3. The slot table is updated so future requests for that slot route directly to the new node
4. The request is retried on the correct node

**ASK Redirects:**
ASK redirects (indicating a slot migration in progress) are counted in the `cluster_redirects` metric but do not update the slot table, since the migration is transient.

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

TLS works with all protocols (RESP, Memcache, Ping) and with cluster mode. It adds some latency overhead from the TLS handshake and encryption, so enable it when your server requires it rather than for local benchmarking. On memcache SET, TLS also costs the zero-copy value path: encryption has to copy, so `SendGuard` no longer avoids a copy.

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

[timestamps]
mode = "software"          # Lower measurement overhead
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
requests. It does not catch one behind on *reading* replies, because where that
delay lands depends on `[timestamps] mode`, which also decides whether it is
measured at all.

**`mode = "userspace"` (the default) includes reaping delay in
`response_latency`.** The measurement is `Instant::elapsed()` sampled after the
response is parsed, and parsing happens in userspace after cachecannon notices
the reply. A generator late to read a reply that already arrived charges the
wait to the server, and nothing in cachecannon's own output separates the two.
Use
`tcp_packet_latency` from client-side Rezolus — see
[Is the generator the bottleneck?](#is-the-generator-the-bottleneck).

**`mode = "software"` excludes it.** Latency becomes
`kernel_recv_timestamp - userspace_send_timestamp`, so the clock stops when the
data arrived rather than when cachecannon got to it:

```toml
[timestamps]
mode = "software"    # Linux only (SO_TIMESTAMPING)
```

A `software`-mode `response_latency` cannot be inflated by slow reaping, which
makes it the better choice when the question is what the server did. It does not
*measure* the reaping delay; it removes it.

**The two modes are not comparable on latency.** Switching mid-comparison moves
every latency figure by however much reaping delay there was, which looks like a
server or configuration change and is neither. Pick one mode for a whole set of
runs.

**`get_ttfb` cannot be used to difference out reaping delay.** Under `software`
it is computed from the same kernel timestamp as `response_latency`:

```rust
// ringline-memcache / ringline-redis, identical bodies
fn finish_timing(&self, send_ts: u64, start: Instant) -> u64 {   // latency_ns
    if self.use_kernel_ts {
        let recv_ts = self.conn.recv_timestamp();
        if recv_ts > 0 && recv_ts > send_ts { return recv_ts - send_ts; }
    }
    start.elapsed().as_nanos() as u64
}
fn compute_ttfb(&self, send_ts: u64) -> Option<u64> {            // ttfb_ns
    if self.use_kernel_ts {
        let recv_ts = self.conn.recv_timestamp();
        if recv_ts > 0 && recv_ts > send_ts { return Some(recv_ts - send_ts); }
    }
    None
}
```

So `get_ttfb` equals `get_latency` under `software` and has no samples under
`userspace` — the difference is zero or unavailable, never informative. An empty
metriken histogram does not reach the Parquet snapshot, so a missing `get_ttfb`
column means `userspace` mode rather than a broken metric.

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
