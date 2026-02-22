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
```

### Target Settings

```toml
[target]
# Target server addresses (supports multiple for load balancing)
endpoints = ["127.0.0.1:6379"]
# endpoints = ["server1:6379", "server2:6379", "server3:6379"]

# Protocol selection
# "resp" - Redis RESP2 protocol
# "resp3" - Redis RESP3 protocol
# "memcache" - Memcache ASCII protocol
# "memcache_binary" - Memcache binary protocol
# "momento" - Momento cache (cloud)
# "ping" - Simple PING/PONG
protocol = "resp"

# Enable TLS encryption
tls = false

# Enable Redis Cluster mode (see guide.md for details)
cluster = false
```

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

# Connection establishment timeout
connect_timeout = "5s"

# Individual request timeout
request_timeout = "1s"

# Request distribution strategy
# "roundrobin" - Distribute requests evenly across connections
# "greedy" - Fill one connection's pipeline before moving to next
request_distribution = "roundrobin"
```

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

# Automatically SET on GET miss (cache-aside pattern)
# See guide.md "Backfill on Miss" section for details
backfill_on_miss = false

[workload.keyspace]
# Key size in bytes
length = 16

# Number of unique keys in the keyspace
# Larger values = more cache misses, more realistic
count = 1000000

# Key selection distribution
# "uniform" - All keys equally likely
# "zipf" - Zipfian distribution (hot keys accessed more often)
distribution = "uniform"

[workload.commands]
# Command ratio as weights (will be normalized)
get = 80  # 80% GETs
set = 20  # 20% SETs

[workload.values]
# Value size for SET commands (bytes)
length = 64
```

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

# Stop after this many consecutive SLO violations
stop_after_failures = 3

# Maximum rate to try (absolute ceiling)
max_rate = 100000000

# Minimum ratio of achieved/target throughput (0.0-1.0)
# Fails the step if achieved throughput drops below this fraction
min_throughput_ratio = 0.9
```

### Timestamp Settings

```toml
[timestamps]
# Enable latency measurement
enabled = true

# Timestamp source
# "userspace" - Application-level timing (portable, ~1µs overhead)
# "software" - Kernel SO_TIMESTAMPING (Linux only, lower overhead)
mode = "userspace"
```

### Output Settings

```toml
[admin]
# Prometheus metrics endpoint (optional)
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

### Momento-Specific Settings

```toml
[momento]
# Cache name on Momento
cache_name = "my-cache"

# Momento endpoint (overrides MOMENTO_ENDPOINT env var)
endpoint = "cell-us-east-1-1.prod.a.momentohq.com"

# TTL for SET operations (seconds)
ttl_seconds = 3600

# Wire format: "grpc" or "protosocket"
wire_format = "grpc"

# Use private (VPC) endpoints instead of public endpoints
# When true, connections route through your VPC endpoint rather than
# the public internet, reducing latency and keeping traffic private
use_private_endpoints = false

# Availability zone for filtering private endpoints (e.g., "usw2-az1")
# Only relevant when use_private_endpoints = true
# Routes traffic to the endpoint in your AZ, minimizing cross-AZ latency
# Can also be set via the MOMENTO_AZ environment variable
availability_zone = "usw2-az1"
```

**Required environment variables:**

| Variable | Description |
|----------|-------------|
| `MOMENTO_API_KEY` | Your Momento API key (required) |
| `MOMENTO_ENDPOINT` | Cache endpoint (optional, can be set in config) |
| `MOMENTO_AZ` | Availability zone filter for private endpoints (optional, can be set in config) |

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
{"type":"config","protocol":"resp","endpoints":["127.0.0.1:6379"],"threads":4,...}
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

### RESP (Redis)

Supported commands:
- `GET key` — Retrieve value
- `SET key value` — Store value
- `PING` — Connection health check

The benchmark generates random keys based on keyspace settings and random values based on value length.

### Memcache

Supported commands:
- `get key` — Retrieve value (supports multi-get)
- `set key flags exptime bytes\r\nvalue` — Store value

ASCII protocol by default. Use `protocol = "memcache_binary"` for binary protocol.

### Momento

Supported commands:
- `Get(key)` — Retrieve value
- `Set(key, value, ttl)` — Store value with TTL

Requires `MOMENTO_API_KEY` environment variable. Optionally set `MOMENTO_ENDPOINT`.

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

### Parquet Schema

Each snapshot contains the full set of metrics. The file uses the metriken exposition format with these metric types:

**Counters** (monotonically increasing, use `irate()` in the viewer for per-second rates):

| Metric | Description |
|--------|-------------|
| `requests_sent` | Total requests sent |
| `responses_received` | Total responses received |
| `request_errors` | Total request errors |
| `cache_hits` | Total cache hits (GET responses with data) |
| `cache_misses` | Total cache misses (GET responses without data) |
| `get_count` | Total GET operations |
| `set_count` | Total SET operations |
| `delete_count` | Total DELETE operations |
| `backfill_set_count` | Backfill SET operations (from `backfill_on_miss`) |
| `bytes_tx` | Total bytes transmitted |
| `bytes_rx` | Total bytes received |
| `connections_failed` | Total failed connection attempts |
| `disconnects_eof` | Disconnects from EOF (server closed connection) |
| `disconnects_recv_error` | Disconnects from receive errors |
| `disconnects_send_error` | Disconnects from send errors |
| `disconnects_closed_event` | Disconnects from close events |
| `disconnects_error_event` | Disconnects from error events |
| `disconnects_connect_failed` | Disconnects from failed connect attempts |
| `cluster_redirects` | Total MOVED/ASK redirects (cluster mode) |

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
| `get_ttfb` | GET time-to-first-byte |
| `backfill_set_latency` | Backfill SET latency |

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
