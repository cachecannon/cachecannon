# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.0.19] - 2026-07-21

### Changed
- Reply handling now goes through ringline's uniform `recv_meta`
  zero-copy reply-metadata path (ringline #288): the client reads only
  the reply header, learns hit/miss/error and byte count from it, and
  drains the body without ever materializing the value. GET, SET, and
  DEL all share one recv model across both backends (io_uring borrows
  the provided-buffer segments; mio streams the drain), replacing the
  per-command, cfg-split reply mappers. Rig-measured against valkey
  9.1.0 (io-threads=16) on 2× c8gn.16xlarge @ 200 GbE: throughput is at
  parity with 0.0.18 (both saturate the NIC at 1M/16M/64M values), at
  ~5% lower client CPU at 64MB and with bounded, value-size-independent
  recv memory. (#110)
- Upgrade to the ringline 0.5.2 coordinated release: ringline 0.5.2,
  ringline-redis/-memcache 0.6.4, ringline-ping 0.5.2.

### Removed
- The `workload.values.length`-derived recv-buffer override (added in
  0.0.17, #104) is gone. With ringline's fallback recv (#274) and
  segmented delivery (#286), the small-buffer starvation cliff that
  motivated it is closed: rig A/B against valkey 9.1.0 at line rate
  showed the 256KiB override making zero difference vs ringline's
  default 16KiB (201 Gbps, byte-identical across reps at 1M/16M/64M),
  and on the copying path 16KiB is now ≥ 256KiB. ringline's default
  buffer geometry is used unmodified.

## [0.0.18] - 2026-07-17

### Changed
- Upgrade to the ringline 0.5.1 coordinated release: ringline 0.5.1,
  ringline-redis/-memcache 0.6.3, ringline-ping 0.5.2. Fixes an
  O(N·K) re-copy in the recv accumulator (ringline #279): a large
  response streamed across many recv completions was re-copying the
  whole accumulated buffer on every chunk, and the reserve-once path
  added in the previous release was latently unreachable behind it.
  Rig-measured in cachecannon (systemslab, loopback, GET-only 100%
  hit, 8 conns): 64MiB values ~4.2 → ~5.6 Gbps (+33%), GET p50
  2.66s → 2.08s.

## [0.0.17] - 2026-07-17

### Changed
- Provided recv-buffer size is now derived from `workload.values.length`
  (64KiB buffers for ≥64KiB values, 256KiB for ≥1MiB; ringline default
  otherwise). Rig-measured: large-value GET throughput tracks per-CQE
  buffer size, not ring capacity — 16KiB buffers cap ~2.4 Gbps at 16MB
  values vs ~4.9 Gbps at 256KiB. No user-facing knob; the generator
  should never be the bottleneck. (#104)
- Upgrade to the ringline 0.5.0 coordinated release: ringline 0.5,
  ringline-redis/-memcache 0.6.2, ringline-ping 0.5.1. Pulls in the
  ENOBUFS fallback recv (graceful degradation when a provided recv ring
  is smaller than one response — rig-validated 7–13× on deliberately
  starved rings; dormant on the auto-derived buffers above) and the
  `NeedAtLeast` accumulator reserve-once path (RESP bulk-length header
  pre-sizes the recv accumulator, eliminating the doubling-regrowth
  cascade on multi-MB values). Memcache values over 1MiB now go on the
  wire (the server's `-I` limit is authoritative). (#105)

### Fixed
- The final `connections N active` report sampled the gauge after worker
  shutdown had begun, under-reporting by however many connections had
  already torn down. The gauge is now sampled before shutdown. (#104)

## [0.0.16] - 2026-07-16

### Added
- Coordinated-omission honesty for rate-limited and saturation runs. New
  `schedule_slip` and `perceived_latency` histograms (plus `current_slip_ns`
  and `requests_dropped`) derive the queueing time the per-request latency
  clock omits from the rate limiter backlog (`available()/rate`), where
  `perceived = response_latency + slip`. Surfaced in the clean/verbose/json
  reports and the viewer latency dashboard, and suppressed on
  non-rate-limited runs (existing latency metrics are unchanged). (#99)
- Overload accounting: requests shed by the rate limiter under sustained
  overload are counted (`requests_dropped`) and reported as
  "Overload: N of M offered dropped". (#99)
- Saturation search bisect refinement: after the geometric climb finds the
  first SLO failure, the knee is pinned by bisection
  (`saturation_search.bisect_tolerance` / `max_bisect_steps`). Transition
  flags (throughput rollover, slip onset, first SLO breach) mark where
  saturation set in. (#99)

### Changed
- Migrate to ringline 0.4 (clients: ringline-redis/-memcache 0.6,
  ringline-ping 0.5). Rig-validated: fixes a hard worker stall with values
  over 1MiB — on ringline 0.3 an oversized GET response wedged the whole
  worker (throughput to zero on all connections, workers failing to shut
  down); on 0.4 the failure is contained to the affected connection.

### Fixed
- Redis (RESP) values over 1MiB now work: require ringline-redis 0.6.1,
  which lifts the client's RESP bulk-string parse cap (resp-proto
  `DEFAULT_MAX_BULK_STRING_LEN` = 1MiB) that made every oversized GET fail
  the connection. Rig-validated A/B at 4MiB values: 0.6.0 → 0% hit rate
  with ~47 reconnects/s; 0.6.1 → 100% hit rate, stable connections. Note:
  memcache values remain capped at 1MiB by the client's hard-coded
  `MAX_VALUE_LEN` (matching memcached's default `-I` limit) pending a
  configurable limit upstream.
- Saturation search now evaluates its SLO against perceived (CO-honest)
  latency, so it stops at the true knee instead of climbing on artificially
  low send-relative tails. Displayed per-step percentiles remain
  send-relative. (#99)
- Migrate to ringline 0.3 (clients: ringline-redis/-memcache 0.5, ringline-ping 0.4).
  `ringline::Config` is now opaque, so the runtime config is built via
  `ConfigBuilder` (`.workers()/.tcp_nodelay()/.tls_client()/.build()`), and
  `TlsClientConfig` is constructed via `::new()`. (#98)
- Memcache client now honors `batch_size` for fire/recv coalescing (wires
  `max_batch_size` from `effective_batch_size`, mirroring the redis client). (#97)
- Bump `ringline-redis`/`ringline-memcache` to 0.4 (memcache coalescing,
  `zc_threshold` guard fold, zero-allocation encode paths). Rig-validated:
  memcache GET coalescing ~10× at P16/64B. (#97)

### Deprecated
- `saturation_search.stop_after_failures` is deprecated and ignored;
  termination is now governed by `bisect_tolerance` / `max_bisect_steps`. (#99)

## [0.0.15] - 2026-06-10

### Changed
- Upgrade valkey-lab to ringline 0.2 / client crates 0.3 (#93)
- Bump rustls-webpki to 0.103.13 (#86)

### Fixed
- Fix cluster SET throughput by precomputing per-endpoint key lists and
  dropping rejection sampling; copy-send small SET values instead of using
  the zero-copy guard (#93)
- Share prefill queues across connections with global completion tracking
  and a connection guardrail (#92)
- Sign RPM package headers with rpmsign in the release workflow (#90)
- Detect cluster slot gaps and bound the per-connection backfill queue (#89)
- Mask marker bits when reading the GET key_id from user_data (#88)
- Validate saturation `start_rate` and reconnect on ping protocol errors (#87)
- Address output/admin audit findings (#85)
- Correct connection-lifecycle metrics and prefill bookkeeping (#84)
- Address design-review findings for perf and measurement correctness (#83)

### Removed
- Drop the Momento protocol, the `ringline-momento` client dependency, and the
  Momento integration test. Momento now exposes a RESP/Valkey-compatible
  endpoint, so it can be benchmarked through the existing RESP driver by
  pointing it at the Momento RESP endpoint with the API token as the password (#91)

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
