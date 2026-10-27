# Benchmark Configuration & Metrics Reference

Complete reference for all configuration options, output formats, dashboard charts, and recorded metrics.

## Configuration Reference

Configuration files use TOML format. All fields below show their default values where applicable.

### General Settings

```toml
[general]
# Test duration (required for bounded tests)
duration = "60s"

# Warmup period before recording metrics
# Allows cache to warm up before measurement
warmup = "10s"

# Number of worker threads (default: CPU count)
threads = 4

# CPU pinning (Linux only)
# Formats: "0-3", "0,2,4,6", "0-3,8-11"
cpu_list = "0-3"

# I/O engine selection
# "auto" - Use io_uring if available, fallback to mio
# "uring" - Force io_uring (Linux 6.0+ only)
# "mio" - Force mio (portable)
io_engine = "auto"

# Print ringline's per-worker event-loop diagnostics to stderr at shutdown
# (`[ringline diag]` iteration mix and `[ringline stall]` stall buckets).
# io_uring only; off by default.
ringline_diag = false

# Provided recv-buffer ring, per worker. Both default to ringline's own values;
# the resolved geometry is printed by `format = "verbose"`.
#
# Buffers in the ring, shared by every connection on the worker. Must be a
# power of two. Default: 256. A buffer is held only between a completion and
# the client draining it, not for the life of a connection, so this serves
# thousands of connections without running dry — measured at 10,000 connections
# and 20,000 req/s of 56 KiB values with zero ENOBUFS parks.
# recv_ring_size = 256

# Bytes per buffer. Default: 16384. A response larger than this spans several
# buffers and costs one recv CQE each.
# recv_buffer_size = 16384

# Seed for the per-connection RNGs that choose keys and commands. Unset draws
# one from OS entropy, so separate processes send different key sequences; the
# seed in use is logged at startup. Set it to repeat a run's key sequence
# exactly.
# seed = 12345
```

### Target Settings

```toml
[target]
# Target server addresses (supports multiple for load balancing)
endpoints = ["127.0.0.1:6379"]
# endpoints = ["server1:6379", "server2:6379", "server3:6379"]

# Protocol selection
# "resp" - Valkey/Redis RESP2 protocol
# "resp3" - Valkey/Redis RESP3 protocol
# "memcache" - Memcache ASCII protocol
# "memcache-binary" - Memcache binary protocol
# "ping" - Simple PING/PONG
protocol = "resp"

# Enable TLS encryption
tls = false

# Explicit SNI hostname (endpoint IP is used when unset)
# tls_hostname = "cache.example.com"

# Verify the server certificate. Default true.
# tls_verify = true

# PEM CA bundle used to verify the server. Replaces the public root store
# rather than adding to it, so only these CAs are trusted.
# tls_ca_file = "ca.pem"

# PEM client certificate chain and its unencrypted private key, for servers
# that require mutual TLS. Both must be set together.
# tls_cert_file = "client.crt"
# tls_key_file = "client.key"

# Enable Valkey/Redis Cluster mode (see guide.md for details)
cluster = false

# ElastiCache for Memcached auto discovery: query the endpoints (a
# configuration endpoint) with `config get cluster` and run against the nodes
# it lists. Requires protocol memcache or memcache-binary; not with cluster.
auto_discovery = false

# AUTH on every new connection, before HELLO 3 and the precheck PING, and
# before CLUSTER SLOTS in cluster mode. RESP protocols only. Without
# auth_user, the one-argument `AUTH <password>` authenticates as the default
# user. Give the password in at most one of auth_password and
# auth_password_file (one trailing newline is ignored); with neither, the
# CACHECANNON_AUTH_PASSWORD environment variable is used if set.
# auth_user = "bench"
# auth_password_file = "/run/secrets/valkey-password"
# auth_password = "..."
```

A password the server rejects fails the precheck with `the server rejected
AUTH`, and the server's error is logged once at warn level. The password is
never logged: `Debug` output of the config shows it as `<redacted>`.

### RESP Module Settings

`[resp]` applies to `protocol = "resp"` and `protocol = "resp3"`. It selects
the command names the RESP path sends GETs and SETs under.

```toml
[resp]
# "none" - GET key / SET key value (default)
# "largeobj" - BLOB.TCP_GET key / BLOB.TCP_SET key value, the TCP commands of
#              the Valkey large-object module (valkey-io/valkey-large-object;
#              config/largeobj.toml runs the same module over RDMA)
module = "none"
```

With `module = "largeobj"`, every GET and SET the workload sends uses the
module's command, prefill included. The module stores values as its own type.
A plain `GET` on one of its keys answers `WRONGTYPE`; a plain `SET` replaces the
key with a string, which `BLOB.TCP_GET` then answers with `WRONGTYPE`.
`BLOB.TCP_SET` also answers `WRONGTYPE` on a key that holds a string, so a
server filled by an earlier run without the module must be flushed first. A server
without the module answers the `BLOB.TCP_*` commands with an unknown-command
error. The other commands the RESP path sends are unchanged: `HELLO 3` under
`resp3`, the precheck `PING`, and `DEL` for `[workload.commands] delete`, which
removes the module's keys like any other. In cluster mode both commands
declare the key as their first argument, so slot ownership and MOVED redirects
apply to them as to GET and SET.

Config load refuses `module = "largeobj"` with a protocol other than `resp` or
`resp3`; `protocol = "dma"` with `[dma] module = "largeobj"` sends the module's
RDMA commands. It also refuses `[workload.append]`. The append writers send
plain `SET`, which replaces the module's keys with strings, and later
`BLOB.TCP_GET`s of those keys answer `WRONGTYPE`. It also refuses
`[workload.kv_replay]`, a combination not run against the module. The RESP
workload sends no TTL, so every SET is the three-argument form `BLOB.TCP_SET`
accepts.

### Connection Settings

```toml
[connection]
# Total connections across all threads
# Distributed evenly: connections / threads per thread
connections = 16

# Maximum in-flight requests per connection (pipelining)
# Higher values = higher throughput, slightly higher latency
# Typical values: 1 (no pipelining), 32 (high throughput)
pipeline_depth = 32

# Most requests coalesced into one send, and the free pipeline room a
# connection needs before it takes more. Clamped to [1, pipeline_depth].
# Defaults to pipeline_depth, or to 1 with workload.routing = "routed", so a
# routed connection with replies pending sends a newly queued request
# whenever its pipeline has room. At saturation batch 1 sends about one
# request per reply, which costs generator capacity; see the guide's routed
# section.
# batch_size = 32

# Connection establishment timeout. Bounds each connect attempt (including
# reconnects mid-run) and doubles as the precheck deadline. "0s" = unbounded.
connect_timeout = "5s"

# Individual request timeout. A pipelined connection's replies are ordered, so
# when the oldest in-flight request exceeds this the connection is torn down:
# every request in flight on it is counted as a timeout (and as an error),
# none of them produce a latency sample, and the connection reconnects.
# Timeouts are reported as `request_timeouts` / `disconnects_timeout` and on
# the results throughput line. "0s" = unbounded (a stalled request would then
# be reported as tail latency, however long it took).
request_timeout = "1s"

# Request distribution strategy
# "roundrobin" - Distribute requests evenly across connections
# "greedy" - Fill one connection's pipeline before moving to next
request_distribution = "roundrobin"

# How connections are assigned to endpoints (several endpoints only)
# "even"      - connection i serves endpoint i % endpoints (default)
# "key_share" - each endpoint gets connections in proportion to the zipf
#               probability its keys carry: at least one if it owns keys,
#               none if it owns none. Requires keyspace.distribution = "zipf"
#               and connections >= endpoints, counting the endpoints listed in
#               [target] (in cluster and auto discovery modes, the seed
#               endpoints, not the nodes discovered later).
endpoint_allocation = "even"
```

With `key_share`, each endpoint receives its zipf share of requests only when
every connection gets the same share of rate-limit tokens. That holds in a
rate-limited run in which no connection is held back by its pipeline:
tokens go first come, first served among a worker's waiting connections, so a
connection whose pipeline is full takes fewer. With `protocol = "dma"` each
connection has one transfer in flight, so at MiB values the hot endpoint's
connections spend most of their time waiting on a transfer and it receives less
than its share. In a closed-loop run an endpoint's share also depends on its
latency. Check the shares a run achieved from per-instance server counters, for
example `cmdstat_get` and `cmdstat_set` calls from `INFO commandstats` on each
instance, divided by their total, against the `zipf_share` column that
`cargo run --release --example zipfbench -- <keys> <endpoints> <theta> 1
<first endpoint addr> <connections>` prints per endpoint. zipfbench builds
its endpoints as consecutive ports on the first address and renders 16-byte hex
keys over the ketama ring, so its shares match a run only with that layout: the
same endpoint addresses, `keyspace.length = 16`, hex keys and no cluster mode.

With `even`, an endpoint that owns no keys still gets connections. That happens
with fewer keys than endpoints and can happen with more: with 128 keys over 48
endpoints at zipfbench's default base address 172.31.0.10:6379, ketama leaves 3
endpoints with none; the count depends on the addresses. In RESP and memcache
each of those connections claims a rate-limit token batch it cannot spend,
which lowers the achieved rate; DMA claims a token only after it has a key.

> **Note:** `pool_size` is accepted as a legacy alias for `connections`. If both are specified, `pool_size` takes precedence. Prefer `connections` in new configurations.

### Workload Settings

```toml
[workload]
# Global rate limit (requests/second, optional)
# Omit or set to 0 for unlimited (open-loop)
rate_limit = 100000

# Pre-populate the cache before measurement
# See guide.md "Prefill" section for details
prefill = false

# Timeout for prefill phase (default: 300s, 0 to disable)
prefill_timeout = "300s"

# Prefill pace (requests/second, optional)
# Omit or set to 0 for an unpaced prefill
# prefill_rate_limit = 10000

# Automatically SET on GET miss (cache-aside pattern)
# See guide.md "Backfill on Miss" section for details
backfill_on_miss = false

# How a request reaches a node when there are several. See guide.md "Routing
# across nodes".
# "pinned" - each connection serves one node and draws only that node's keys
#            (default)
# "routed" - each request draws its key from the whole keyspace and goes to the
#            node that owns it. Requires rate_limit > 0 or
#            [workload.saturation_search], a RESP or memcache protocol, and
#            connections >= threads x nodes (the nodes found by discovery in
#            cluster and auto discovery modes). Not with
#            connection.endpoint_allocation = "key_share".
routing = "pinned"

# Read every GET hit's value: CRC-32c over each value as the client returns it.
# Off by default. See "Reading values" below.
read_values = false

[workload.keyspace]
# Key size in bytes
length = 16

# Number of unique keys in the keyspace
# Larger values = more cache misses, more realistic
count = 1000000

# Key selection distribution
# "uniform" - All keys equally likely
# "zipf" - Zipfian distribution (hot keys accessed more often). With several
#          endpoints each connection samples zipf restricted to its endpoint's
#          keys, using the key-to-endpoint partition computed at startup; like
#          "uniform", it does not follow slot-table changes during the run.
# "recency" - Newest keys are hot, in units of append batches
#             (requires [workload.append]; see guide.md "Append Stream")
distribution = "uniform"

# zipf only: exponent, P(key id k) proportional to (k + 1)^-zipf_theta.
# In (0, 10]; default 0.99.
zipf_theta = 0.99

# recency only: how many of the newest batches form the hot window (default 8)
hot_generations = 8
# recency only: weight of each older hot batch relative to the next newer one,
# in (0, 1]; 1.0 is flat across the window (default 0.5)
hot_decay = 0.5
# recency only: fraction of reads that land in the hot window; the rest go
# uniformly to the body below it (default 0.8)
hot_fraction = 0.8

# Append stream: batches of new keys written on a cadence by dedicated writer
# connections. Grows the keyspace over the run. Requires distribution = "recency".
# [workload.append]
# batch = 100000          # keys per batch
# every = "60s"           # interval between batch starts, from the start of warmup
# pace = "burst"          # "burst" (as fast as the writers allow) or "spread"
# connections = 1         # writer connections; >= the endpoint count when there
#                          # are several (in cluster and auto discovery modes,
#                          # the nodes found by discovery)
# pipeline_depth = 32     # in-flight SETs per writer connection

[workload.commands]
# Command ratio as weights (will be normalized)
get = 80  # 80% GETs
set = 20  # 20% SETs

[workload.values]
# Value size for SET commands (bytes)
length = 64
```

#### Reading values

By default the RESP client receives GET replies with ringline-redis
`recv_meta`, which parses the reply header and drains the value without
assembling it. Client CPU per request is then a lower bound on what a client
that uses the value spends. `read_values = true` receives RESP replies with
`recv` instead, which waits until the whole reply is in ringline's
per-connection accumulator and returns the value as a slice of it, and then
computes a CRC-32c over that slice. No copy is made beyond what the table
lists.

| Protocol and backend | Option off | Option on |
|----------------------|------------|-----------|
| resp, resp3 on io_uring | value drained from the kernel-filled provided buffers; no userspace copy | each reply copied from the provided buffers into the accumulator (one userspace copy), then CRC-32c |
| resp, resp3 on mio | each 8 KiB read copied into the accumulator and the value drained from it | same copy; the option adds the CRC-32c and holds the whole reply until it is parsed |
| memcache, memcache-binary (both backends) | `recv` already assembles each reply in the accumulator | adds the CRC-32c over the returned slice |

Each CRC is folded into a per-connection value passed to `black_box`, so the
compiler cannot remove it. `values_read_bytes` counts the bytes checksummed.
`protocol = "ping"` or `protocol = "dma"` with `read_values = true` is
rejected at config load. For DMA, `[dma] checksum = true` checks the CRC-32c
the server returns with each GET.

Differences from the default path:

- The CRC runs after the reply's latency is recorded. Its cost appears in the
  latency of later replies on the same worker.
- Client memory. `recv` holds each whole reply until it is parsed, so peak
  accumulator memory is up to connections × `pipeline_depth` × value size.
  `recv_meta` holds about one read burst per connection.
- An unexpected reply type to a RESP GET is a per-request error and the
  connection stays open. With the option off it closes the connection, which
  is counted in `disconnects_recv_error`.
- A RESP reply larger than ringline's `recv_accumulator_max` (default 1 GiB)
  fails only with the option on: the runtime closes the connection when the
  accumulator would exceed the limit, the worker counts it in
  `disconnects_eof` and reconnects, and the next such reply does the same.
  `recv_meta` drains the value as it arrives and is not limited.

### KV-Cache Replay Settings

`[workload.kv_replay]` replaces the synthetic keyspace with a kvtrace request
trace. `docs/guide.md` "KV-Cache Prefix Replay" describes what a trace request
does and how tokens are counted.

```toml
[workload.kv_replay]
# kvtrace request trace (Parquet), in timestamp order (kvtrace enforces it).
# Read during the run, at most 16,384 requests ahead of the replay.
trace = "requests.parquet"

# Trace time is divided by this: 10 replays ten times faster than recorded.
# 0 replays closed loop: each connection starts its next trace request as
# soon as the previous one finished. Default 1.
speedup = 1.0

# Model geometry: exactly one of model_config and kv_bytes_per_token.
# A Hugging Face config.json; KV bytes per token, MLA and head counts come
# from it.
model_config = "config.json"
# KV cache dtype, overriding the config's (model_config only).
# kv_dtype = "fp8"
# KV bytes per token over all layers and ranks, instead of model_config.
# kv_bytes_per_token = 131072
# With kv_bytes_per_token: multi-head latent attention, one object per
# chunk at the full latent size. Default false.
# mla = false

# Tensor-parallel degree. Only 1 is accepted: tensor parallelism needs one
# connection per rank, which kv_replay does not open yet (cachecannon#214).
# Default 1.
tp = 1

# Which objects a trace request SETs after its lookup:
# "set-from-miss"    - every object from the end of the lookup's prefix hit
#                      to the last full chunk (default)
# "insert-if-absent" - every object whose EXISTS returned 0
write_rule = "set-from-miss"

# Object key: key_prefix followed by the object id.
# "hex"     - 16 lowercase hex digits, zero-padded (default)
# "decimal" - decimal, no padding
key_prefix = "kv:"
key_format = "hex"

# One CSV row per completed trace request (optional).
# requests_out = "kv-requests.csv"
```

The object id is kvtrace's `chunk << 16 | slot << 12 | rank`, with slot 0.
A SET's value is the object's size in bytes, taken from the shared value pool;
an object larger than the pool (1 GiB) is rejected at startup.

Related settings keep their meaning: `connection.connections` bounds the trace
requests in flight (one per connection), `connection.pipeline_depth` bounds the
GETs or SETs one trace request has in flight, and `connection.request_timeout`
applies to every EXISTS, GET and SET. `general.duration` is an upper bound: the
run ends when every trace request has finished. Trace requests that finish
during `general.warmup` are not recorded.

Rejected at config load: protocols other than `resp` and `resp3`, more than one
endpoint, `cluster = true`, `connection.pipeline_depth = 0`, `tp` other than 1, `rate_limit`, `[workload.saturation_search]`,
`prefill`, `backfill_on_miss`, `read_values` and `[workload.append]`. Rejected
when the run starts: an empty trace, a trace file kvtrace cannot open, a
model with recurrent state (a hybrid linear-attention model), and a trace
whose largest request's lookup needs more send-pool slots than ringline's
per-worker maximum of 65,535. A read error
(a corrupt batch, a row out of timestamp order) found while the reader fills
its first 16,384 requests ends the run after precheck, before any trace
request is sent; a corrupt batch is found when the reader reaches its first
row. One found later stops the replay at that row; the requests already read
finish, the results print, and the run exits with the error.

#### KV replay results

The config summary describes the replay instead of the synthetic keyspace,
which a KV replay ignores. The JSON `config` line has no `keyspace`,
`key_size` or `value_size`, and gains a `kv_replay` object with `trace`,
`speedup` (0 for closed loop), `model_config`, `kv_dtype` and
`kv_bytes_per_token` (whichever are set), `mla` and `write_rule`. The clean
and verbose formats print the same as one workload line.

The JSON `result` line gains a `kv_replay` object:

| Field | Meaning |
|-------|---------|
| `timing` | `"open-loop"` or `"closed-loop"` |
| `speedup` | Trace time divisor; 0 in closed loop |
| `write_rule` | `"set-from-miss"` or `"insert-if-absent"` |
| `trace_requests` | Requests in the trace |
| `requests` | Trace requests replayed to completion during recording |
| `requests_failed` | Trace requests abandoned on a connection error, send failure or server error to the lookup; in no token count. A request whose connection closed before the server answered any of its operations (a server closing idle connections) runs again on the next idle connection instead; `docs/guide.md` "What is not modelled" gives the limits |
| `input_tokens` | Prompt tokens of the replayed requests |
| `cached_tokens` | Prompt tokens served from the cache |
| `new_tokens` | `input_tokens - cached_tokens` |
| `cached_fraction` | `cached_tokens / input_tokens` |
| `full_hits` | Requests whose loaded prefix covered the whole prompt |
| `misses_at_chunk_0` | Requests whose lookup found the first chunk missing |
| `no_full_chunk` | Requests shorter than one chunk; they issue no operations |
| `get_nil` | GETs that returned nil after their EXISTS returned 1 |
| `exists` | EXISTS operations |
| `lookup` | Per request: first EXISTS sent to last EXISTS reply |
| `load` | Per request with GETs: first GET sent to last GET reply |
| `store` | Per request with SETs: first SET sent to last SET reply |
| `load_per_1k_cached_tokens` | Per request with GETs and cached tokens: `load × 1000 / cached_tokens`. Its `count` can be below `load`'s: a request whose first GET returned nil has a load time and no cached tokens |
| `schedule_lag` | Open loop only, per request: start minus the time the trace scheduled it |

The five distributions are objects with `count` (requests sampled) and
`p50_us` through `max_us`, like `get` and `set`. A request is attributed to the
phase it finished in.

The EXISTS, GETs and SETs are also counted in `requests`, `responses`,
`bytes_tx` and `bytes_rx`, and the GETs and SETs in `get`, `set`, `hits` and
`misses`. EXISTS have no per-operation latency: the lookup is sent as one
pipeline whose replies arrive together.

The clean and verbose formats print the request and token totals,
`full_hits`, `misses_at_chunk_0`, `get_nil` and the distributions; `exists`,
`no_full_chunk` and each distribution's `count` are in the JSON only. A trace
that finishes before recording starts (during `general.warmup`) records
nothing, and the run exits with an error after its results. In those results
the `kv_replay` totals are 0, and the run totals (`requests`, `responses`,
GET and SET counts and latencies) count the warmup traffic.

`requests_out` columns, one row per trace request that completed, in completion
order:

| Column | Meaning |
|--------|---------|
| `req_id` | From the trace |
| `timestamp_ns` | Trace timestamp |
| `lag_ns` | Open loop: start minus scheduled time. Empty in closed loop |
| `input_tokens` | From the trace |
| `chunks` | Full chunks in the trace request |
| `lookup_hit_chunks` | Chunks of the lookup's prefix hit |
| `hit_chunks` | Chunks loaded: the lookup's prefix, cut at the first GET that returned nil or an error |
| `cached_tokens`, `new_tokens` | As in the result |
| `gets`, `get_nil`, `sets` | Operations the request sent |
| `lookup_ns`, `load_ns`, `store_ns` | As in the result; 0 when there was no such step |
| `recorded` | 1 if the request finished during recording, 0 during warmup, read from the phase when it finished. A request finishing at the warmup-to-recording transition can be on either side of the JSON totals |

### Object Trace Replay Settings

`[workload.trace_replay]` replaces the synthetic keyspace with a cachesim
object-level trace: the `cachetrace` Parquet format from pelikan-io/cachesim,
one row per cache request with `obj_id`, `obj_size` and an optional `op`
(libCacheSim's `req_op_e` codes). `cachesim convert` makes one from a
libCacheSim binary trace or a pelikan-io/cache-trace CSV, and `cachesim kv
expand` from a KV-cache request trace.

```toml
[workload]
# Rows per second; unset replays closed loop.
rate_limit = 5000
# A GET that misses is followed by a SET of the row's obj_size.
backfill_on_miss = true

[workload.trace_replay]
trace = "trace.parquet"
# Prepended to every key. Default "".
key_prefix = "obj:"
# How obj_id is written after the prefix: "hex" (16 digits, default) or
# "decimal".
key_format = "hex"
```

Each row sends one command, in file order:

- reads (GET, GETS, READ, NOP) and rows with no `op` send `GET`;
- `DELETE` sends `DEL`;
- every write (SET, ADD, CAS, REPLACE, APPEND, PREPEND, INCR, DECR, WRITE,
  UPDATE) sends a `SET` of `obj_size` bytes.

`[resp] module = "largeobj"` sends `BLOB.TCP_GET` and `BLOB.TCP_SET` in
place of `GET` and `SET`. A row with an op code of 15 or more, or a SET larger
than the 1 GiB value pool, is skipped and counted in `trace_rows_skipped`.

Rows are paced by `workload.rate_limit` (a backfill SET takes a token too),
or run closed loop. The trace's timestamps are not used. Rows are first sent
as warmup starts when there is a warmup, otherwise as recording starts. The
run ends when every row has been answered or counted failed, at a read error
in the trace, or at `duration`.

Rows go to whichever connection has room for them, not to a connection chosen
by key, so a hot key's requests are in flight on several connections at once.
Rows can then reach the server out of file order: rows in flight on one
worker's connections at once (up to its connections x `pipeline_depth`), and
rows claimed by different workers (each claims 64 at a time), can arrive in
either order. A worker
whose connections stall or reconnect keeps its claimed rows while other
workers continue, so how far apart in the file two reordered rows can be is
not bounded. For an order the server must see (a fill, then deletes, then
reads), use one trace file per phase and one run each.

A row in flight on a connection that fails is counted in `trace_rows_failed`
and not retried, because the server may have applied it; the rows that
worker had claimed and not sent go to a shared queue that any worker sends
from first. A row that could not be sent is put back at the front of the
worker's claimed rows and sent again. When the send pool refused the send
outright (ringline's `InvalidInput`: a buffer wider than the whole pool, or a
TLS record limit), the failure counts against the row, and after three such
failures the row is counted in `trace_rows_failed`; a send pool that is only
full for now does not count. On io_uring ringline also refuses a batch
carrying a zero-copy value whose copied bytes exceed one 16,448-byte send-pool
slot, so the replay flushes a batch before it would. A backfill SET that could
not be sent is dropped. Rows in flight when the run stops are in neither the
answered nor the failed count.

The trace must have `obj_id` (UInt64) and `obj_size` (UInt32) columns, an
`op` column if any of UInt8, and `obj_size` statistics in its footer, which
size the send buffers. A file without them, with another type for one of
these columns (pyarrow's default Int64, for example), or with no rows is
refused when the run starts; a trace whose every row is skipped fails the run.
A read error later in the file stops the replay at that row and fails the run
after printing results. Config load refuses `[workload.trace_replay]` with a
protocol other than `resp` or `resp3`, with more than one endpoint or cluster
mode, and with `prefill`, `read_values`, `[workload.append]`,
`[workload.saturation_search]`, `[workload.kv_replay]` or
`routing = "routed"`.

#### Trace replay results

The clean and verbose outputs and the JSON result line (`trace_replay`) report
the rows answered, failed and skipped, the rows answered by the command they
were sent as (GET, SET, DEL), how often a connection had room to send while
the trace reader had no row ready (`read_waits`; the run logs a warning when
it is above 0, since those waits lowered the achieved rate), and whether the
whole trace was replayed (`finished`, false after a read error). Row counts
are for the recording phase, except `rows_skipped`, which counts every skipped
row the reader has reached, including rows it read ahead of the replay. The
GETs, SETs and DELs are also counted in the request, response and per-command
latency metrics.

### Saturation Search Settings

See the [Saturation Search](guide.md#saturation-search) section in the guide for how this works.

```toml
[workload.saturation_search]
# Latency SLO thresholds — at least one required
# All specified thresholds must be met for the SLO to pass
# Available percentiles: p50, p99, p999
slo = { p999 = "1ms" }

# Starting request rate (req/s)
start_rate = 1000

# Increase rate by this factor each step (1.05 = 5% increase)
step_multiplier = 1.05

# Duration to sample at each rate level
sample_window = "5s"

# Drain in-flight requests after a rate change before sampling
drain_window = "500ms"

# Stop after this many consecutive SLO violations
stop_after_failures = 3

# Maximum rate to try (absolute ceiling)
max_rate = 100000000

# Minimum ratio of achieved/target throughput (0.0-1.0)
# Fails the step if achieved throughput drops below this fraction
min_throughput_ratio = 0.9

# Extra consecutive measurements required to confirm an SLO failure.
# With 1, a rate must fail twice in a row to count as failed; a retry that
# passes clears the count. Guards against one transient capping the whole run.
# Costs one extra sample window per failure. 0 = act on the first failure.
confirm_failures = 1

# Minimum fractional latency improvement expected when the search halves the
# rate, before it concludes the target's latency floor is above the SLO.
# Only applies while no rate has passed yet. 0 disables the check.
floor_improvement_ratio = 0.10

# Relative interval width at which the bisection stops (0.0-1.0)
bisect_tolerance = 0.05

# Hard cap on the number of bisection probes
max_bisect_steps = 8
```

The latency half of the SLO is evaluated against **perceived** latency
(`response_latency + schedule_slip`), not response latency alone. See
[Perceived latency is the criterion](guide.md#perceived-latency-is-the-criterion)
in the guide for how to read a step that fails with response latency inside the
SLO.

### Timestamp Settings

```toml
[timestamps]
enabled = true
mode = "userspace"
```

`enabled` is not read. `mode` accepts only `"userspace"`; `"software"` (kernel
SO_TIMESTAMPING) is rejected at config load.

`response_latency` is sampled after the response is parsed, so it includes time
cachecannon spent getting to a reply that had already arrived. `get_ttfb` stops
when the worker handled the receive that carried the reply's first byte. See
[Separating server time from reaping delay](guide.md#separating-server-time-from-reaping-delay)
in the guide.

### Output Settings

```toml
[admin]
# Metrics endpoint (optional). Serves three paths:
#   /metrics         Prometheus exposition text
#   /metrics/binary  msgpack snapshot, for `rezolus record` to `.rez`/parquet
#   /metrics/stream  dendro replication stream, for `rezolus record` to `.dendro`
listen = "127.0.0.1:9090"

# Write metrics to Parquet file (optional)
parquet = "results.parquet"

# Parquet snapshot interval
parquet_interval = "1s"

# Output format: clean, json, verbose, quiet
format = "clean"

# Color mode: auto, always, never
color = "auto"
```

### Metrics Endpoint

With `[admin] listen` set, the benchmark serves its live metrics on three paths:

| Path | Content type | Consumer |
|---|---|---|
| `/metrics` (and `/`) | `text/plain; version=0.0.4` | Prometheus scrapers |
| `/metrics/binary` | `application/msgpack` | `rezolus record` to `.rez` or parquet |
| `/metrics/stream` | `application/vnd.metriken.replication.v1+dendro` | `rezolus record` to `.dendro` |

All three carry the same metrics, including the latency histograms. The msgpack
path is the one [Rezolus](https://github.com/brayniac/rezolus) probes for a
non-Prometheus source, so a run can be recorded alongside system telemetry into
a single `.rez` archive — see
[Correlating with Rezolus](guide.md#correlating-with-rezolus). Any other path
returns 404.

`/metrics/stream` is metriken-exposition's stream route. A request names its
frame interval and layout, `/metrics/stream?interval=500ms&layout=long`
(defaults `1s` and `wide`); a zero or malformed interval or an unknown layout
is refused with 400. An interval below the 10 ms update floor is served at the
floor, and `x-metriken-frame-interval` names the interval served. The response
has no `Content-Length`: it is dendro's preamble and a handshake
(`source=cachecannon`; the version and the KPI template in its metadata, see
[KPI template](#kpi-template)), then one frame per
interval, aligned to the Unix epoch, until the client disconnects. The
`x-metriken-frame-interval`, `x-metriken-update-floor` (10 ms) and
`x-metriken-layout` headers describe it. Every metric is in one group,
`cachecannon/main`, stamped when it is read, with no acquisition window;
histograms are sent as histograms, not as summaries.

### KPI Template

cachecannon's KPI template is `templates/cachecannon.json`, a Rezolus
`ServiceExtension`: the throughput, latency, error and cache-hit charts the
Rezolus viewer shows as cachecannon's section. It is sent as the
`service_queries` key of the `/metrics/stream` handshake's metadata, which
`rezolus record` keeps in the recording's metadata, and written under the same
key into the `[admin] parquet` file's metadata. The viewer renders the template
a `.dendro` recording carries, so a template change reaches the viewer with a
cachecannon release.

Every query selects `source="cachecannon"`. Where that label is missing, the
section renders with empty charts:

- The `[admin] parquet` file on its own: its columns carry no `source` label
  (the source is in the file metadata only), so the charts are empty until
  rezolus attaches a single-source parquet's source to its series as a label.
- `rezolus recording combine … -o out.parquet` drops the template in rezolus
  up to 6.0.0-alpha.62. `rezolus recording combine … -o out.rez` keeps it,
  and its charts have data.
- A recording relabelled with another source, such as
  `rezolus record --endpoint http://host:9090,source=cc-a`: the carried
  template still selects `source="cachecannon"`, so its charts are empty.
  Keep the source `cachecannon`, and tell two runs apart with another label
  (`--label arm=a`).

Rezolus's built-in copy of the template renders instead of the carried one:

- for a recording that carries none: made by an older cachecannon, scraped
  from `/metrics` (which carries no metadata), or recorded by rezolus 5.x
  from `/metrics/binary`;
- when the carried JSON does not parse;
- for a parquet combined by a rezolus that drops the template (above);
- with `rezolus view --registry-templates`, which prefers the built-in copy.

Tests in `src/admin/template.rs` check that each query selects
`source="cachecannon"` and only metrics cachecannon registers, that each KPI's
`type` is one the viewer charts and its `role` a lowercase word, and that every
`unit_system` is one the viewer knows.

## Output Formats

### Clean (Default)

Human-readable table with ANSI colors:

```
Configuration:
  Protocol: resp
  Endpoints: 127.0.0.1:6379
  Threads: 4
  Connections: 16
  Pipeline depth: 32
  Keyspace: 1000000 keys (uniform)
  Value size: 64 bytes
  Duration: 60s (warmup: 10s)

Running benchmark...

  Time   Req/s    Err/s   Hit%     P50     P90     P99   P99.9  P99.99     Max
─────────────────────────────────────────────────────────────────────────────────
  1.0s   523847       0  79.2%    48µs    89µs   156µs   312µs   891µs  1.24ms
  2.0s   531294       0  79.4%    47µs    87µs   152µs   298µs   756µs  1.12ms
  ...

Results:
  Duration: 60.0s
  Requests: 31,847,291
  Throughput: 530,788 req/s
  Cache hit rate: 79.3%
  Latency (GET): P50=47µs P99=154µs P99.9=301µs
  Latency (SET): P50=52µs P99=167µs P99.9=334µs
```

### JSON

Newline-delimited JSON (NDJSON) for programmatic consumption:

```json
{"type":"config","target":"127.0.0.1:6379","protocol":"Resp","tls":false,"threads":4,"conns":64,"pipeline":32,"keyspace":1000000,...}
{"type":"sample","time":1.0,"requests":523847,"errors":0,"hit_rate":0.792,"p50":48000,...}
{"type":"sample","time":2.0,"requests":531294,"errors":0,"hit_rate":0.794,"p50":47000,...}
{"type":"result","duration":60.0,"total_requests":31847291,"throughput":530788,...}
```

### Verbose

Tracing-style output with debug information:

```
2024-01-15T10:30:00.000Z INFO  benchmark: Starting benchmark
2024-01-15T10:30:00.001Z DEBUG benchmark: Worker 0 connecting to 127.0.0.1:6379
...
```

### Quiet

Single-line summary only:

```
530788 req/s, 0 errors, 79.3% hit rate, P99=154µs
```

## Protocol Details

### RESP (Valkey/Redis)

Supported commands:
- `GET key` — Retrieve value
- `SET key value` — Store value
- `DEL key` — Delete, for a nonzero `delete` ratio
- `PING` — Connection health check
- `HELLO 3` — Under `protocol = "resp3"`, once per connection
- `AUTH [user] password` — When a password is configured, once per connection, first

`[resp] module = "largeobj"` sends `BLOB.TCP_GET key` and
`BLOB.TCP_SET key value` in place of `GET` and `SET`; see
[RESP Module Settings](#resp-module-settings).

The benchmark generates random keys based on keyspace settings and random values based on value length.

### Memcache

Supported commands:
- `get key` — Retrieve value (supports multi-get)
- `set key flags exptime bytes\r\nvalue` — Store value

ASCII protocol by default. Use `protocol = "memcache-binary"` for binary protocol.

### Ping

Sends `PING`, expects `PONG`. Useful for measuring baseline connection latency without cache operations.

## Results Viewer

The `view` subcommand launches an interactive web dashboard. All time-series charts share a synchronized time axis and support zooming, panning, and tooltips.

### Overview

A single-page summary with six key metrics:

| Chart | What it shows | Unit |
|-------|---------------|------|
| **Throughput** | Responses per second (10s irate) | req/s |
| **Hit Rate** | `hits / (hits + misses)` as a percentage | % |
| **Latency** | Percentiles (p50, p90, p99, p99.9, p99.99) on a log scale | time |
| **Error Rate** | `errors / requests_sent` as a percentage | % |
| **TX Bandwidth** | Bytes sent, converted to bits/sec | bit/s |
| **RX Bandwidth** | Bytes received, converted to bits/sec | bit/s |

### Throughput

Detailed throughput breakdown:

| Chart | What it shows | Unit |
|-------|---------------|------|
| **Responses/sec** | Total response rate | req/s |
| **Error Rate** | Error percentage over time | % |
| **TX Bytes/sec** | Network send throughput | bytes/s |
| **RX Bytes/sec** | Network receive throughput | bytes/s |
| **GET/sec** | GET operation rate | req/s |
| **SET/sec** | SET operation rate | req/s |
| **DELETE/sec** | DELETE operation rate | req/s |

### Latency

Per-operation latency percentiles, all on log scale:

| Chart | What it shows |
|-------|---------------|
| **Response** | All operations combined (p50, p90, p99, p99.9, p99.99) |
| **GET** | GET latency percentiles |
| **SET** | SET latency percentiles |
| **DELETE** | DELETE latency percentiles |

### Cache

Cache behavior over time:

| Chart | What it shows | Unit |
|-------|---------------|------|
| **Hit Rate** | Cache hits per second | req/s |
| **Miss Rate** | Cache misses per second | req/s |
| **GET/sec** | GET operation rate | req/s |
| **SET/sec** | SET operation rate | req/s |
| **DELETE/sec** | DELETE operation rate | req/s |

### Connections

Connection pool health:

| Chart | What it shows | Unit |
|-------|---------------|------|
| **Active Connections** | Current live connection count (gauge) | count |
| **Connection Failures/sec** | Rate of failed connection attempts | req/s |
| **EOF** | Disconnects from server closing the connection | req/s |
| **Recv Error** | Disconnects from receive errors | req/s |
| **Send Error** | Disconnects from send errors | req/s |
| **Closed Event** | Disconnects from close events | req/s |
| **Error Event** | Disconnects from error events | req/s |
| **Connect Failed** | Disconnects from failed connect attempts | req/s |

### Server Metrics / Client Metrics

When you provide `--server` or `--client` Rezolus parquet files, these sections display system-level telemetry (CPU utilization, network statistics, scheduler behavior, etc.) aligned to the benchmark timeline.

## Parquet Output

The benchmark can record all metrics to a Parquet file for post-hoc analysis with the built-in viewer, or programmatically with tools like DuckDB, pandas, or Polars.

### Recording Behavior

- Snapshots are only recorded during the **Running** phase (not during Connect, Prefill, or Warmup)
- One snapshot is captured per `parquet_interval`
- A final snapshot is captured on shutdown
- The file metadata carries `source`, `version`, `sampling_interval_ms` and the
  [KPI template](#kpi-template) as `service_queries`

### Parquet Schema

Each snapshot contains the full set of metrics. The file uses the metriken exposition format with these metric types:

**Counters** (monotonically increasing, use `irate()` in the viewer for per-second rates):

| Metric | Description |
|--------|-------------|
| `requests_sent` | Total requests sent |
| `responses_received` | Total responses received |
| `request_errors` | Total request errors |
| `requests_send_failed` | RESP and memcache main-workload requests not sent because a send failed (usually a full send pool or zero-copy slab); not counted in `requests_sent` |
| `routed_queue_dropped` | With `workload.routing = "routed"`: requests the dispatcher funded but dropped because the owning node's queue was full; not counted in `requests_sent` |
| `cache_hits` | Total cache hits (GET responses with data) |
| `cache_misses` | Total cache misses (GET responses without data) |
| `get_count` | Total GET operations |
| `set_count` | Total SET operations |
| `delete_count` | Total DELETE operations |
| `backfill_set_count` | Backfill SET operations (from `backfill_on_miss`) |
| `bytes_tx` | Total bytes transmitted |
| `bytes_rx` | Total bytes received |
| `values_read_bytes` | GET value bytes checksummed by the client (`workload.read_values`); 0 when the option is off |
| `connections_failed` | Total failed connection attempts |
| `disconnects_eof` | Disconnects from EOF (server closed connection) |
| `disconnects_recv_error` | Disconnects from receive errors |
| `disconnects_send_error` | Disconnects from send errors |
| `disconnects_closed_event` | Disconnects from close events |
| `disconnects_error_event` | Disconnects from error events |
| `disconnects_connect_failed` | Disconnects from failed connect attempts |
| `cluster_redirects` | Total MOVED/ASK redirects (cluster mode) |
| `kv_requests` | KV replay: trace requests replayed to completion |
| `kv_requests_failed` | KV replay: trace requests abandoned |
| `kv_input_tokens`, `kv_cached_tokens`, `kv_new_tokens` | KV replay: prompt tokens in, served from the cache, and computed |
| `kv_full_hits` | KV replay: requests whose cached prefix covered the whole prompt |
| `kv_chunk0_misses` | KV replay: requests whose lookup found the first chunk missing |
| `kv_no_full_chunk` | KV replay: requests shorter than one chunk |
| `kv_get_nil` | KV replay: GETs that returned nil after their EXISTS returned 1 |
| `kv_exists_count` | KV replay: EXISTS operations |
| `kv_lookups`, `kv_loads`, `kv_stores` | KV replay: requests with a lookup, GETs, SETs (sample counts of the time histograms) |
| `kv_loads_with_cached_tokens` | KV replay: requests with GETs that ended with cached tokens (sample count of `kv_load_time_per_1k_tokens`) |
| `kv_trace_read_waits` | KV replay: dispatcher polls (one per worker per millisecond) that found a connection idle and the trace reader behind, over the whole run including warmup. Non-zero means the generator delayed requests; the run logs a warning |

**Gauges** (point-in-time values):

| Metric | Description |
|--------|-------------|
| `connections_active` | Currently active connections |
| `target_rate` | Current target request rate (saturation search) |

**Histograms** (latency distributions in nanoseconds):

| Metric | Description |
|--------|-------------|
| `response_latency` | All responses combined |
| `get_latency` | GET response latency |
| `set_latency` | SET response latency |
| `delete_latency` | DELETE response latency |
| `get_ttfb` | GET time-to-first-byte: from the send to the completion batch in which the worker handled the receive carrying the reply's first byte. Excludes routed queue wait |
| `backfill_set_latency` | Backfill SET latency |
| `schedule_slip` | Queueing the latency clock omits: rate-limiter tokens beyond one claim (one batch), divided by rate, per worker. Not recorded without a rate limiter (neither `workload.rate_limit` nor `[workload.saturation_search]`), so the column is absent. With `workload.routing = "routed"`, recorded once per funded request, including dropped ones, and not for backfill SETs |
| `perceived_latency` | `response_latency + schedule_slip`. What a client issuing at the target rate experienced |
| `routed_queue_wait` | With `workload.routing = "routed"`: time from the dispatcher funding a request to a connection sending it, recorded at send (including requests a failed send then discards). Included in `response_latency` and the per-command latencies |
| `kv_lookup_time`, `kv_load_time`, `kv_store_time` | KV replay, per trace request: first EXISTS, GET or SET sent to the last reply |
| `kv_load_time_per_1k_tokens` | KV replay, per trace request with cached tokens: load time × 1000 / cached tokens |
| `kv_schedule_lag` | KV replay, open loop, per trace request: start minus scheduled time |

### Programmatic Analysis

You can query Parquet files directly with DuckDB, pandas, or Polars:

```sql
-- DuckDB: query throughput over time
SELECT timestamp, responses_received
FROM 'results.parquet'
ORDER BY timestamp;
```

```python
# pandas
import pandas as pd
df = pd.read_parquet("results.parquet")
```

```python
# polars
import polars as pl
df = pl.read_parquet("results.parquet")
```

## Console Metrics

### Per-Second Samples

| Metric | Description |
|--------|-------------|
| Requests/sec | Successful requests completed |
| Errors/sec | Failed requests |
| Hit % | Cache hit rate (GET commands) |
| P50-P99.99 | Latency percentiles |
| Max | Maximum latency observed |

### Final Summary

| Metric | Description |
|--------|-------------|
| Duration | Actual test duration |
| Total requests | Requests sent and received |
| Throughput | Average requests/second |
| Cache hits/misses | Hit/miss counts |
| Hit rate | Percentage of GETs that hit |
| Bytes TX/RX | Network bytes transferred |
| GET latency | Percentiles for GET operations |
| SET latency | Percentiles for SET operations |
| Connections | Active/failed connection counts |
