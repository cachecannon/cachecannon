# Coordinated-omission honesty for cachecannon

**Date:** 2026-06-25
**Status:** Design — pending implementation

## Background

We recently addressed coordinated omission (CO) in `../llm-perf` (commits #141, #146,
#148, #145, #151). This spec ports the relevant ideas to cachecannon, adapted to its
architecture.

### The CO blind spot in cachecannon

cachecannon is **closed-loop-per-connection with a shared token-bucket throttle**. Each
connection keeps up to `pipeline_depth` requests in flight; a single shared
`ratelimit::Ratelimiter` (v2.0.0) throttles the aggregate send rate. Per-request latency
(`latency_ns`) is stamped inside ringline from `sent_at → response parsed`.

When the server slows under a rate-limited (open-loop-intent) run, pipelines fill, the
workers stop consuming tokens, and the bucket's `available()` backlog grows. The latency
clock starts only at *actual send*, so the queueing time spent waiting for the generator
to catch up is silently excluded — understating the tail exactly when the server is
saturated, which is the case the tool most needs to measure honestly.

Key insight: **the token bucket *is* the schedule.** `available()` tokens are requests
that are already overdue, so `available() / rate` (in seconds) is the omitted slip.
`dropped()` (already tracked by the crate) is load shed once the backlog hits
`max_tokens`. Together, slip and drops decompose the CO story completely.

Pure closed-loop runs (`rate_limit` unset, bucket rate 0) have `available() == 0`, so
slip is identically zero and all new behavior is inert — those runs are unaffected.

## Scope

Three workstreams:

- **(A) Slip + perceived latency** — make the reported tail honest.
- **(B) Overload / drop accounting** — make existing shed-on-overflow visible.
- **(C) Saturation search** — gate the SLO on CO-honest latency; add bisect+drain
  refinement; deprecate `stop_after_failures`; add transition flags.

Explicitly **out of scope**: marginal-gain plateau detection (deferred); pausing the
generator under overload (would re-introduce CO); per-request slip carry through ringline
(see "Rejected alternative" below).

## Design

### A. Slip and perceived latency

#### New metrics (`src/metrics.rs`)

| Metric | Type | Description |
|---|---|---|
| `SCHEDULE_SLIP` | `AtomicHistogram(7, 64)` (ns) | Per request sent: `available() * 1e9 / rate`. |
| `PERCEIVED_LATENCY` | `AtomicHistogram(7, 64)` (ns) | Per response: `latency_ns + current_slip_ns`. |
| `CURRENT_SLIP_NS` | `Gauge` (ns) | Global current slip; written by workers, read by callbacks. |
| `REQUESTS_DROPPED` | `Counter` | Mirror of `ratelimiter.dropped()`. |

Declared with the existing patterns (`AtomicHistogram::new(7, 64)`, `Gauge::new()`,
`Counter::new(...)`).

#### Slip computation (worker fire loops)

In each fire path — RESP (`worker.rs` ~974), memcache (~1466), ping (~1813) — when a rate
limiter is present, compute once per fire opportunity:

```rust
let rate = rl.rate().max(1);
let slip_ns = rl.available().saturating_mul(1_000_000_000) / rate;
metrics::CURRENT_SLIP_NS.set(slip_ns as i64);
```

Record one `SCHEDULE_SLIP.increment(slip_ns)` sample **per request actually sent** (at the
same point `REQUESTS_SENT` is incremented), so the slip histogram is request-weighted and
directly comparable to the latency histograms. When `rl` is `None` (no rate limit), slip is
0 and neither metric is touched.

`CURRENT_SLIP_NS` is also refreshed on the non-firing branch (pipeline full) so the gauge
keeps rising while the backlog grows even though no request is sent that iteration.

#### Perceived latency (on_result callbacks)

In `make_resp_callback` / `make_memcache_callback` / `make_ping_callback` (~730–789), after
the existing send-relative recording, add:

```rust
let _ = metrics::PERCEIVED_LATENCY.increment(r.latency_ns + metrics::CURRENT_SLIP_NS.get() as u64);
```

All existing histograms (`RESPONSE_LATENCY`, `GET_LATENCY`, `SET_LATENCY`,
`DELETE_LATENCY`, `GET_TTFB`) are left **byte-for-byte unchanged** — backward-comparable
with prior runs and older binaries.

#### Approximation (accepted)

Perceived latency uses the **global, recv-time** slip gauge rather than true per-request
send-time slip. This is an accepted approximation because:

- The bucket is global/shared, so all in-flight requests see ~the same backlog at any
  instant.
- Slip moves on ~second timescales (bucket fill), while cache request lifetimes are
  sub-millisecond, so send-time slip ≈ recv-time slip within one request's life.

The faithful alternative is documented and rejected below.

### B. Overload / drop accounting

`ratelimit` v2.0.0 already counts bucket-overflow drops via `dropped()`. The stats/output
tick reads it and updates `REQUESTS_DROPPED` (mirror the cumulative value). No generator
pause — the existing shed-on-overflow is just made visible. `offered = REQUESTS_SENT +
REQUESTS_DROPPED`. No new required config; `max_tokens` (auto = `max(rate, batch_size)`)
remains the cap.

### C. Saturation search (`src/saturation.rs`)

#### SLO gates on perceived latency

Baseline/step snapshots switch from `RESPONSE_LATENCY` to `PERCEIVED_LATENCY` for the
p50/p99/p999 the SLO evaluates (`slo_fail_reason`). The achieved/target throughput-ratio
guard is unchanged. Send-relative percentiles remain captured and shown as informational
columns. Net effect: the search stops at the rate where real queueing begins (the true
knee) instead of climbing on artificially-low send-relative tails.

#### Bisect + drain refinement

Add a phase to the state machine:

- **Climb** (today's behavior): multiply rate by `step_multiplier` each step until the
  first SLO failure. On that failure, record `lo = last_good_rate`, `hi = failed_rate`,
  switch to **Bisect**.
- **Bisect**: probe `mid = (lo + hi) / 2`, each probe preceded by the existing
  `drain_window` then measured over `sample_window`. Pass → `lo = mid`; fail → `hi = mid`.
  Terminate when **either** `(hi - lo) / hi <= bisect_tolerance` **or**
  `bisect_steps >= max_bisect_steps`. Report `lo` as the sustainable knee.

If the climb reaches `max_rate` without a failure, terminate as today (report `max_rate`
band).

#### Deprecate `stop_after_failures`

Keep the config field. If set, log a one-time deprecation warning; it no longer governs
termination (bisect convergence + `max_rate` ceiling do). Field retained as a no-op for
config compatibility.

#### Transition flags

Extend `SaturationStep` with booleans: `throughput_rollover` (achieved rate fell vs the
previous step), `slip_onset` (slip p99 crossed a small threshold for the first time),
`slo_breach` (first SLO failure). The printed table marks these so the report shows *where*
saturation set in, not just the final number. JSON output includes the flags.

#### New config (`SaturationSearch`)

- `bisect_tolerance: f64` — relative interval width to stop bisecting (default `0.05`).
- `max_bisect_steps: u32` — hard cap on bisect probes (default e.g. `8`).

### Output surfaces

- **`output/clean.rs`, `output/verbose.rs`**: add "Schedule Slip" and "Perceived Latency"
  sections plus an "Overload: N of M offered dropped (X%)" line. These sections are
  **suppressed when the run has no rate limit** (slip ≡ 0) to avoid noise on pure
  closed-loop runs.
- **`output/json.rs`**: add `schedule_slip`, `perceived_latency`, `requests_dropped`
  fields, and the saturation transition flags.
- **Viewer dashboard (`viewer/dashboard/latency.rs`)**: add `plot_promql` scatter plots
  for `schedule_slip` and `perceived_latency` percentiles, alongside the existing response
  plot (same `histogram_percentiles([...])` pattern). Metrics already reach the tsdb via
  the admin parquet path.

## Rejected alternative: per-request slip carry through ringline

True per-request perceived latency would require the send-time slip to reach the
`on_result` callback. That callback receives a `CommandResult` with **no** `user_data`
(`ringline-redis-0.5.0` `CommandResult` has only `command`, `latency_ns`, `hit`, `success`,
`ttfb_ns`, `tx_bytes`, `rx_bytes`). The per-request `user_data: u64` only round-trips on
the separate `recv()`/`CompletedOp` path and is already fully consumed encoding `key_id` +
prefill/backfill marker bits.

Carrying slip would need a **new opaque per-request tag** (a second 64-bit field for
`slip_at_send`) threaded through `fire_*` → ringline's internal pending-op tracking →
surfaced on `CommandResult`, across `ringline-redis`, `ringline-memcache`, and
`ringline-ping`, plus a coordinated three-crate release and version bump. Given the
approximation argument above (global bucket, sub-ms request lifetimes vs second-scale
slip), the fidelity gain is marginal. **Not worth it** for this iteration.

## Testing

Pure unit tests (no root / eBPF):

- **Slip math**: `(available, rate) → ns` including `rate == 0` guard and saturation at
  `max_tokens`.
- **Overload accounting**: `offered == sent + dropped`.
- **Bisect state machine**: convergence within tolerance and within `max_bisect_steps`;
  correct knee selection on a synthetic pass/fail oracle (mirrors llm-perf's tested
  `sample_workloads` pattern — the high-value, easily isolated piece).

Async fire-loop / callback changes: build + integration via existing
`config/saturation.toml` and `config/valkey-saturation-ci.toml`.

## Backward compatibility

- Existing latency metrics and their semantics are unchanged.
- New report/JSON fields are additive; slip/perceived sections suppressed when rate is
  unset.
- `stop_after_failures` retained as a deprecated no-op.
- New saturation config knobs have defaults; existing configs keep working.
