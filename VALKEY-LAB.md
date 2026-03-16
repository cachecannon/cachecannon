# valkey-lab

A high-performance Valkey/Redis benchmark tool powered by [cachecannon](https://github.com/cachecannon/cachecannon).

Uses `io_uring` for kernel-bypassed I/O, per-connection pipelining, and multi-threaded workers to saturate modern cache servers.

## Quick Start

```bash
# Benchmark localhost:6379 for 60 seconds (defaults)
valkey-lab

# Custom host, 16 connections, pipeline depth 32
valkey-lab -h 10.0.0.1 -c 16 -P 32

# 30-second run, 100:0 GET-only workload, 1M keys
valkey-lab -d 30s -r 100:0 -n 1000000

# Find maximum throughput under a p99.9 < 1ms SLO
valkey-lab saturate --slo-p999 1ms --start-rate 100000
```

## Installation

```bash
cargo install --path . --bin valkey-lab
```

Requires Linux (io_uring). Tested on kernel 5.10+.

## CLI Reference

### Tier 1 — Common Flags

Flag | Short | Default | Description
---- | ----- | ------- | -----------
`--host` | `-h` | `127.0.0.1` | Server hostname or IP
`--port` | `-p` | `6379` | Server port
`--duration` | `-d` | `60s` | Test duration (e.g. 30s, 5m, 1h)
`--connections` | `-c` | `1` | Number of connections
`--pipeline` | `-P` | `1` | Pipeline depth per connection
`--threads` | `-t` | CPU count | Number of worker threads
`--ratio` | `-r` | `80:20` | GET:SET command ratio
`--keyspace` | `-n` | `1000000` | Number of unique keys
`--data-size` | `-s` | `64` | Value size in bytes
`--cluster` | --- | off | Enable Valkey Cluster mode
`--tls` | --- | off | Enable TLS encryption
`--output` | `-o` | `clean` | Output format: clean, json, verbose, quiet

### Tier 2 — Power User Flags

Flag | Default | Description
---- | ------- | -----------
`--warmup` | `10s` | Warmup period before recording
`--key-size` | `16` | Key size in bytes
`--distribution` | `uniform` | Key distribution: uniform or zipf
`--rate-limit` | unlimited | Max requests/second
`--prefill` | off | Prefill all keys before benchmarking
`--backfill` | off | SET on GET miss to backfill cache
`--parquet` | none | Write results to Parquet file
`--cpu-list` | none | Pin threads to CPUs (e.g. 0-3,8-11)
`--resp3` | off | Use RESP3 protocol
`--tls-skip-verify` | off | Skip TLS certificate verification
`--tls-hostname` | host value | TLS SNI hostname override
`--connect-timeout` | `5s` | Connection timeout
`--request-timeout` | `1s` | Request timeout
`--color` | `auto` | Color mode: auto, always, never
`--config` | none | Load base config from TOML file

## Subcommands

### `saturate` — Find Maximum SLO-Compliant Throughput

Automatically ramps request rate until latency SLOs are breached, then reports the maximum compliant throughput.

```bash
valkey-lab saturate --slo-p999 1ms
valkey-lab saturate --slo-p99 500us --slo-p999 2ms --start-rate 50000
valkey-lab saturate --slo-p999 1ms -c 16 -P 32 --start-rate 500000 --step 1.1
```

Flag | Default | Description
---- | ------- | -----------
`--slo-p50` | none | p50 latency SLO (e.g. 500us, 1ms)
`--slo-p99` | none | p99 latency SLO
`--slo-p999` | none | p99.9 latency SLO
`--start-rate` | `1000` | Starting request rate
`--step` | `1.05` | Rate multiplier per step (1.05 = +5%)
`--sample-window` | `5` | Seconds at each rate level
`--max-rate` | `100000000` | Absolute rate ceiling

At least one `--slo-*` flag is required. Each step ramps the rate and reports a block summary:

```
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
STEP 1 — PASS

SLO:    500K @ p999 ≤ 1ms
Result: 498K @ p999=312 µs
Headroom: 69%
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
STEP 5 — FAIL — Throughput Limited

SLO:    551K @ p999 ≤ 1ms
Result: 412K @ p999=845 µs
Throughput: 75% (need 90%)
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
STEP 6 — FAIL — Latency Exceeded

SLO:    579K @ p999 ≤ 1ms
Result: 401K @ p999=3.40 ms
Latency: p999 3400us > 1000us SLO
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

────────────────────────────────────────────────────────
MAX COMPLIANT THROUGHPUT: 525K req/s
```

### `view` — Web Dashboard

View Parquet benchmark results in a browser.

```bash
valkey-lab view results.parquet
```

## Examples

```bash
# Quick sanity check
valkey-lab -d 10s

# High-throughput test
valkey-lab -c 32 -P 64 -t 8

# Cluster mode with TLS
valkey-lab --cluster --tls -h cluster.example.com

# Write-heavy workload with Zipfian distribution
valkey-lab -r 20:80 --distribution zipf -s 512

# Prefill cache, then benchmark GETs only
valkey-lab --prefill -r 100:0

# Rate-limited test for consistent results
valkey-lab --rate-limit 50000 -c 4

# Save results for later analysis
valkey-lab --parquet results.parquet -d 5m

# Saturation search with high starting rate
valkey-lab saturate --slo-p999 1ms --start-rate 100000 --step 1.1 -c 16 -P 32
```

## Relationship to cachecannon

valkey-lab is an additive companion binary that ships alongside cachecannon. It provides a streamlined CLI for the most common use case — benchmarking Valkey/Redis servers — while cachecannon remains the full-featured tool supporting TOML configs, Memcache, Momento, and advanced workflow options.

Both binaries share the same engine: io_uring workers, latency histograms, cluster discovery, prefill/backfill, saturation search, and Parquet output.

## License

Same as cachecannon — see [LICENSE](LICENSE).
