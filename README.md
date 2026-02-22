# Cachecannon

A high-performance load generator for cache servers with support for multiple protocols, detailed latency measurements, and flexible workload configuration.

## Quick Start

```bash
# Build the benchmark tool
cargo build --release

# Run against a local Redis server
./target/release/cachecannon config/redis.toml

# Run against a local Memcached server
./target/release/cachecannon config/memcache.toml

# Save results to Parquet and view in the web dashboard
./target/release/cachecannon config/redis.toml \
    --parquet results.parquet
./target/release/cachecannon view results.parquet
```

## Features

- **Multiple protocols** — Redis RESP2/RESP3, Memcache ASCII/binary, Momento gRPC, Ping
- **io_uring I/O** — Same high-performance krio framework as the server (Linux 6.0+)
- **Request pipelining** — Configurable pipeline depth per connection
- **Latency histograms** — p50, p90, p99, p99.9, p99.99 with userspace or kernel timestamps
- **[Prefill and backfill](docs/guide.md#prefill)** — Pre-populate the cache or auto-SET on miss (cache-aside)
- **[Saturation search](docs/guide.md#saturation-search)** — Automatically find max throughput that meets your latency SLO
- **[Redis Cluster](docs/guide.md#cluster-mode)** — Topology discovery, hash slot routing, MOVED redirect handling
- **[Parquet output](docs/reference.md#parquet-output)** — Record all metrics for post-hoc analysis
- **[Web dashboard](docs/reference.md#results-viewer)** — Interactive viewer with throughput, latency, cache, and connection charts
- **[Rezolus correlation](docs/guide.md#correlating-with-rezolus)** — Overlay system telemetry (CPU, network, scheduler) on benchmark results
- **CPU pinning** — Pin worker threads to specific cores for reproducible results

## Command Line Interface

```
cachecannon <CONFIG> [OPTIONS]

Arguments:
  <CONFIG>  Path to TOML configuration file

Options:
  --parquet <PATH>      Override Parquet output path from config
  -h, --help            Print help

Subcommands:
  view                  View benchmark results in a web dashboard
```

### Viewer

```
cachecannon view <INPUT> [OPTIONS]

Arguments:
  <INPUT>               Path to benchmark parquet file

Options:
  --server <PATH>       Path to server-side Rezolus parquet file
  --client <PATH>       Path to client-side Rezolus parquet file
  --listen <ADDR>       Listen address [default: 127.0.0.1:0]
  -v, --verbose         Increase verbosity
```

## Choosing a Configuration

**First time?** Start with `quick-test.toml` to verify connectivity, then move to `redis.toml` or `memcache.toml` for a real benchmark:

```bash
# Verify everything works (5 seconds)
cachecannon config/quick-test.toml

# Run a full benchmark
cachecannon config/redis.toml
```

**Measuring capacity?** Use `saturation.toml` to automatically find the maximum throughput that meets your latency SLO.

**Testing a specific server feature?** Use `eviction-test.toml` (eviction behavior), `desync-test.toml` (protocol correctness under high concurrency), or `redis-cluster.toml` (cluster topology).

**Need to customize?** Copy `example.toml` — it has every available option with comments.

## Pre-built Configurations

| Config | Description |
|--------|-------------|
| `quick-test.toml` | 5-second smoke test — start here to verify connectivity |
| `redis.toml` | Standard Redis RESP protocol benchmark |
| `memcache.toml` | Memcache protocol benchmark (ASCII; binary protocol shown as comment) |
| `momento.toml` | Momento cloud cache (requires `MOMENTO_API_KEY`) |
| `ping.toml` | Simple PING/PONG baseline latency test |
| `example.toml` | Reference config with all available options commented |
| `redis-cluster.toml` | Redis Cluster with topology discovery and prefill |
| `saturation.toml` | Automated saturation search to find max SLO-compliant throughput |
| `eviction-test.toml` | High write rate to trigger eviction cycles |
| `desync-test.toml` | High concurrency stress test for protocol correctness (128 connections) |

## Comparison with Other Tools

| Feature | cachecannon | redis-benchmark | memtier_benchmark |
|---------|-------------------|-----------------|-------------------|
| io_uring support | Yes | No | No |
| Zero-copy send | Yes | No | No |
| Multiple protocols | RESP, Memcache, Momento | RESP only | RESP, Memcache |
| Parquet export | Yes | No | No |
| Web dashboard | Yes | No | No |
| Saturation search | Yes | No | No |
| Prefill / backfill | Yes | No | No |
| Cluster mode | Yes | Yes | Yes |
| Zipfian distribution | Yes | No | Yes |
| Pipelining | Yes | Yes | Yes |
| CPU pinning | Yes | No | Yes |

## Documentation

- **[Practical Guide](docs/guide.md)** — End-to-end walkthrough, workload patterns, prefill, backfill, saturation search, cluster mode, tuning, and troubleshooting
- **[Configuration & Metrics Reference](docs/reference.md)** — Complete TOML configuration, output formats, dashboard charts, Parquet schema, and full metrics catalog

## Building

```bash
# Build release binary
cargo build --release

# Build with specific features
cargo build --release --features io_uring

# Run tests
cargo test
```
