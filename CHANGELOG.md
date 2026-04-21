# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.0.14] - 2026-04-21

### Added
- Run the benchmark engine on macOS via ringline's mio backend; add macOS
  to the CI matrix for clippy, doc, test, and test-release (#79)

### Fixed
- Pre-acquire ratelimit tokens per batch so rate-limited fire loops emit
  fully coalesced batches instead of breaking early on `try_wait()` errors
  (#80)

### Changed
- Narrow Linux-only surface to kernel `SO_TIMESTAMPING` and
  `sched_setaffinity` CPU pinning; non-Linux now logs a warning and
  continues instead of failing (#79)
- Bump `ratelimit` to 2.0.0 for `try_wait_n` support (#80)

## [0.0.13] - 2026-04-20

### Fixed
- Coalesce `fire_*` calls into batched sends during steady state
  to reduce per-op syscall overhead (#75)
- Make the macOS build compile so pre-release `cargo clippy`
  and `cargo test` can run locally (#76)

## [0.0.12] - 2026-04-19

### Security
- Bump rustls-webpki to 0.103.12 to address RUSTSEC-2026-0098 and
  RUSTSEC-2026-0099 (#69)

### Fixed
- Fix final results latency histogram including warmup/prefill data,
  causing inflated tail latencies (p999+) in the summary report (#63)
- Fix prefill completion bug comparing global count against per-worker
  total (#41)
- Replace panicking unwrap() calls in viewer with graceful error
  handling (#42)
- Replace panicking unwrap with error propagation in cluster slot table
  build (#52)
- Use u128 for Prometheus histogram sum to prevent overflow (#54)
- Fix stale and misleading doc comments (#45)
- Set `target_rate` gauge for fixed-rate runs (#70)

### Changed
- Let saturation search control its own run duration (#61)
- Defer precheck timeout until workers finish initializing (#60)
- Use RwLock for slot table to reduce contention in route_key (#58)
- Add computed methods to Results, deduplicate formatter logic (#46)
- Split debug symbols into separate packages to reduce binary size (#62)
- Adopt ringline per-command byte metrics, TTFB, and op latency from
  `CompletedOp` (#67, #68)

### Added
- Add backfill-on-miss support to Momento driver (#50)
- Add config validation for value length against pool size (#51)
- Add config validation for threads, connections, commands, and
  saturation search (#43)
- Make viewer work cross-platform by gating benchmark deps behind
  cfg(target_os = "linux") (#59)
- Add Content-Type header to viewer data endpoint (#55)
- Log warnings when histogram percentile or delta computation fails (#44)
- Log error instead of panicking on dashboard view serialization
  failure (#53)
- Log warnings on JSON serialization failures instead of silently
  dropping output (#56)
- Log errors in admin server instead of silently discarding them (#47)
- Log worker thread panics during precheck and prefill error
  shutdown (#48)

### Removed
- Remove dead code from viewer plot module (#57)

## [0.0.11] - 2026-03-20

### Changed
- Upgrade to histogram 1.0.0 and metriken 0.8 — percentile queries now
  use the 0.0–1.0 scale (e.g. 0.99 instead of 99.0)
- Upgrade metriken-exposition to 0.14 and metriken-query to 0.6
- Replace custom DynamicRateLimiter with the ratelimit crate (1.0.0-alpha.0)
  from iopsystems/ratelimit
- Replace ringlog with tracing-appender for non-blocking log output

### Removed
- Remove unused histogram direct dependency (now used through metriken)
- Remove unused ringlog dependency

### Added
- Add CI integration tests for rate-limited and saturation search benchmarks
  using Valkey
- Switch TLS integration test from Redis to Valkey

## [0.0.10] - 2026-03-19

### Fixed
- Fix in-flight prefill key loss on disconnect by tracking keys between
  fire and recv and restoring them to the queue for retry
- Retry failed prefill SETs (OOM, READONLY, MOVED, etc.) instead of
  silently dropping them
- Apply key routing during prefill for multi-endpoint setups so keys
  land on the correct shard
- Fix stall detector zero-progress blindspot that prevented detection
  when prefill never made any progress

### Changed
- Replace minimal prefill progress line with full diagnostic table
  (set/s, err/s, connections, reconnects, progress) reported every 1s
- Prefill progress interval reduced from 5s to 1s for better diagnostic
  resolution

## [0.0.9] - 2026-03-17

### Changed
- Rename remaining valkey-bench references to valkey-lab

## [0.0.8] - 2026-03-17

### Changed
- Rename valkey-bench binary to valkey-lab
- Rename krio references to ringline

### Fixed
- Bump lz4_flex 0.11.5 → 0.11.6 to address RUSTSEC-2026-0041
  (information leak from invalid decompression input)

### Added
- Add product page (docs/index.html)
- Add curl|bash install script
- Add valkey-lab to install script

## [0.0.7] - 2026-03-10

### Fixed
- Fix viewer percentile charts by bumping metriken-query to 0.2.0

## [0.0.6] - 2026-03-02

### Added
- Add `valkey-bench` CLI binary as an alias for `cachecannon`
- Add separate `valkey-bench` DEB/RPM packages for standalone installation

## [0.0.5] - 2026-02-26

### Fixed
- Fix prefill stall caused by idle connections spin-looping without
  yielding to the cooperative scheduler, starving connections with
  pending responses

## [0.0.4] - 2026-02-26

### Changed
- Add fire/recv pipelining to RESP and Memcache workers, honoring
  `pipeline_depth` config (previously ignored for these protocols)
- Update ringline dependencies to c084d5c (fire/recv API)
- Simplify on_result callbacks to record latency only; counter metrics
  now go through the unified `record_counters()` path

### Fixed
- Fix tag-release workflow to create PR for dev version bump

## [0.0.3] - 2026-02-25

### Changed
- Vendor sharded counter module, removing external crucible dependency
- Update ringline dependencies to 1f08cac

## [0.0.2] - 2026-02-24

### Changed
- Update ringline dependencies to 0f76448
- Momento latency metrics now use the on_result callback pattern, matching
  valkey/memcache/ping protocols

## [0.0.1] - 2026-02-21

Initial release. Extracted from the [crucible](https://github.com/brayniac/crucible)
benchmark module into a standalone tool.

### Added
- High-performance cache protocol benchmarking with io_uring via ringline
- Protocol support: Valkey/Redis (RESP), Memcache (ASCII), Momento, Ping
- Multiplexed and pipelined request modes
- Kernel SO_TIMESTAMPING for precise latency measurement
- Precheck phase to verify connectivity before warmup
- Prefill phase to populate cache before benchmarking
- Configurable workload: keyspace, value size, command mix (get/set/delete)
- Rate limiting support
- Cluster mode with CLUSTER SLOTS discovery and redirect handling
- TLS support for all protocols
- Metrics exposition via Parquet snapshots and HTTP endpoint
- Built-in results viewer with web UI
- APT and YUM package repositories
- CI: clippy, tests, Momento integration tests, Valkey/Redis TLS tests
