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
   - **Latency**: All specified percentile thresholds must be met (e.g., p99.9 < 1ms)
   - **Throughput**: Achieved throughput must be at least `min_throughput_ratio` of the target rate (default: 90%). This detects when the server can't keep up regardless of latency.
5. If the SLO passes: record this rate as the current maximum, multiply by `step_multiplier`, continue
6. If the SLO fails: increment the consecutive failure counter
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
threads = 8
io_engine = "uring"

[connection]
connections = 64           # 8 per thread
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
threads = 4
cpu_list = "0-3"           # Pin to dedicated cores
io_engine = "uring"

[connection]
connections = 4            # 1 per thread
pipeline_depth = 1         # No pipelining

[timestamps]
mode = "software"          # Lower measurement overhead
```

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
1. Increase `connections` and `pipeline_depth`
2. Add more `threads`
3. Check target server capacity

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
