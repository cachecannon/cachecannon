# Coordinated-Omission Honesty Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make cachecannon's rate-limited and saturation runs report coordinated-omission-honest latency by deriving schedule slip from the token bucket's backlog, surfacing shed load, and gating the saturation search on the corrected latency with a bisect-refined knee.

**Architecture:** The shared `ratelimit::Ratelimiter` token bucket *is* the schedule: `available()/rate` is the omitted queueing slip and `dropped()` is shed load. Workers record `SCHEDULE_SLIP` per request and publish a global `CURRENT_SLIP_NS` gauge; the result callbacks add it to produce `PERCEIVED_LATENCY`. Saturation gates its SLO on perceived latency and refines the knee by bisection.

**Tech Stack:** Rust, `metriken` (metrics), `ratelimit` v2.0.0, `ringline-*` clients, existing TOML config + output formatters.

**Design spec:** `docs/superpowers/specs/2026-06-25-coordinated-omission-design.md`

**Conventions:** Build with `cargo build`. Run a single test with `cargo test <name>`. Lint with `cargo clippy`. Commit after each task. Existing latency metrics are NEVER modified.

---

## Task 1: Slip math helper (pure function + tests)

A pure function converting bucket backlog to omitted slip in nanoseconds. Lives in `metrics.rs` so it sits next to the metric it feeds.

**Files:**
- Modify: `src/metrics.rs` (append helper + test module)

- [ ] **Step 1: Write the failing test**

Append to the end of `src/metrics.rs`:

```rust
#[cfg(test)]
mod co_tests {
    use super::slip_ns;

    #[test]
    fn slip_is_backlog_over_rate_in_ns() {
        // 1000 overdue tokens at 1000 req/s = 1.0s = 1e9 ns.
        assert_eq!(slip_ns(1000, 1000), 1_000_000_000);
        // 50 tokens at 1000 req/s = 50ms.
        assert_eq!(slip_ns(50, 1000), 50_000_000);
        // No backlog = no slip.
        assert_eq!(slip_ns(0, 1000), 0);
    }

    #[test]
    fn slip_handles_zero_rate_without_panicking() {
        // Rate 0 (unlimited / not configured): no schedule, no slip.
        assert_eq!(slip_ns(1234, 0), 0);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test slip_is_backlog_over_rate_in_ns`
Expected: FAIL — `cannot find function slip_ns`.

- [ ] **Step 3: Write the helper**

Add near the latency-histogram section of `src/metrics.rs` (e.g. just above `RESPONSE_LATENCY` at line ~160):

```rust
/// Omitted schedule slip in nanoseconds, derived from the rate limiter's
/// backlog. `available` overdue tokens at `rate` req/s represent
/// `available / rate` seconds of queueing the latency clock did not see.
/// Returns 0 when `rate == 0` (no schedule configured).
pub fn slip_ns(available: u64, rate: u64) -> u64 {
    if rate == 0 {
        return 0;
    }
    available.saturating_mul(1_000_000_000) / rate
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test co_tests`
Expected: PASS (both tests).

- [ ] **Step 5: Commit**

```bash
git add src/metrics.rs
git commit -m "feat(metrics): slip_ns helper — bucket backlog to omitted slip"
```

---

## Task 2: New CO metric declarations

Declare the four new metrics. No behavior change yet — just registration.

**Files:**
- Modify: `src/metrics.rs` (after `BACKFILL_SET_LATENCY`, line ~195)

- [ ] **Step 1: Add the metrics**

Append after the `BACKFILL_SET_LATENCY` declaration (line ~195):

```rust
// ── Coordinated-omission metrics ─────────────────────────────────────────
#[metric(
    name = "schedule_slip",
    description = "Per-request schedule slip histogram (nanoseconds): queueing time the latency clock omits, from the rate limiter backlog"
)]
pub static SCHEDULE_SLIP: AtomicHistogram = AtomicHistogram::new(7, 64);

#[metric(
    name = "perceived_latency",
    description = "Arrival-relative (CO-honest) response latency histogram (nanoseconds): response_latency + schedule slip"
)]
pub static PERCEIVED_LATENCY: AtomicHistogram = AtomicHistogram::new(7, 64);

#[metric(
    name = "current_slip_ns",
    description = "Current schedule slip in nanoseconds (rate limiter backlog / rate)"
)]
pub static CURRENT_SLIP_NS: Gauge = Gauge::new();

#[metric(
    name = "requests_dropped",
    description = "Requests shed by the rate limiter under overload (bucket overflow)"
)]
pub static REQUESTS_DROPPED: Gauge = Gauge::new();
```

Note: `REQUESTS_DROPPED` is a `Gauge` (not a `Counter`) because it mirrors the rate limiter's cumulative `dropped()` value by `set()`, not by incrementing local slots.

- [ ] **Step 2: Build to verify it compiles**

Run: `cargo build`
Expected: builds cleanly.

- [ ] **Step 3: Commit**

```bash
git add src/metrics.rs
git commit -m "feat(metrics): declare schedule_slip, perceived_latency, current_slip_ns, requests_dropped"
```

---

## Task 3: Wire slip + perceived latency into the RESP path

Publish slip from the RESP fire loop and add perceived latency in the RESP callback.

**Files:**
- Modify: `src/worker.rs` — RESP fire loop (~969–1063) and `make_resp_callback` (~730–751)

- [ ] **Step 1: Publish slip in the RESP fire loop**

In the RESP fire branch, the token grant is computed at `src/worker.rs:974` (`let mut token_budget = match state.task_state.ratelimiter { ... }`). Immediately after that `match` block (after line 984, before the `while client.pending_count() < pipeline_depth` loop), insert:

```rust
            // Coordinated omission: publish the current schedule slip (rate
            // limiter backlog) so each sent request can be charged a sample
            // and the result callback can compute perceived latency.
            let slip_ns = match state.task_state.ratelimiter {
                Some(ref rl) => crate::metrics::slip_ns(rl.available(), rl.rate()),
                None => 0,
            };
            crate::metrics::CURRENT_SLIP_NS.set(slip_ns as i64);
```

- [ ] **Step 2: Charge a slip sample per request sent (RESP)**

In the same loop, at the steady-state send success site `src/worker.rs:1057-1058`:

```rust
                if sent {
                    metrics::REQUESTS_SENT.increment();
```

change to:

```rust
                if sent {
                    metrics::REQUESTS_SENT.increment();
                    let _ = metrics::SCHEDULE_SLIP.increment(slip_ns);
```

And at the backfill send-success site `src/worker.rs:1009-1010`:

```rust
                        Ok(_) => {
                            metrics::REQUESTS_SENT.increment();
```

change to:

```rust
                        Ok(_) => {
                            metrics::REQUESTS_SENT.increment();
                            let _ = metrics::SCHEDULE_SLIP.increment(slip_ns);
```

- [ ] **Step 3: Keep the slip gauge fresh when the pipeline is full (RESP)**

When the pipeline is full the fire branch is skipped, so the gauge would go stale while the backlog grows. At the top of the connection loop, just before the `if ... want_fire` dispatch (the `else if (phase == Phase::Warmup || phase == Phase::Running) && want_fire` at `src/worker.rs:969`), the loop already runs every iteration. Add this unconditional refresh right after the phase/firing decision is known but before the recv wait — concretely, immediately before `// Wait for one response` at `src/worker.rs:1065`:

```rust
        // Refresh the slip gauge every iteration (including when the pipeline
        // is full and we did not fire) so perceived latency reflects a still-
        // growing backlog.
        if let Some(ref rl) = state.task_state.ratelimiter {
            crate::metrics::CURRENT_SLIP_NS.set(crate::metrics::slip_ns(rl.available(), rl.rate()) as i64);
        }
```

- [ ] **Step 4: Add perceived latency in the RESP callback**

In `make_resp_callback` (`src/worker.rs:747`), after `let _ = metrics::RESPONSE_LATENCY.increment(r.latency_ns);` add:

```rust
        let _ = metrics::RESPONSE_LATENCY.increment(r.latency_ns);
        let _ = metrics::PERCEIVED_LATENCY
            .increment(r.latency_ns + metrics::CURRENT_SLIP_NS.value() as u64);
```

Note: `Gauge` reads via `.value()` (returns `i64`); `slip_ns` is non-negative so the cast is safe.

- [ ] **Step 5: Build**

Run: `cargo build`
Expected: builds cleanly.

- [ ] **Step 6: Commit**

```bash
git add src/worker.rs
git commit -m "feat(worker): RESP schedule slip + perceived latency"
```

---

## Task 4: Wire slip + perceived latency into the memcache path

Same pattern as Task 3, for the memcache loop and callback.

**Files:**
- Modify: `src/worker.rs` — memcache fire loop (~1411–1571) and `make_memcache_callback` (~755–776)

- [ ] **Step 1: Publish slip in the memcache fire loop**

The memcache token grant is at `src/worker.rs:1466`. Immediately after that `match` block, insert the same slip block:

```rust
            let slip_ns = match state.task_state.ratelimiter {
                Some(ref rl) => crate::metrics::slip_ns(rl.available(), rl.rate()),
                None => 0,
            };
            crate::metrics::CURRENT_SLIP_NS.set(slip_ns as i64);
```

- [ ] **Step 2: Charge a slip sample per request sent (memcache)**

At the memcache steady-state send-success site (`metrics::REQUESTS_SENT.increment();` at `src/worker.rs:1541`) add immediately after it:

```rust
                    let _ = metrics::SCHEDULE_SLIP.increment(slip_ns);
```

At the memcache backfill send-success site (`metrics::REQUESTS_SENT.increment();` at `src/worker.rs:1490`) add immediately after it:

```rust
                            let _ = metrics::SCHEDULE_SLIP.increment(slip_ns);
```

- [ ] **Step 3: Keep the slip gauge fresh when the pipeline is full (memcache)**

Immediately before the memcache `// Wait for one response` / recv section (the `if client.pending_count() == 0` mirror in the memcache loop, around `src/worker.rs:1545`), insert:

```rust
        if let Some(ref rl) = state.task_state.ratelimiter {
            crate::metrics::CURRENT_SLIP_NS.set(crate::metrics::slip_ns(rl.available(), rl.rate()) as i64);
        }
```

- [ ] **Step 4: Add perceived latency in the memcache callback**

In `make_memcache_callback`, after `let _ = metrics::RESPONSE_LATENCY.increment(r.latency_ns);` (`src/worker.rs:772`) add:

```rust
        let _ = metrics::PERCEIVED_LATENCY
            .increment(r.latency_ns + metrics::CURRENT_SLIP_NS.value() as u64);
```

- [ ] **Step 5: Build + commit**

Run: `cargo build` (expect clean), then:

```bash
git add src/worker.rs
git commit -m "feat(worker): memcache schedule slip + perceived latency"
```

---

## Task 5: Wire slip + perceived latency into the ping path

The ping loop is single-in-flight with a blocking rate-limit wait (`src/worker.rs:1813`).

**Files:**
- Modify: `src/worker.rs` — ping loop (~1809–1837) and `make_ping_callback` (~779–788)

- [ ] **Step 1: Publish slip + charge a sample in the ping loop**

After the rate-limit acquire succeeds and before/at the send (`metrics::REQUESTS_SENT.increment();` at `src/worker.rs:1827`), insert just after the `if let Some(ref rl) = ... { match rl.try_wait() {...} }` block (after line 1825) and before `metrics::REQUESTS_SENT.increment();`:

```rust
        let slip_ns = match state.task_state.ratelimiter {
            Some(ref rl) => crate::metrics::slip_ns(rl.available(), rl.rate()),
            None => 0,
        };
        crate::metrics::CURRENT_SLIP_NS.set(slip_ns as i64);

        metrics::REQUESTS_SENT.increment();
        let _ = metrics::SCHEDULE_SLIP.increment(slip_ns);
```

(Replace the existing standalone `metrics::REQUESTS_SENT.increment();` at line 1827 with the two lines above so the sample is charged on every sent ping.)

- [ ] **Step 2: Add perceived latency in the ping callback**

In `make_ping_callback`, after `let _ = metrics::RESPONSE_LATENCY.increment(r.latency_ns);` (`src/worker.rs:786`) add:

```rust
        let _ = metrics::PERCEIVED_LATENCY
            .increment(r.latency_ns + metrics::CURRENT_SLIP_NS.value() as u64);
```

- [ ] **Step 3: Build + commit**

Run: `cargo build` (expect clean), then:

```bash
git add src/worker.rs
git commit -m "feat(worker): ping schedule slip + perceived latency"
```

---

## Task 6: Overload / drop accounting

Mirror the rate limiter's cumulative `dropped()` into `REQUESTS_DROPPED`, and add a tested `offered` helper.

**Files:**
- Modify: `src/runner.rs` — the periodic stats/output tick that already reads metrics (the loop containing the saturation `check_and_advance` near `src/runner.rs:603`)
- Modify: `src/output/mod.rs` — add `requests_dropped` to `Results` + an `offered()`/`overload_pct()` helper with a test

- [ ] **Step 1: Write the failing test for overload accounting**

Append to `src/output/mod.rs` a test module:

```rust
#[cfg(test)]
mod overload_tests {
    use super::Results;

    fn results_with(requests: u64, dropped: u64) -> Results {
        Results {
            duration_secs: 1.0,
            requests,
            responses: requests,
            errors: 0,
            hits: 0,
            misses: 0,
            bytes_tx: 0,
            bytes_rx: 0,
            get_count: 0,
            set_count: 0,
            get_latencies: Default::default(),
            get_ttfb: Default::default(),
            set_latencies: Default::default(),
            backfill_set_count: 0,
            backfill_set_latencies: Default::default(),
            conns_active: 0,
            conns_failed: 0,
            conns_total: 0,
            requests_dropped: dropped,
        }
    }

    #[test]
    fn offered_is_sent_plus_dropped() {
        let r = results_with(900, 100);
        assert_eq!(r.offered(), 1000);
        assert!((r.overload_pct() - 10.0).abs() < 1e-9);
    }

    #[test]
    fn overload_pct_zero_when_nothing_offered() {
        let r = results_with(0, 0);
        assert_eq!(r.offered(), 0);
        assert_eq!(r.overload_pct(), 0.0);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test overload_tests`
Expected: FAIL — `Results` has no field `requests_dropped` / no method `offered`.

- [ ] **Step 3: Add the field and helpers**

In `src/output/mod.rs`, add to `struct Results` (after `conns_total` at line 148):

```rust
    pub conns_total: u64,
    /// Requests shed by the rate limiter under overload (bucket overflow).
    pub requests_dropped: u64,
```

And in `impl Results` (after `tx_bps`, line ~196):

```rust
    /// Total requests offered = sent + dropped.
    pub fn offered(&self) -> u64 {
        self.requests + self.requests_dropped
    }

    /// Percentage of offered requests shed under overload (0.0-100.0).
    pub fn overload_pct(&self) -> f64 {
        let offered = self.offered();
        if offered > 0 {
            (self.requests_dropped as f64 / offered as f64) * 100.0
        } else {
            0.0
        }
    }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test overload_tests`
Expected: PASS.

- [ ] **Step 5: Mirror `dropped()` into the metric and populate `Results`**

In `src/runner.rs`, in the periodic loop where the saturation state is advanced (the block using `if ... && let Some(ref rl) = ratelimiter` near line 603), add a mirror each tick:

```rust
        if let Some(ref rl) = ratelimiter {
            metrics::REQUESTS_DROPPED.set(rl.dropped() as i64);
        }
```

The final report subtracts warmup baselines (see `let requests = metrics::REQUESTS_SENT.value() - baseline_requests;` at `src/runner.rs:754`). Capture a drop baseline next to the other `baseline_*` captures at the warmup→running transition (search the file for `baseline_requests =`):

```rust
    let baseline_requests_dropped = ratelimiter
        .as_ref()
        .map(|rl| rl.dropped())
        .unwrap_or(0);
```

Then in the `let requests = ...` delta block (near `src/runner.rs:754`):

```rust
    let requests_dropped = ratelimiter
        .as_ref()
        .map(|rl| rl.dropped().saturating_sub(baseline_requests_dropped))
        .unwrap_or(0);
```

And add the field to `Results { ... }` at `src/runner.rs:795`:

```rust
        conns_total: total_connections as u64,
        requests_dropped,
```

(`metrics` is already in scope here — the same fn sets `metrics::TARGET_RATE` at line 120.)

- [ ] **Step 6: Build + commit**

Run: `cargo build` (expect clean), `cargo test overload_tests` (expect PASS), then:

```bash
git add src/runner.rs src/output/mod.rs
git commit -m "feat(overload): mirror ratelimit dropped() into requests_dropped + offered helpers"
```

---

## Task 7: Output — slip / perceived / overload sections for the main run

Surface the new metrics in the final results. Sections are suppressed when there is no rate limit (slip ≡ 0). The slip/perceived percentiles are read from the histograms into `LatencyStats` the same way existing latencies are.

**Files:**
- Modify: `src/runner.rs:777` `Results` build — add `schedule_slip` + `perceived_latency` `LatencyStats`
- Modify: `src/output/mod.rs` `struct Results` — add the two `LatencyStats` fields
- Modify: `src/output/clean.rs`, `src/output/verbose.rs` `print_results` — print the sections
- Modify: `src/output/json.rs` — add JSON fields

- [ ] **Step 1: Add fields to `Results`**

In `src/output/mod.rs` `struct Results`, after `requests_dropped`:

```rust
    pub requests_dropped: u64,
    /// Schedule slip percentiles (microseconds). Zero when no rate limit.
    pub schedule_slip: LatencyStats,
    /// Perceived (arrival-relative, CO-honest) response latency percentiles (µs).
    pub perceived_latency: LatencyStats,
```

Update the `overload_tests` constructor in `src/output/mod.rs` to set both new fields to `Default::default()`.

- [ ] **Step 2: Populate them in the runner**

The runner converts histograms to `LatencyStats` via `delta_latency_stats(&histogram, &baseline)` (see `src/runner.rs:769-775`), subtracting a warmup baseline. Capture baselines for the two new histograms next to the existing `baseline_*_latency` captures at the warmup→running transition (search for `baseline_get_latency =`):

```rust
    let baseline_schedule_slip = metrics::SCHEDULE_SLIP.load();
    let baseline_perceived = metrics::PERCEIVED_LATENCY.load();
```

(Match the exact form the existing baselines use — e.g. `metrics::GET_LATENCY.load()`.) Then in the delta block near `src/runner.rs:769`:

```rust
    let schedule_slip = delta_latency_stats(&metrics::SCHEDULE_SLIP, &baseline_schedule_slip);
    let perceived_latency = delta_latency_stats(&metrics::PERCEIVED_LATENCY, &baseline_perceived);
```

And add both to `Results { ... }`:

```rust
        requests_dropped,
        schedule_slip,
        perceived_latency,
```

Do not introduce a new histogram→`LatencyStats` conversion path; reuse `delta_latency_stats`.

- [ ] **Step 3: Print the sections (clean)**

In `src/output/clean.rs` `print_results`, after the existing latency section, add (matching the file's existing section style and color helpers):

```rust
        if results.schedule_slip.p99_us > 0.0 || results.requests_dropped > 0 {
            println!();
            println!("{}", self.cyan("Schedule Slip (queueing the latency clock omits)"));
            self.print_latency_row("slip", &results.schedule_slip);
            println!("{}", self.cyan("Perceived Latency (arrival-relative, CO-honest)"));
            self.print_latency_row("perceived", &results.perceived_latency);
            if results.requests_dropped > 0 {
                println!(
                    "Overload: {} of {} offered dropped ({:.1}%)",
                    results.requests_dropped,
                    results.offered(),
                    results.overload_pct(),
                );
            }
        }
```

Use the existing per-row latency printer in `clean.rs` (the one used for GET/SET rows) instead of `print_latency_row` if it has a different name — match the established helper.

- [ ] **Step 4: Print the sections (verbose)**

In `src/output/verbose.rs` `print_results`, add equivalent `tracing::info!` lines guarded by the same `schedule_slip.p99_us > 0.0 || requests_dropped > 0` condition, following the file's existing `tracing::info!` latency style.

- [ ] **Step 5: JSON fields**

In `src/output/json.rs`, add to the results record the fields `schedule_slip`, `perceived_latency` (serialized like the existing `get_latencies`/`set_latencies`), `requests_dropped`, and `offered`. Follow the existing serialization pattern in that file.

- [ ] **Step 6: Build + commit**

Run: `cargo build` (expect clean), `cargo test` (expect PASS), then:

```bash
git add src/runner.rs src/output/mod.rs src/output/clean.rs src/output/verbose.rs src/output/json.rs
git commit -m "feat(output): schedule slip, perceived latency, and overload sections"
```

---

## Task 8: Saturation — gate the SLO on perceived latency

Snapshot `PERCEIVED_LATENCY` for the SLO percentiles; keep send-relative for the informational columns.

**Files:**
- Modify: `src/saturation.rs` — baseline/step snapshots (lines ~95–140)

- [ ] **Step 1: Snapshot perceived latency at baseline**

In `check_and_advance`, the baseline capture at `src/saturation.rs:95-96`:

```rust
                self.step_responses = metrics::RESPONSES_RECEIVED.value();
                self.step_histogram = metrics::RESPONSE_LATENCY.load();
```

Add a parallel perceived snapshot. Add a field `step_perceived: Option<Histogram>` to `SaturationSearchState` (next to `step_histogram`, line 35) initialized to `None` in `new()` and in the reset block (line 211). Then at baseline:

```rust
                self.step_responses = metrics::RESPONSES_RECEIVED.value();
                self.step_histogram = metrics::RESPONSE_LATENCY.load();
                self.step_perceived = metrics::PERCEIVED_LATENCY.load();
```

- [ ] **Step 2: Compute SLO percentiles from perceived; keep send-relative for display**

At the percentile computation (`src/saturation.rs:121-140`), compute BOTH. Keep the existing `(p50, p99, p999)` from `RESPONSE_LATENCY` for the displayed columns, and add `(perc_p50, perc_p99, perc_p999)` from the perceived delta using the identical `wrapping_sub` + `percentile_from_histogram` logic against `current_perceived` / `self.step_perceived`, where:

```rust
        let current_perceived = metrics::PERCEIVED_LATENCY.load();
```

- [ ] **Step 3: Feed perceived percentiles to the SLO check**

At `src/saturation.rs:147`:

```rust
        let latency_reason = self.slo_fail_reason(p50, p99, p999);
```

change to use the perceived percentiles:

```rust
        let latency_reason = self.slo_fail_reason(perc_p50, perc_p99, perc_p999);
```

Leave `SaturationStep { p50_us: p50, p99_us: p99, p999_us: p999, ... }` populated from the send-relative values (display unchanged) but also carry perceived (added in Task 11).

- [ ] **Step 4: Build + manual check**

Run: `cargo build` (expect clean). The behavior is exercised by Task 9's tests and integration configs.

- [ ] **Step 5: Commit**

```bash
git add src/saturation.rs
git commit -m "feat(saturation): gate SLO on perceived (CO-honest) latency"
```

---

## Task 9: Saturation — bisect+drain knee refinement (testable core)

Extract a pure rate-search state machine, test it, then drive it from `check_and_advance`. This replaces the geometric-climb-until-N-failures termination.

**Files:**
- Create: `src/saturation/search.rs` is overkill — keep it inline in `src/saturation.rs`.
- Modify: `src/saturation.rs` — add `RateSearch` + tests, integrate into `check_and_advance`

- [ ] **Step 1: Write the failing tests for the search core**

Add to `src/saturation.rs` (the file already has a `#[cfg(test)]` section near line 373 — add a new module or extend it):

```rust
#[cfg(test)]
mod rate_search_tests {
    use super::{RateSearch, SearchOutcome};

    // Oracle: every rate <= `knee` passes, every rate above fails.
    fn run_to_completion(knee: u64) -> Option<u64> {
        let mut s = RateSearch::new(
            /* start_rate */ 1000,
            /* step_multiplier */ 2.0,
            /* max_rate */ 1_000_000,
            /* bisect_tolerance */ 0.05,
            /* max_bisect_steps */ 8,
        );
        let mut rate = 1000u64;
        loop {
            let passed = rate <= knee;
            match s.advance(passed) {
                SearchOutcome::Probe(next) => rate = next,
                SearchOutcome::Done { knee } => return knee,
            }
        }
    }

    #[test]
    fn converges_near_the_true_knee() {
        let found = run_to_completion(30_000).expect("a knee");
        // Within bisect_tolerance (5%) of the true knee.
        let rel = (found as f64 - 30_000.0).abs() / 30_000.0;
        assert!(rel <= 0.05, "found {found}, rel error {rel}");
        assert!(found <= 30_000, "reported knee must actually pass SLO");
    }

    #[test]
    fn stops_within_max_bisect_steps() {
        // A pathological oracle that never lets the interval close enough
        // would loop forever without the step cap. Use tolerance 0 to force
        // the cap to be the terminator.
        let mut s = RateSearch::new(1000, 2.0, 1_000_000, 0.0, 4);
        let mut rate = 1000u64;
        let mut probes = 0;
        loop {
            probes += 1;
            assert!(probes < 100, "must terminate");
            match s.advance(rate <= 12_345) {
                SearchOutcome::Probe(next) => rate = next,
                SearchOutcome::Done { .. } => break,
            }
        }
    }

    #[test]
    fn climb_to_max_rate_without_failure_reports_max() {
        // Knee above max_rate: climb never fails, terminate at the ceiling.
        let found = run_to_completion(10_000_000);
        assert_eq!(found, Some(1_000_000));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test rate_search_tests`
Expected: FAIL — `RateSearch` / `SearchOutcome` not found.

- [ ] **Step 3: Implement the search core**

Add to `src/saturation.rs` (above the existing `impl SaturationSearchState`):

```rust
/// Phase of the rate search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchPhase {
    /// Geometric climb until the first SLO failure.
    Climb,
    /// Bisecting [lo, hi] to pin the knee.
    Bisect,
}

/// What to do after measuring the current rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchOutcome {
    /// Measure this next rate.
    Probe(u64),
    /// Search finished; `knee` is the highest rate that met SLO (if any).
    Done { knee: Option<u64> },
}

/// Pure rate-search state machine: geometric climb to the first failure,
/// then bisection between the last good rate and the failed rate until the
/// interval closes within `bisect_tolerance` or `max_bisect_steps` is hit.
pub struct RateSearch {
    phase: SearchPhase,
    current_rate: u64,
    last_good: Option<u64>,
    lo: u64,
    hi: u64,
    bisect_steps: u32,
    step_multiplier: f64,
    max_rate: u64,
    bisect_tolerance: f64,
    max_bisect_steps: u32,
}

impl RateSearch {
    pub fn new(
        start_rate: u64,
        step_multiplier: f64,
        max_rate: u64,
        bisect_tolerance: f64,
        max_bisect_steps: u32,
    ) -> Self {
        Self {
            phase: SearchPhase::Climb,
            current_rate: start_rate,
            last_good: None,
            lo: 0,
            hi: 0,
            bisect_steps: 0,
            step_multiplier,
            max_rate,
            bisect_tolerance,
            max_bisect_steps,
        }
    }

    /// The rate currently being measured.
    pub fn current_rate(&self) -> u64 {
        self.current_rate
    }

    /// Record the SLO result for `current_rate` and decide the next move.
    pub fn advance(&mut self, passed: bool) -> SearchOutcome {
        if passed {
            self.last_good = Some(self.current_rate);
        }
        match self.phase {
            SearchPhase::Climb => {
                if !passed {
                    // First failure: bisect between last good and here.
                    let lo = self.last_good.unwrap_or(0);
                    self.lo = lo;
                    self.hi = self.current_rate;
                    self.phase = SearchPhase::Bisect;
                    return self.bisect_probe();
                }
                // Climb to the next rate; cap at max_rate, terminate there.
                let next = ((self.current_rate as f64) * self.step_multiplier) as u64;
                if next >= self.max_rate {
                    // Probe the ceiling once if we haven't, else done.
                    if self.current_rate >= self.max_rate {
                        return SearchOutcome::Done {
                            knee: self.last_good,
                        };
                    }
                    self.current_rate = self.max_rate;
                    return SearchOutcome::Probe(self.current_rate);
                }
                self.current_rate = next.max(self.current_rate + 1);
                SearchOutcome::Probe(self.current_rate)
            }
            SearchPhase::Bisect => {
                if passed {
                    self.lo = self.current_rate;
                } else {
                    self.hi = self.current_rate;
                }
                self.bisect_steps += 1;
                let width = if self.hi > 0 {
                    (self.hi - self.lo) as f64 / self.hi as f64
                } else {
                    0.0
                };
                if width <= self.bisect_tolerance
                    || self.bisect_steps >= self.max_bisect_steps
                    || self.hi.saturating_sub(self.lo) <= 1
                {
                    return SearchOutcome::Done {
                        knee: if self.lo > 0 { Some(self.lo) } else { self.last_good },
                    };
                }
                self.bisect_probe()
            }
        }
    }

    fn bisect_probe(&mut self) -> SearchOutcome {
        let mid = self.lo + (self.hi - self.lo) / 2;
        let mid = mid.max(self.lo + 1).min(self.hi.saturating_sub(1)).max(1);
        self.current_rate = mid;
        SearchOutcome::Probe(mid)
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test rate_search_tests`
Expected: PASS (all three).

- [ ] **Step 5: Drive `RateSearch` from the search state**

In `SaturationSearchState`, replace the consecutive-failure / geometric advance block (`src/saturation.rs:181-214` — the `if slo_passed { ... } else { ... }` through `self.ratelimiter.set_rate(next_rate)`) with:

```rust
        // Drive the rate-search state machine.
        match self.search.advance(slo_passed) {
            SearchOutcome::Done { knee } => {
                self.last_good_rate = knee;
                self.completed = true;
                return true;
            }
            SearchOutcome::Probe(next_rate) => {
                self.current_rate = next_rate;
                self.ratelimiter.set_rate(next_rate);
                metrics::TARGET_RATE.set(next_rate as i64);
            }
        }

        // Reset step tracking for the next probe (drain then sample).
        self.step_start = now;
        self.baseline_at = None;
        self.step_histogram = None;
        self.step_perceived = None;
        self.step_responses = 0;

        true
```

Add a `search: RateSearch` field to `SaturationSearchState` and build it in `new()`:

```rust
            search: RateSearch::new(
                start_rate,
                config.step_multiplier,
                config.max_rate,
                config.bisect_tolerance,
                config.max_bisect_steps,
            ),
```

(`bisect_tolerance` / `max_bisect_steps` come from Task 10's config additions — implement Task 10 first or add the fields in the same change.) Remove the now-unused `consecutive_failures` field and its references, and remove the `stop_after_failures` check at `src/saturation.rs:190-193` (superseded).

- [ ] **Step 6: Build + test**

Run: `cargo build` and `cargo test saturation` (and `rate_search_tests`).
Expected: clean build, tests PASS.

- [ ] **Step 7: Commit**

```bash
git add src/saturation.rs
git commit -m "feat(saturation): bisect+drain knee refinement via pure RateSearch core"
```

---

## Task 10: Config — bisect knobs + deprecate `stop_after_failures`

**Files:**
- Modify: `src/config/mod.rs` — `SaturationSearch` struct + default fns
- Modify: `src/saturation.rs` — deprecation warning

- [ ] **Step 1: Add the new config fields**

In `src/config/mod.rs` `struct SaturationSearch` (after `min_throughput_ratio`, line 256):

```rust
    pub min_throughput_ratio: f64,
    /// Relative interval width at which bisection stops (0.0-1.0).
    #[serde(default = "default_bisect_tolerance")]
    pub bisect_tolerance: f64,
    /// Hard cap on the number of bisection probes.
    #[serde(default = "default_max_bisect_steps")]
    pub max_bisect_steps: u32,
```

Add the default fns near the other `default_*` fns for this struct:

```rust
fn default_bisect_tolerance() -> f64 {
    0.05
}

fn default_max_bisect_steps() -> u32 {
    8
}
```

- [ ] **Step 2: Emit a deprecation warning for `stop_after_failures`**

`stop_after_failures` keeps its field + `default_stop_after_failures` (config compatibility) but no longer governs termination. In `SaturationSearchState::new()` (`src/saturation.rs:49`), after setting the initial rate, add:

```rust
        if config.stop_after_failures != default_stop_after_failures() {
            tracing::warn!(
                "saturation_search.stop_after_failures is deprecated and ignored; \
                 termination is now governed by bisect_tolerance / max_bisect_steps"
            );
        }
```

Import `default_stop_after_failures` from the config module (or compare against the known default `3` if the fn is private — prefer making the fn `pub(crate)` and importing it).

- [ ] **Step 3: Build**

Run: `cargo build`
Expected: clean.

- [ ] **Step 4: Commit**

```bash
git add src/config/mod.rs src/saturation.rs
git commit -m "feat(config): bisect_tolerance + max_bisect_steps; deprecate stop_after_failures"
```

---

## Task 11: Saturation — transition flags + perceived columns in the report

**Files:**
- Modify: `src/output/mod.rs` — extend `SaturationStep`
- Modify: `src/saturation.rs` — populate flags + perceived percentiles
- Modify: `src/output/clean.rs`, `src/output/json.rs` — render flags/columns

- [ ] **Step 1: Extend `SaturationStep`**

In `src/output/mod.rs` `struct SaturationStep` (after `slo_percentile_us`, line 223):

```rust
    pub slo_percentile_us: f64,
    /// Perceived (CO-honest) p99 latency in microseconds.
    pub perceived_p99_us: f64,
    /// Achieved rate fell vs the previous step.
    pub throughput_rollover: bool,
    /// Schedule slip first crossed a small threshold at this step.
    pub slip_onset: bool,
    /// First SLO breach occurred at this step.
    pub slo_breach: bool,
```

- [ ] **Step 2: Populate the flags in `check_and_advance`**

In `src/saturation.rs`, track previous-step achieved rate plus `slip_seen`/`breach_seen` bools on `SaturationSearchState` (fields `prev_achieved: f64` default 0.0, `slip_seen: bool` default false, `breach_seen: bool` default false). When building `SaturationStep` (`src/saturation.rs:165`):

```rust
        let slip_p99_us = percentile_from_histogram(
            &metrics::SCHEDULE_SLIP.load().unwrap_or_default(),
            0.99,
        ) / 1000.0; // ns→µs, matching existing percentile units
        let throughput_rollover = self.prev_achieved > 0.0 && achieved_rate < self.prev_achieved;
        let slip_onset = !self.slip_seen && slip_p99_us > 1000.0; // first >1ms slip
        let slo_breach = !slo_passed && !self.breach_seen; // first SLO breach only
```

Set the step fields:

```rust
            perceived_p99_us: perc_p99,
            throughput_rollover,
            slip_onset,
            slo_breach,
```

After recording the step, update state: `self.prev_achieved = achieved_rate;`, `if slip_p99_us > 1000.0 { self.slip_seen = true; }`, and `if !slo_passed { self.breach_seen = true; }`. Confirm `percentile_from_histogram` returns the unit the rest of the file uses (the existing p50/p99 are already in µs — match that exact conversion; if the existing helper already returns µs, drop the `/1000.0`).

- [ ] **Step 3: Render flags (clean) + columns**

In `src/output/clean.rs` `print_saturation_step`, append flag markers to the row when set, e.g. ` [rollover]`, ` [slip↑]`, ` [SLO breach]`, and add a perceived-p99 column next to the send-relative p99. Match the existing column layout/colors in that function.

- [ ] **Step 4: JSON**

In `src/output/json.rs` saturation-step serialization, add `perceived_p99_us`, `throughput_rollover`, `slip_onset`, `slo_breach`.

- [ ] **Step 5: Build + test + commit**

Run: `cargo build` (clean), `cargo test` (PASS), then:

```bash
git add src/output/mod.rs src/saturation.rs src/output/clean.rs src/output/json.rs
git commit -m "feat(saturation): transition flags + perceived p99 column in report"
```

---

## Task 12: Viewer — slip & perceived latency plots

**Files:**
- Modify: `src/viewer/dashboard/latency.rs`

- [ ] **Step 1: Add the plots**

In `src/viewer/dashboard/latency.rs`, after the existing DELETE plot (line 36) and before `view.group(latency);`:

```rust
    // Schedule slip (queueing the latency clock omits)
    latency.plot_promql(
        PlotOpts::scatter("Schedule Slip", "schedule-slip-pct", Unit::Time).with_log_scale(true),
        "histogram_percentiles([0.5, 0.9, 0.99, 0.999, 0.9999], schedule_slip)".to_string(),
    );

    // Perceived (arrival-relative, CO-honest) latency
    latency.plot_promql(
        PlotOpts::scatter("Perceived", "perceived-latency-pct", Unit::Time).with_log_scale(true),
        "histogram_percentiles([0.5, 0.9, 0.99, 0.999, 0.9999], perceived_latency)".to_string(),
    );
```

- [ ] **Step 2: Build**

Run: `cargo build`
Expected: clean.

- [ ] **Step 3: Commit**

```bash
git add src/viewer/dashboard/latency.rs
git commit -m "feat(viewer): schedule slip + perceived latency plots"
```

---

## Final verification

- [ ] `cargo build` — clean
- [ ] `cargo clippy` — no new warnings in touched files
- [ ] `cargo test` — all unit tests pass (`co_tests`, `overload_tests`, `rate_search_tests`, existing saturation tests)
- [ ] `cargo xtask fmt` — formatting clean
- [ ] Integration smoke (requires a target server): run `config/saturation.toml` and confirm the report shows Schedule Slip + Perceived Latency sections and a bisect-refined final rate; run a plain `config/redis.toml` with no `rate_limit` and confirm slip/perceived sections are suppressed and latency numbers are unchanged from before.

---

## Spec coverage check

- Slip metric from `available()/rate` → Tasks 1–5. ✔
- Perceived latency (global recv-time gauge estimate) → Tasks 3–5. ✔
- Existing latency metrics unchanged → Tasks 3–5 only add. ✔
- Overload/drop via `dropped()` → Task 6. ✔
- SLO gated on perceived latency → Task 8. ✔
- Bisect+drain with `bisect_tolerance` AND `max_bisect_steps` → Tasks 9–10. ✔
- Deprecate `stop_after_failures` → Task 10. ✔
- Transition flags → Task 11. ✔
- Output sections (clean/verbose/json) → Tasks 7, 11. ✔
- Viewer plots → Task 12. ✔
- Suppression on closed-loop runs → Tasks 7, 1 (slip≡0). ✔
- Tests (slip math, overload, bisect) → Tasks 1, 6, 9. ✔
