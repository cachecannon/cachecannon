# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed
- The append stream's per-batch `append batch opened` / `append batch
  committed` lines are logged at info only when `append.every` is at least
  10 s. At a shorter interval they are logged at debug
  (`RUST_LOG=cachecannon=debug`), since at a sub-second cadence they were
  several info lines a second. The `append stream configured` line at startup
  now says which level they use.

### Fixed
- `schedule_slip` no longer reads milliseconds at low connection counts while
  the configured rate is being delivered in full (#174). The token dispatcher
  (0.0.25, #166) slept up to 50 ms whenever its queue of waiting connections
  was empty, and a connection that queued a claim did not wake it. With few
  connections per worker the queue empties often, so a new claim could wait
  for the dispatcher's next wakeup while unspent tokens built up in the
  limiter. Slip is that backlog divided by the rate, and perceived latency adds
  slip to every response, so a saturation search judging on perceived
  percentiles could stop climbing with the server nowhere near its limit. The
  requests were also sent in bursts rather than on schedule. Queuing a claim
  now wakes an idle dispatcher.

## [0.0.26] - 2026-09-23

### Fixed
- A connection released by the token dispatcher (0.0.25, #166) without tokens
  now returns from its wait instead of staying parked. `drain` at the end of a
  run, and a claim failed with `ExceedsCapacity`, woke the connection without a
  grant, and the wait completed only on a non-zero grant, so it re-registered
  and never returned. At shutdown the parked tasks were dropped with the
  worker, so runs ended normally; the `ExceedsCapacity` case (only reachable
  with `max_tokens` at 0) would have stalled the connection for good. The wait
  now completes when the dispatcher is done with the claim, funded or not.
- In a rate-limited run, only a connection the limiter has just turned away
  parks on the token dispatcher. Since #166 every idle connection parked,
  including ones in Prefill with their queue drained. The dispatcher funded
  them, they looped and parked again, and each grant overwrote the last, so the
  limiter was drained at the configured rate for tokens nothing spent, and
  warmup opened with a batch held by every connection (up to 131K requests at
  4096 connections x 32). All of it landed in warmup, so measured results were
  not affected. Other idle states now use the fixed poll that closed-loop runs
  use.

## [0.0.25] - 2026-09-22

### Fixed
- `connection.request_timeout` (enforced since 0.0.24, #157) no longer costs
  O(connections per worker) per reply. 0.0.24 armed an io_uring timeout for
  every recv and cancelled it when the reply arrived; the kernel's
  `io_timeout_cancel` walks the ring's pending-timeout list, which holds about
  one timer per connection. Profiled on the rack at 4096 closed-loop
  connections, 8 threads, pipeline 32: the cancel was 69% of generator CPU
  (63% at 2048), every worker sat at a full core, and replies fell behind far
  enough to pass the 1 s timeout -- 1.02M req/s with ~480K timeouts per 60 s.
  With the timeout disabled the same cell ran 1.25M req/s on 2.16 cores (whole
  process; 0.194 per worker) with a 178 ms max and no timeouts, so the timeouts were caused by the timer cost,
  not by the server.

  Each connection now keeps one deadline timer across recvs, armed for its
  oldest in-flight request and left running when replies arrive. When it
  fires, it re-arms for the current oldest request or reports the timeout. A
  healthy connection re-arms about once per `request_timeout` and never
  cancels. Timeout behaviour is unchanged: the black-hole CI check and a mixed
  healthy/stalled run report identical timeout counts before and after.

  Same rig and cells, only the commit differing (cores per `ringline-worker`):

  | cell | before | after |
  |---|---|---|
  | 2048 closed, 1 s timeout | 0.754 | 0.189 |
  | 4096 closed, 1 s timeout | 0.999, 1.02M req/s, ~480K timeouts | 0.195, 1.22M req/s, 0 timeouts |
  | 4096 closed, timeout off | 0.194 | 0.156 |

  At 4096 with the timeout on, p99 went from 692 ms to 111 ms and max from
  1.07 s to 180 ms, matching the timeout-off cell.

### Changed
- Rate-limit tokens are dispatched once per worker instead of polled by every
  connection. Connections used to poll the shared `Ratelimiter` individually,
  which made the wakeup rate `connections / sleep` -- and since offered-load
  smoothness is *also* `connections / sleep`, how often anybody checks for a
  token, CPU cost and measurement fidelity were the same quantity and could only
  be traded against each other. Measured at 2048 connections and 20,000 req/s:
  cutting the wakeup rate eightfold halved CPU and took p99 from ~270us to
  376-1868us, with schedule slip p99 rising 100us -> 803us. No value of the
  oversampling constant was a free win; it only picked a point on that line.

  One task per worker now polls the limiter and funds queued claims, so wakeups
  scale with the token rate rather than the connection count while the cadence
  governing smoothness is set by the dispatcher alone. Measured on the rack
  (Linux/io_uring, 8 threads, Valkey 9.0.1, identical cells, only the commit
  differing) -- cores per `ringline-worker`:

  | cell | before | after |
  |---|---|---|
  | 4096 conns @ 300K req/s | 0.999 | 0.149 |
  | 4096 @ 200K | 0.964 | 0.117 |
  | 4096 @ 100K | 0.685 | 0.047 |
  | 4096 @ 50K | 0.406 | 0.026 |
  | 2048 @ 100K | 0.434 | 0.053 |
  | 4096 closed-loop | 0.999 | 1.000 |

  85-94% less generator CPU on the rate-limited cells, with latency improving
  rather than trading against it: 4096 @ 300K went p50 827 -> 238us and p99
  1720 -> 409us. Every cell hit its target rate exactly and no open-loop cell
  timed out in either arm.

  Closed-loop is deliberately untouched and measured flat to three decimals: a
  run with no `rate_limit` and no `[workload.saturation_search]` has no limiter,
  so it gets no dispatcher and keeps the old poll, which it reaches only on send
  backpressure. The 4096 closed-loop pin had a separate cause, the per-recv
  request timeout timer, fixed in this release (see Fixed above).


## [0.0.24] - 2026-09-18

### Changed
- The per-connection idle poll no longer jitters its sleep. #153 (shipped in
  0.0.23, and undocumented here) replaced a fixed 100 us poll with one that
  scales as `connections / (64 * rate)`, so the fleet-wide wakeup rate tracks
  the token rate instead of the connection count. That part stands. The jitter
  wrapped around it does not.

  Measured afterwards, cores at 20,000 req/s, each arm built from its release
  tag: at 64 connections 1.163 with backoff+jitter against 0.664 with backoff
  alone; at 2048 connections 3.488 without the backoff, 1.317 with
  backoff+jitter, 0.659 with backoff alone. Ranges disjoint throughout. At 64
  the sleep is already at the floor, so the jitter is the only variable and it
  costs 1.75x on the case the floor exists to leave alone. Both points fit timer
  coalescing: uniform sleeps share a slot and collapse into one wakeup, jittered
  ones smear across +/-25% of the sleep and each needs its own expiry.

  These are re-measurements taken 2026-09-18. An earlier set (`0.54 / 1.10 /
  0.51` and `3.50 / 1.27 / 0.51`) was taken on a laptop that had up to three
  other sessions compiling concurrently, which nobody knew at the time because
  nothing recorded the conditions per run. The re-take reproduces the
  `no backoff` and `backoff+jitter` columns closely and moves `backoff only`
  from 0.51 to 0.66 in both configs, so the penalty is 1.75x and 2.00x rather
  than the 2.16x and 2.49x first reported. The effect is real; the original
  magnitude was overstated.

  Two regimes matter because `idle_sleep` returns the floor when `rate == 0`
  (no `rate_limit` and no `[workload.saturation_search]`, i.e. closed-loop). In
  a rate-limited run the scaling does real work and dropping the jitter takes
  1.317 to 0.659; in a closed-loop run the scaling is inert and the jitter was
  pure cost. So this is better than 0.0.23 in both, with no workload preferring
  the jittered version. Corroborated on Linux/io_uring, where a 4096-connection
  closed-loop cell saturated every worker at 1.00 core.

  This gives up thundering-herd protection when a saturation-search rate step
  releases every waiter at once -- speculative when written, expensive when
  measured, and self-reporting if it occurs, since unclaimed tokens are limiter
  backlog and backlog is what `schedule_slip` measures.

### Fixed
- `connection.request_timeout` and `connection.connect_timeout` are enforced.
  `request_timeout` was parsed, documented and set in every shipped config, but
  nothing consulted it: a request could sit in a pipeline for the whole run
  and the reported tail latency would include it. #127 measured a p999 of
  8-12 s under a 1 s timeout, with a max that outlived the measurement window.
  Replies on a pipelined connection are ordered, so a stalled head stalls
  everything behind it and the only recovery is to abandon the connection:
  each connection now tracks the fire time of its in-flight requests and
  awaits every reply under the remaining budget of the oldest one. On expiry,
  everything in flight on that connection is counted as `request_timeouts`
  (and as errors), no latency sample is recorded for it, the connection is
  closed and re-established, and in-flight prefill and append keys are
  requeued. `connect_timeout` was only the precheck deadline; it now also
  bounds each connect attempt, including mid-run reconnects. `"0s"` disables
  either. Timeouts surface as `request_timeouts` / `disconnects_timeout`, on
  the results throughput line, and as `timeouts` in JSON. CI runs a job
  against a RESP server that answers PING and swallows everything else.
  (#157)

  Rig-measured afterwards (Valkey 9.0.1, 8 generator threads, pipeline 32,
  1 s timeout): at 2048 closed-loop connections, 1.27M req/s with p999 69 ms
  and no timeouts; at 4096, max is capped at 1.07 s instead of 62 s and the
  stalled requests report as ~8-9K timeouts/s, with every generator worker at
  a full core. The #127 tail was the generator at saturation, not the target,
  and the timeout now says so instead of hiding it in p999. At 4096
  connections open-loop at 300K req/s: p999 3-6 ms, no timeouts.

### Added
- `distribution = "recency"` and `[workload.append]`: a keyspace that grows in
  batches on a cadence, read newest-first. Key ids are a monotonic sequence and
  hotness attaches to the append batch: a read picks one of the
  `hot_generations` newest batches with weight `hot_decay^age`, then a key
  uniformly within it, and `hot_fraction` of reads land in that window while
  the rest go uniformly to the body. Dedicated writer connections
  (`append.connections`, their own pool so a batch never queues reads behind
  SETs inside the client) drain per-endpoint append queues through the same
  route / confirm-on-response / retry path prefill uses; the runner opens a
  batch every `append.every` from the start of warmup and publishes the new
  keyspace head only once the whole batch is confirmed, so a read never
  targets an unwritten key. `pace = "burst" | "spread"`. Append SETs report
  on their own `APPEND SET` line and as `append_set` / `append_batches` in
  JSON, never in the read workload's totals. RESP and memcache. Landed in
  #154 before 0.0.23 was cut and was not recorded there.

- `general.recv_ring_size` and `general.recv_buffer_size` expose the per-worker
  provided recv-buffer ring, which was previously ringline's default with no way
  to change it. Both default to exactly what ringline would have used (256 x
  16 KiB), so no existing run changes behaviour. `format = "verbose"` prints the
  resolved geometry and the buffers-per-response it implies.

  These are instrumentation, not a fix. An earlier draft of this change also
  derived `recv_buffer_size` from `workload.values.length` so one response fit
  one buffer, on the theory that multi-buffer responses shrink the ring's
  effective depth and invite `ENOBUFS` parking at high connection counts. Rig
  measurement refuted it: 10,000 connections, 20,000 req/s, 56 KiB values, four
  buffers per response against a 256-buffer ring -- every worker reported
  `parks=0 fallbacks=0`. Buffers are held only between a completion and the
  client draining it, so connection count does not consume them. The CQE
  argument does not rescue the derivation either: that generator spent ~12M
  CQEs/s to move 20k req/s, request I/O under 1% of it, so fitting a response in
  one buffer moves ~0.5% of the budget. The geometry stays ringline's until
  something measures otherwise; these keys are what make that measurable.

## [0.0.23] - 2026-09-17

### Fixed
- Counter-group metrics no longer vanish between the registry and the wire.
  `ShardedCounterGroup::value()` returns `Value::CounterGroup`, and both
  exposition sites -- the Prometheus endpoint and the msgpack/parquet snapshot
  -- matched only `Counter`, `Gauge` and `Histogram` before falling through to
  a catch-all. ringline publishes its entire runtime as counter groups, so
  everything it measures about the generator was counted at runtime and then
  dropped: `ringline/pool` carries RECV_PARKED (a connection's multishot recv
  hit ENOBUFS and parked while its socket stayed readable) and
  BUFFER_RING_EMPTY, `ringline/bytes` carries FALLBACK_RECEIVED (traffic
  arriving through the degraded path because a single response exceeded the
  provided buffer ring), `ringline/ring` carries SQE_SUBMIT_FAILURES.

  Those are precisely the counters that separate a buffer-starved generator
  from a slow server, and their absence is not neutral: a generator that cannot
  reap responses reports the delay as latency, so a saturated client reads as a
  degraded target. A 56 KiB-response characterisation run attributed 72% of its
  p50 at 10k connections to the server before client-side `tcp_packet_latency`
  showed the delay was the generator's own. None of it was recoverable from the
  recording afterwards.

  Groups are now flattened one series per populated slot. Prometheus renders
  them as labelled series (`ringline_pool{op="recv_parked"}`); the snapshot
  appends the slot index (`ringline/poolx4`) and keeps the base name in the
  `metric` metadata key, matching metriken-exposition's own snapshotter so the
  columns survive the eventual dependency bump. Slot labels come from the
  runtime's own metadata, so a slot that was never written emits nothing rather
  than a zero -- an unwritten counter must not read as "measured, and fine".

- The Prometheus endpoint served invalid exposition text on every run. Metric
  names must match `[a-zA-Z_:][a-zA-Z0-9_:]*`; linking ringline registers
  `ringline/connections/active`, which the gauge arm emitted verbatim, so
  `/metrics` carried `ringline/connections/active 0` whether or not ringline
  was doing anything. Names are now sanitized on the way out, as
  metriken-exposition does. Every cachecannon metric name was already legal and
  is unaffected -- asserted by a test, alongside one that scans the whole
  rendered body for illegal names.

### Performance
- Idle connections no longer poll the rate limiter at a fixed 100 us. A
  connection with nothing in flight is almost never waiting on the network; it
  is waiting for the shared limiter to mint a token, and the fixed poll made
  that cost O(connections) per 100 us, independent of the request rate.

  Measured at a fixed 20,000 req/s of 56 KiB values on a 16-worker
  c6gn.4xlarge, varying only connection count: at 64 connections the loop
  delivered 9,801 wakeups/s per connection -- one per 102 us, 98% of what a
  100 us timer demands, because there was CPU to spare. At 10,000 connections
  the design asked for 100M wakeups/s to hand out 20,000 tokens, delivered 12M,
  and pinned the generator at 15.8 of 16 cores. Completions per *request* rose
  31 -> 637 at constant request rate, and `dead` iterations fell 6.8% -> 0.1%.
  Responses then sat unread: `tcp_packet_latency` p50 reached 1536 us, 72% of
  the latency that run reported for the target under test.

  The sleep now scales as `connections / (K * rate)` with `K = 64`, holding the
  fleet-wide wakeup rate at `K * rate` whatever the connection count. It is
  clamped to [100 us, 50 ms], so small runs are bit-identical to the previous
  behaviour, and jittered +/-25% so a mid-run rate change does not
  re-synchronise every waiter into a herd. Aggregate throughput is unchanged --
  a token is claimed by whichever connection wakes next -- and the one cost a
  longer sleep can carry, a delayed next fire, is already reported as
  `schedule_slip`.

## [0.0.22] - 2026-09-10

Two measurement-correctness fixes. Both concern the saturation search
reporting a confident, plausible, wrong number rather than an error, which is
why neither surfaced from CI or a code review.

### Fixed
- Post-prefill writeback no longer lands in the first measured window.
  Prefill leaves the SERVER's page cache full of dirty pages, and Linux does
  not write them back promptly: `dirty_expire_centisecs` defaults to 3000 --
  thirty seconds -- after which the flusher evicts everything past expiry in
  one burst. Measured against a disk-backed cache server: prefill wrote 57 GB
  at 218-428 MB/s, and forty seconds after it ended the kernel dumped ~4.8 GB
  at 483 MB/s in a single ten-second burst, taking the first window's p99 to
  151ms. The search read that as the knee and reported 4.8K req/s where the
  same hardware sustained 55-60K; three configurations returned byte-identical
  numbers because all three bisected from the same burst. `warmup` already
  discards its samples but defaults to 10s, so the discard window was
  guaranteed to close before the burst it exists to absorb. New
  `workload.prefill_settle`, default 60s, added to warmup only when prefill
  runs, so a memory-only target pays nothing. Set to "0s" for the previous
  behavior. (#137)

### Added
- `saturation_search.floor_improvement_ratio`, default 0.10: the search now
  stops when latency stops responding to the offered rate. While no rate has
  passed, bisecting downward is productive only if latency depends on rate at
  all; when the target has a floor above the SLO -- a device read, a fixed
  round trip -- no rate will pass and every further halving is wasted. The
  existing tolerance check cannot catch this, because `lo` stays 0 and the
  interval width is therefore exactly 1.0 forever, leaving only the step limit
  to terminate. Measured: a search ran 5000 req/s down to 19, a 263x
  reduction, while p50 went from 668us to 889us -- it got worse -- burning 16
  measurement windows to establish what the third already showed. Detection is
  inert once any rate has passed, since the bisection is then narrowing a real
  interval. The verdict now distinguishes a target that cannot meet the SLO at
  any rate from one the search failed to bracket, and `SaturationResults`
  carries `latency_floor`. Set to 0 to always bisect to the step limit. (#138)

## [0.0.21] - 2026-09-10

### Added
- `general.ringline_diag` gates ringline's event-loop diagnostics, which
  were previously always on. (#131)

### Fixed
- `[workload.keyspace] distribution` is now implemented. The field was
  accepted by the config parser and never read: key selection was
  `rng.random_range(0..key_count)` unconditionally, so a run configured
  with `distribution = "zipf"` produced a uniform workload and reported
  it as a successful run. Nothing errored and the numbers looked
  plausible. Skew moves cache hit rate further than almost any other
  parameter, so measurements of a working set larger than memory were a
  uniform worst case being read as a typical one. Zipf sampling now uses
  `rand_distr::Zipf` (rejection-inversion, so O(1) setup rather than an
  O(keyspace) zeta prefix sum, and `theta == 1` is a legal skew); the
  sampler is built once by the runner and shared. New
  `keyspace.zipf_theta`, default 0.99 — the exponent published cache
  benchmarks quote as "zipfian 0.99". Skew across multiple endpoints is
  supported: hot keys hash onto particular shards and those shards take
  disproportionate load, which is the imbalance a skewed keyspace exists
  to measure, so skewed runs draw globally and reject-route rather than
  using the uniform per-endpoint fast path. (#133)
- The saturation search no longer treats a single failed sample as
  authoritative. A failure during the climb set the bisection ceiling
  permanently and no higher rate was ever retried, so one transient could
  cap a run far below the real knee — and the run still converged tidily
  and reported that knee as a clean result. Observed in practice: a step
  failing at p99 151ms anchored a run at 4.8K req/s on hardware that
  sustained 55-60K, and the tell was that three different configurations
  returned byte-identical numbers because all three had bisected from the
  same bad first sample. New `saturation_search.confirm_failures`,
  default 1: a rate must fail that many additional consecutive times
  before the search acts on it, and a retry that passes clears the count.
  Set to 0 for the previous behavior. (#134)

### Changed
- CI drops the unused Chrome apt source before updating. (#132)

## [0.0.20] - 2026-09-08

### Added
- Memcache binary protocol. `protocol = "memcache-binary"` was accepted
  by the config enum but rejected at validation as unimplemented; it is
  now driven via ringline-memcache's `BinaryClient`, sharing one drive
  loop with the ASCII client. The VERSION precheck is skipped for binary
  (the binary subset has no VERSION opcode); a live connection is the
  precheck. (#120)
- TLS against private CAs and mutual TLS. Three new PEM options on
  `[target]`: `tls_ca_file` (replaces the public roots with the CAs that
  verify the server, so `tls_verify = false` is no longer the only way to
  reach a server behind a private CA), and `tls_cert_file` /
  `tls_key_file` (client certificate presented for mutual TLS, applied on
  both the verifying and non-verifying paths). Config validation rejects
  a half-configured client identity and certificate options without
  `tls = true`; load errors name the offending file. (#123)
- RESP3 is now actually negotiated: `protocol = "resp3"` / `--resp3`
  sends `HELLO 3` on every new connection before workload traffic, so the
  server switches to RESP3 framing (`_\r\n` for a GET miss). The flag was
  previously inert. (#115)
- `[workload.keyspace] format = "hex" | "uuid"`. `uuid` renders each key
  as a canonical dashed UUID (requires `length = 36`) for servers that
  key by UUID; the id's low bits land in the trailing hex chars so keys
  spread evenly under last-N-hex directory sharding. (#121)
- Admin endpoint routing: `/metrics/binary` serves a msgpack `Snapshot`
  (the path `rezolus record` probes, so a benchmark run can be recorded
  alongside server- and client-side agents in one `.rez` archive);
  `/metrics` and `/` keep serving Prometheus text; other paths 404. The
  Prometheus output now includes the latency histograms, which were
  silently dropped before. (#118)
- CI: Memcache integration jobs (ASCII, binary, and binary over TLS with
  certificate verification against a generated CA), and a Valkey RESP3
  job. The Valkey TLS job now verifies the server certificate instead of
  setting `tls_verify = false`. (#115, #124, #125)

### Changed
- Upgrade to the ringline 0.6.0 coordinated breaking release plus the
  0.6.1 core patch: ringline 0.6.1, ringline-redis 0.7.0,
  ringline-memcache 0.7.1, ringline-ping 0.6.0, resp-proto 0.0.2. Carries
  an io_uring liveness fix (a worker reaped zero completions for as long
  as any task stayed runnable, under `DEFER_TASKRUN`), send-CQE
  generation validation, deferred-close ordering so queued sends reach
  the wire, and the resp-proto RESP line-framing fix for a stray `\r`
  inside a CRLF-terminated line. (#119)

### Fixed
- Runs asking for more than 256 connections per worker silently ran
  smaller than requested: ringline's standalone-task slab defaulted to
  256 per worker and the spawn error was discarded, so a 4096-connection
  run over 8 threads established 2040 and reported success. The slab is
  now sized from the connection count, and a failed spawn is logged and
  counted in `connections_failed`. (#122)
- The sibling `timer_slots` pool (also 256 per worker) was sized
  independently of the workload and, unlike the task slab, panicked the
  worker on exhaustion; with the release profile's `panic = "abort"`
  this surfaced as a bare `Aborted`. It is now sized from the connection
  count too, with two timers per connection. (#126)
- Histograms were dropped from metrics snapshots: `create_snapshot`
  never matched metriken-core 0.2's `Value::Histogram` variant, so
  neither the parquet recording nor the Prometheus endpoint carried any
  latency series. Both `cachecannon` and `valkey-lab` were affected.
  (#117)
- Docs and example configs said `protocol = "memcache_binary"`, which
  does not parse; corrected to `memcache-binary`. (#123)

### Removed
- The orphaned `config/redis-tls-ci.toml`, referenced by no workflow and
  the last config still setting `tls_verify = false`. (#128)

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
