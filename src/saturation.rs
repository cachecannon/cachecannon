//! Saturation search state management.
//!
//! This module implements the saturation search algorithm that finds the
//! maximum throughput while maintaining SLO compliance.

use crate::config::SaturationSearch;
use crate::metrics;
use crate::output::{OutputFormatter, SaturationResults, SaturationStep};
use ratelimit::Ratelimiter;

use metriken::histogram::Histogram;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// State machine for saturation search.
pub struct SaturationSearchState {
    /// Configuration for the search.
    config: SaturationSearch,
    /// Dynamic rate limiter (shared with workers).
    ratelimiter: Arc<Ratelimiter>,

    /// Current target rate.
    current_rate: u64,
    /// Last rate that met SLO.
    last_good_rate: Option<u64>,
    /// Rate-search state machine (climb + bisect).
    search: RateSearch,
    /// When the current step started (i.e. when the new rate was applied).
    step_start: Instant,
    /// When the measurement baseline was captured (set once the drain window
    /// has elapsed). `None` means we are still draining old-rate in-flight
    /// requests and have not yet snapshotted the histogram/response baseline.
    baseline_at: Option<Instant>,
    /// Histogram snapshot at baseline capture (for delta calculation).
    step_histogram: Option<Histogram>,
    /// Perceived-latency histogram snapshot at baseline capture.
    step_perceived: Option<Histogram>,
    /// Schedule-slip histogram snapshot at baseline capture.
    step_slip: Option<Histogram>,
    /// Response count at baseline capture.
    step_responses: u64,
    /// Achieved rate of the previous recorded step (0.0 before the first).
    prev_achieved: f64,
    /// Whether schedule slip has crossed the onset threshold yet.
    slip_seen: bool,
    /// Whether an SLO breach has occurred yet.
    breach_seen: bool,

    /// All step results.
    results: Vec<SaturationStep>,
    /// Whether the search has completed.
    completed: bool,
    /// Whether we've printed the header yet.
    header_printed: bool,
}

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
    /// Extra consecutive failures needed before a failure is acted on.
    confirm_failures: u32,
    /// Consecutive failures observed at `current_rate` so far.
    failures_at_rate: u32,
    /// Minimum fractional latency improvement expected from halving the rate.
    floor_improvement_ratio: f64,
    /// Rate and SLO-percentile latency of the last confirmed failure, used to
    /// tell a load-bound target from one with a floor above the SLO.
    last_confirmed_fail: Option<(u64, f64)>,
    /// Set when the search stopped because latency stopped responding to rate.
    floor_detected: bool,
}

impl RateSearch {
    pub fn new(
        start_rate: u64,
        step_multiplier: f64,
        max_rate: u64,
        bisect_tolerance: f64,
        max_bisect_steps: u32,
        confirm_failures: u32,
        floor_improvement_ratio: f64,
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
            confirm_failures,
            failures_at_rate: 0,
            floor_improvement_ratio,
            last_confirmed_fail: None,
            floor_detected: false,
        }
    }

    /// Whether the search ended because latency stopped responding to rate,
    /// rather than because it ran out of bisection steps.
    pub fn floor_detected(&self) -> bool {
        self.floor_detected
    }

    /// Record the SLO result for `current_rate` and decide the next move.
    ///
    /// A failure is not acted on until it has repeated `confirm_failures` more
    /// times at the same rate. Without that, one transient sample permanently
    /// caps the search: a climb failure fixes the bisection ceiling and no
    /// higher rate is ever retried, so the run converges tidily on a knee that
    /// is far too low and reports it as success. A retry that passes clears the
    /// count and the rate is treated as passing.
    pub fn advance(&mut self, passed: bool, slo_latency_us: f64) -> SearchOutcome {
        if passed {
            self.failures_at_rate = 0;
            self.last_good = Some(self.current_rate);
            self.last_confirmed_fail = None;
        } else {
            self.failures_at_rate += 1;
            if self.failures_at_rate <= self.confirm_failures {
                // Re-measure the same rate; leave phase, lo and hi untouched.
                return SearchOutcome::Probe(self.current_rate);
            }
            self.failures_at_rate = 0;

            // Latency-floor detection. Only meaningful while NO rate has passed:
            // once `last_good` exists the bisection is narrowing a real interval
            // and must run to its tolerance. Before that, the search is hunting
            // downward for any compliant rate, which is productive only if
            // latency responds to rate at all. If halving the rate does not
            // improve the SLO percentile materially, the target has a floor
            // above the SLO and no rate will pass.
            if self.last_good.is_none()
                && self.floor_improvement_ratio > 0.0
                && slo_latency_us.is_finite()
                && slo_latency_us > 0.0
            {
                if let Some((prev_rate, prev_us)) = self.last_confirmed_fail
                    && prev_rate >= self.current_rate.saturating_mul(2)
                    && prev_us > 0.0
                {
                    let improvement = (prev_us - slo_latency_us) / prev_us;
                    if improvement < self.floor_improvement_ratio {
                        self.floor_detected = true;
                        return SearchOutcome::Done { knee: None };
                    }
                }
                self.last_confirmed_fail = Some((self.current_rate, slo_latency_us));
            }
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
                        knee: if self.lo > 0 {
                            Some(self.lo)
                        } else {
                            self.last_good
                        },
                    };
                }
                self.bisect_probe()
            }
        }
    }

    /// Whether the search is still in the geometric climb phase.
    pub fn is_climbing(&self) -> bool {
        self.phase == SearchPhase::Climb
    }

    fn bisect_probe(&mut self) -> SearchOutcome {
        let mid = self.lo + (self.hi - self.lo) / 2;
        let mid = mid.max(self.lo + 1).min(self.hi.saturating_sub(1)).max(1);
        self.current_rate = mid;
        SearchOutcome::Probe(mid)
    }
}

impl SaturationSearchState {
    /// Create a new saturation search state.
    pub fn new(config: SaturationSearch, ratelimiter: Arc<Ratelimiter>) -> Self {
        let start_rate = config.start_rate;

        // Set initial rate
        ratelimiter.set_rate(start_rate);
        metrics::TARGET_RATE.set(start_rate as i64);

        if config.stop_after_failures != crate::config::default_stop_after_failures() {
            tracing::warn!(
                "saturation_search.stop_after_failures is deprecated and ignored; \
                 termination is now governed by bisect_tolerance / max_bisect_steps"
            );
        }

        Self {
            config: config.clone(),
            ratelimiter,
            current_rate: start_rate,
            last_good_rate: None,
            search: RateSearch::new(
                start_rate,
                config.step_multiplier,
                config.max_rate,
                config.bisect_tolerance,
                config.max_bisect_steps,
                config.confirm_failures,
                config.floor_improvement_ratio,
            ),
            step_start: Instant::now(),
            baseline_at: None,
            step_histogram: None,
            step_perceived: None,
            step_slip: None,
            step_responses: 0,
            prev_achieved: 0.0,
            slip_seen: false,
            breach_seen: false,
            results: Vec::new(),
            completed: false,
            header_printed: false,
        }
    }

    /// Check if it's time to advance to the next step, and do so if needed.
    ///
    /// Returns `true` if a step was completed (and printed).
    pub fn check_and_advance(&mut self, formatter: &dyn OutputFormatter) -> bool {
        if self.completed {
            return false;
        }

        // Snapshot the clock once and use it for both the window check and the
        // rate denominator. Reading elapsed() twice would let slowness between
        // the two reads (e.g. histogram math) inflate the denominator past the
        // window the response count was sampled against.
        let now = Instant::now();

        // Drain phase: after a rate change, requests at the previous rate are
        // still in flight. Wait `drain_window` before capturing the baseline
        // so their responses don't bias the new step's measurements.
        let baseline_at = match self.baseline_at {
            Some(t) => t,
            None => {
                if now.saturating_duration_since(self.step_start) < self.config.drain_window {
                    return false;
                }
                self.step_responses = metrics::RESPONSES_RECEIVED.value();
                self.step_histogram = metrics::RESPONSE_LATENCY.load();
                self.step_perceived = metrics::PERCEIVED_LATENCY.load();
                self.step_slip = metrics::SCHEDULE_SLIP.load();
                self.baseline_at = Some(now);
                return false;
            }
        };

        let elapsed = now.saturating_duration_since(baseline_at);
        if elapsed < self.config.sample_window {
            return false;
        }

        // Print header on first step
        if !self.header_printed {
            formatter.print_saturation_header();
            self.header_printed = true;
        }

        // Calculate delta histogram for this step
        let current_histogram = metrics::RESPONSE_LATENCY.load();
        let current_perceived = metrics::PERCEIVED_LATENCY.load();
        let current_responses = metrics::RESPONSES_RECEIVED.value();

        let delta_responses = current_responses.saturating_sub(self.step_responses);
        let elapsed_secs = elapsed.as_secs_f64();
        let achieved_rate = delta_responses as f64 / elapsed_secs;

        // Get percentiles from delta histogram
        let (p50, p99, p999) = match (&current_histogram, &self.step_histogram) {
            (Some(current), Some(previous)) => match current.wrapping_sub(previous) {
                Ok(delta) => (
                    percentile_from_histogram(&delta, 0.50),
                    percentile_from_histogram(&delta, 0.99),
                    percentile_from_histogram(&delta, 0.999),
                ),
                Err(e) => {
                    tracing::warn!("histogram delta computation failed: {e}");
                    (0.0, 0.0, 0.0)
                }
            },
            (Some(current), None) => (
                percentile_from_histogram(current, 0.50),
                percentile_from_histogram(current, 0.99),
                percentile_from_histogram(current, 0.999),
            ),
            _ => (0.0, 0.0, 0.0),
        };

        // Get percentiles from perceived (CO-honest) delta histogram
        let (perc_p50, perc_p99, perc_p999) = match (&current_perceived, &self.step_perceived) {
            (Some(current), Some(previous)) => match current.wrapping_sub(previous) {
                Ok(delta) => (
                    percentile_from_histogram(&delta, 0.50),
                    percentile_from_histogram(&delta, 0.99),
                    percentile_from_histogram(&delta, 0.999),
                ),
                Err(e) => {
                    tracing::warn!("histogram delta computation failed: {e}");
                    (0.0, 0.0, 0.0)
                }
            },
            (Some(current), None) => (
                percentile_from_histogram(current, 0.50),
                percentile_from_histogram(current, 0.99),
                percentile_from_histogram(current, 0.999),
            ),
            _ => (0.0, 0.0, 0.0),
        };

        // Check throughput ratio (detect saturation)
        let throughput_ratio = achieved_rate / self.current_rate as f64;
        let throughput_ok = throughput_ratio >= self.config.min_throughput_ratio;

        // Check SLO compliance (latency + throughput)
        let latency_reason = self.slo_fail_reason(perc_p50, perc_p99, perc_p999);
        let slo_passed = throughput_ok && latency_reason.is_none();

        // Build failure reason
        let fail_reason = if slo_passed {
            String::new()
        } else if !throughput_ok {
            format!(
                "Throughput: {:.0}% (need {:.0}%)",
                throughput_ratio * 100.0,
                self.config.min_throughput_ratio * 100.0
            )
        } else {
            latency_reason.unwrap_or_default()
        };

        // Per-step schedule-slip p99 (delta over the step window), in µs.
        let current_slip = metrics::SCHEDULE_SLIP.load();
        let slip_p99_us = match (&current_slip, &self.step_slip) {
            (Some(current), Some(previous)) => match current.wrapping_sub(previous) {
                Ok(delta) => percentile_from_histogram(&delta, 0.99),
                Err(_) => 0.0,
            },
            (Some(current), None) => percentile_from_histogram(current, 0.99),
            _ => 0.0,
        };
        let throughput_rollover = self.search.is_climbing()
            && self.prev_achieved > 0.0
            && achieved_rate < self.prev_achieved;
        let slip_onset = !self.slip_seen && slip_p99_us > 1000.0; // first >1ms slip
        let slo_breach = !slo_passed && !self.breach_seen; // first SLO breach only
        let (_, perceived_slo_us) = self.slo_percentile(perc_p50, perc_p99, perc_p999);

        // Record step
        let (slo_percentile_label, slo_percentile_us) = self.slo_percentile(p50, p99, p999);
        let step = SaturationStep {
            target_rate: self.current_rate,
            achieved_rate,
            p50_us: p50,
            p99_us: p99,
            p999_us: p999,
            slo_passed,
            fail_reason,
            slo_display: self.slo_display(),
            slo_threshold_us: self.slo_threshold_us(),
            slo_percentile_label,
            slo_percentile_us,
            perceived_slo_us,
            throughput_rollover,
            slip_onset,
            slo_breach,
        };
        formatter.print_saturation_step(&step);
        self.results.push(step);

        self.prev_achieved = achieved_rate;
        if slip_p99_us > 1000.0 {
            self.slip_seen = true;
        }
        if !slo_passed {
            self.breach_seen = true;
        }

        // Drive the rate-search state machine.
        match self.search.advance(slo_passed, slo_percentile_us) {
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

        // Reset step tracking. The new rate just took effect; the baseline
        // will be captured after `drain_window` has elapsed so old-rate
        // in-flight responses don't bias the new step.
        self.step_start = now;
        self.baseline_at = None;
        self.step_histogram = None;
        self.step_perceived = None;
        self.step_slip = None;
        self.step_responses = 0;

        true
    }

    /// Return the reason the SLO failed, or None if it passed.
    fn slo_fail_reason(&self, p50_us: f64, p99_us: f64, p999_us: f64) -> Option<String> {
        let slo = &self.config.slo;

        if let Some(threshold) = slo.p50 {
            let threshold_us = threshold.as_micros() as f64;
            if p50_us > threshold_us {
                return Some(format!(
                    "Latency: p50 {:.0}us > {:.0}us SLO",
                    p50_us, threshold_us
                ));
            }
        }

        if let Some(threshold) = slo.p99 {
            let threshold_us = threshold.as_micros() as f64;
            if p99_us > threshold_us {
                return Some(format!(
                    "Latency: p99 {:.0}us > {:.0}us SLO",
                    p99_us, threshold_us
                ));
            }
        }

        if let Some(threshold) = slo.p999 {
            let threshold_us = threshold.as_micros() as f64;
            if p999_us > threshold_us {
                return Some(format!(
                    "Latency: p999 {:.0}us > {:.0}us SLO",
                    p999_us, threshold_us
                ));
            }
        }

        None
    }

    /// Build a display string for the configured SLO (e.g. "p999 ≤ 1ms").
    fn slo_display(&self) -> String {
        let slo = &self.config.slo;
        // Show the highest percentile SLO configured
        if let Some(threshold) = slo.p999 {
            format!("p999 \u{2264} {}", format_duration_short(threshold))
        } else if let Some(threshold) = slo.p99 {
            format!("p99 \u{2264} {}", format_duration_short(threshold))
        } else if let Some(threshold) = slo.p50 {
            format!("p50 \u{2264} {}", format_duration_short(threshold))
        } else {
            String::new()
        }
    }

    /// Get the SLO threshold in microseconds (highest configured percentile).
    fn slo_threshold_us(&self) -> Option<f64> {
        let slo = &self.config.slo;
        slo.p999
            .or(slo.p99)
            .or(slo.p50)
            .map(|t| t.as_micros() as f64)
    }

    /// Pick the percentile (label + measured value) that corresponds to the
    /// SLO's highest configured percentile, matching `slo_display`. Falls back
    /// to p999 when no SLO is configured.
    fn slo_percentile(&self, p50_us: f64, p99_us: f64, p999_us: f64) -> (&'static str, f64) {
        let slo = &self.config.slo;
        if slo.p999.is_some() {
            ("p999", p999_us)
        } else if slo.p99.is_some() {
            ("p99", p99_us)
        } else if slo.p50.is_some() {
            ("p50", p50_us)
        } else {
            ("p999", p999_us)
        }
    }

    /// Whether the search has completed.
    pub fn is_completed(&self) -> bool {
        self.completed
    }

    /// Get the sample window duration.
    pub fn sample_window(&self) -> Duration {
        self.config.sample_window
    }

    /// Get the final results.
    pub fn results(&self) -> SaturationResults {
        SaturationResults {
            max_compliant_rate: self.last_good_rate,
            steps: self.results.clone(),
            latency_floor: self.search.floor_detected(),
        }
    }
}

/// Format a Duration as a compact human-readable string (e.g. "1ms", "1.5ms", "500us").
fn format_duration_short(d: Duration) -> String {
    let us = d.as_micros();
    if us >= 1_000_000 {
        if us.is_multiple_of(1_000_000) {
            format!("{}s", us / 1_000_000)
        } else {
            format!("{:.1}s", us as f64 / 1_000_000.0)
        }
    } else if us >= 1_000 {
        if us.is_multiple_of(1_000) {
            format!("{}ms", us / 1_000)
        } else {
            format!("{:.1}ms", us as f64 / 1_000.0)
        }
    } else {
        format!("{}us", us)
    }
}

/// Get a percentile from a histogram snapshot (in microseconds).
fn percentile_from_histogram(hist: &Histogram, p: f64) -> f64 {
    match hist.quantiles(&[p]) {
        Ok(Some(results)) => {
            if let Some(bucket) = results.entries().values().next() {
                // Histogram stores nanoseconds, convert to microseconds
                return bucket.end() as f64 / 1000.0;
            }
        }
        Err(e) => {
            tracing::warn!("histogram percentile computation failed: {e}");
        }
        Ok(None) => {}
    }
    0.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SloThresholds;

    #[test]
    fn test_slo_check() {
        let config = SaturationSearch {
            slo: SloThresholds {
                p50: Some(Duration::from_micros(100)),
                p99: None,
                p999: Some(Duration::from_millis(1)),
            },
            start_rate: 1000,
            step_multiplier: 1.05,
            sample_window: Duration::from_secs(5),
            drain_window: Duration::from_millis(500),
            stop_after_failures: 3,
            confirm_failures: 0,
            floor_improvement_ratio: 0.0,
            max_rate: 100_000_000,
            min_throughput_ratio: 0.9,
            bisect_tolerance: 0.05,
            max_bisect_steps: 8,
        };

        let rl = Arc::new(
            Ratelimiter::builder(1000)
                .initial_available(1000)
                .build()
                .unwrap(),
        );
        let state = SaturationSearchState::new(config, rl);

        // Under thresholds - should pass
        assert!(state.slo_fail_reason(50.0, 500.0, 800.0).is_none());

        // p50 over threshold - should fail
        assert!(state.slo_fail_reason(150.0, 500.0, 800.0).is_some());

        // p999 over threshold - should fail
        assert!(state.slo_fail_reason(50.0, 500.0, 1500.0).is_some());
    }

    #[test]
    fn test_format_duration_short() {
        // Exact boundaries
        assert_eq!(format_duration_short(Duration::from_micros(500)), "500us");
        assert_eq!(format_duration_short(Duration::from_millis(1)), "1ms");
        assert_eq!(format_duration_short(Duration::from_secs(1)), "1s");

        // Sub-unit precision preserved
        assert_eq!(format_duration_short(Duration::from_micros(1500)), "1.5ms");
        assert_eq!(
            format_duration_short(Duration::from_micros(1_500_000)),
            "1.5s"
        );
    }
}

#[cfg(test)]
mod rate_search_tests {
    use super::{RateSearch, SearchOutcome};

    // Oracle: every rate <= `knee` passes, every rate above fails.
    fn run_to_completion(knee: u64) -> Option<u64> {
        run_to_completion_with(knee, 0)
    }

    fn run_to_completion_with(knee: u64, confirm_failures: u32) -> Option<u64> {
        let mut s = RateSearch::new(
            /* start_rate */ 1000,
            /* step_multiplier */ 2.0,
            /* max_rate */ 1_000_000,
            /* bisect_tolerance */ 0.05,
            /* max_bisect_steps */ 8,
            confirm_failures,
            /* floor */ 0.0,
        );
        let mut rate = 1000u64;
        loop {
            let passed = rate <= knee;
            match s.advance(passed, 1000.0) {
                SearchOutcome::Probe(next) => rate = next,
                SearchOutcome::Done { knee } => return knee,
            }
        }
    }

    #[test]
    fn converges_near_the_true_knee() {
        let found = run_to_completion(30_000).expect("a knee");
        let rel = (found as f64 - 30_000.0).abs() / 30_000.0;
        assert!(rel <= 0.05, "found {found}, rel error {rel}");
        assert!(found <= 30_000, "reported knee must actually pass SLO");
    }

    #[test]
    fn stops_within_max_bisect_steps() {
        let mut s = RateSearch::new(1000, 2.0, 1_000_000, 0.0, 4, 0, 0.0);
        let mut rate = 1000u64;
        let mut probes = 0;
        loop {
            probes += 1;
            assert!(probes < 100, "must terminate");
            match s.advance(rate <= 12_345, 1000.0) {
                SearchOutcome::Probe(next) => rate = next,
                SearchOutcome::Done { .. } => break,
            }
        }
    }

    #[test]
    fn climb_to_max_rate_without_failure_reports_max() {
        let found = run_to_completion(10_000_000);
        assert_eq!(found, Some(1_000_000));
    }

    #[test]
    fn knee_invariant_holds_across_a_sweep() {
        // For any knee — below start, in range, above max, unreachable — the
        // reported knee (if any) must be a rate that actually passed (<= knee),
        // and the search must terminate quickly.
        for &knee in &[0u64, 500, 1000, 1500, 7777, 30_000, 999_999, 5_000_000] {
            let mut s = RateSearch::new(1000, 2.0, 1_000_000, 0.05, 8, 0, 0.0);
            let mut rate = 1000u64;
            let mut probes = 0;
            let found = loop {
                probes += 1;
                assert!(probes < 200, "must terminate for knee={knee}");
                match s.advance(rate <= knee, 1000.0) {
                    SearchOutcome::Probe(next) => rate = next,
                    SearchOutcome::Done { knee: k } => break k,
                }
            };
            if let Some(k) = found {
                assert!(
                    k <= knee,
                    "reported knee {k} did not pass for true knee {knee}"
                );
            }
        }
    }

    #[test]
    fn everything_fails_reports_no_knee() {
        // start_rate already fails the SLO (knee below start): no compliant rate.
        let found = run_to_completion(0);
        assert_eq!(found, None);
    }

    #[test]
    fn zero_tolerance_and_zero_max_steps_still_terminate() {
        // tolerance 0 forces termination via max_bisect_steps / hi-lo<=1.
        let mut s = RateSearch::new(1000, 2.0, 1_000_000, 0.0, 0, 0, 0.0);
        let mut rate = 1000u64;
        let mut probes = 0;
        loop {
            probes += 1;
            assert!(probes < 200, "must terminate with zero knobs");
            match s.advance(rate <= 12_345, 1000.0) {
                SearchOutcome::Probe(next) => rate = next,
                SearchOutcome::Done { .. } => break,
            }
        }
    }

    /// The regression guard for the reads-run failure this change fixes.
    ///
    /// Oracle: the true knee is 50k, but the FIRST sample at the start rate
    /// reports a spurious failure (a post-prefill writeback burst, a step-onset
    /// herd -- the cause does not matter). Without confirmation the search
    /// treats 1000 as the ceiling and bisects below it, converging tidily on a
    /// knee ~50x too low and reporting it as a clean result. That is exactly
    /// what happened on a real run: three different modes returned byte
    /// identical numbers because all three bisected from the same bad first
    /// sample.
    #[test]
    fn a_transient_first_failure_does_not_cap_the_search() {
        fn run(confirm_failures: u32) -> Option<u64> {
            let mut s = RateSearch::new(1000, 2.0, 1_000_000, 0.05, 8, confirm_failures, 0.0);
            let mut rate = 1000u64;
            let mut first_sample = true;
            loop {
                // One transient failure, then the honest oracle forever after.
                let passed = if first_sample {
                    first_sample = false;
                    false
                } else {
                    rate <= 50_000
                };
                match s.advance(passed, 1000.0) {
                    SearchOutcome::Probe(next) => rate = next,
                    SearchOutcome::Done { knee } => return knee,
                }
            }
        }

        let without = run(0).expect("baseline still reports some knee");
        assert!(
            without < 1000,
            "expected the old behavior to be capped below the start rate, got {without}"
        );

        let with = run(1).expect("confirmed search must find a knee");
        let rel = (with as f64 - 50_000.0).abs() / 50_000.0;
        assert!(
            rel <= 0.05,
            "confirmed search should recover the true knee ~50k, got {with}"
        );
    }

    /// A real failure must still be acted on -- confirmation must not turn a
    /// genuine ceiling into an endless retry loop.
    #[test]
    fn persistent_failures_still_converge_with_confirmation() {
        for knee in [5_000u64, 50_000, 250_000] {
            let found = run_to_completion_with(knee, 1).expect("must find a knee");
            let rel = (found as f64 - knee as f64).abs() / knee as f64;
            assert!(rel <= 0.05, "knee {knee}: found {found}, rel error {rel}");
            assert!(found <= knee, "reported knee must actually pass SLO");
        }
    }

    /// confirm_failures = 0 preserves the previous semantics exactly.
    #[test]
    fn zero_confirmations_matches_legacy_behavior() {
        for knee in [5_000u64, 50_000, 250_000] {
            assert_eq!(run_to_completion(knee), run_to_completion_with(knee, 0));
        }
    }

    /// Each rate gets its own confirmation budget: a failure at one rate must
    /// not leave a partial count that makes the next rate fail early.
    #[test]
    fn confirmation_budget_resets_per_rate() {
        let mut s = RateSearch::new(1000, 2.0, 1_000_000, 0.05, 8, 1, 0.0);
        // Fail once at the start rate -- should re-probe the SAME rate.
        assert_eq!(s.advance(false, 1000.0), SearchOutcome::Probe(1000));
        // Then pass; the search should climb rather than stay put.
        match s.advance(true, 1000.0) {
            SearchOutcome::Probe(next) => assert!(next > 1000, "should climb, got {next}"),
            other => panic!("expected a climb, got {other:?}"),
        }
    }

    /// The regression guard for the wasted-bisection case.
    ///
    /// Oracle modelled on a real run: the target has a latency FLOOR above the
    /// SLO, so no rate passes and reducing the rate does not help. Measured on
    /// a cache whose working set was 227% of RAM, p50 went 668us at 5000 req/s
    /// to 889us at 19 req/s -- a 263x rate reduction made latency WORSE. The
    /// old search burned all 8 bisect steps (16 windows with confirmation, ~19
    /// minutes) proving what the third window already showed.
    #[test]
    fn a_latency_floor_stops_the_search_early() {
        // Latency is rate-independent: slightly worse as rate drops, as observed.
        fn latency_for(rate: u64) -> f64 {
            900.0 - (rate as f64).min(5000.0) * 0.04
        }
        fn run(floor_ratio: f64) -> (usize, Option<u64>, bool) {
            let mut s = RateSearch::new(5000, 2.0, 1_000_000, 0.05, 8, 0, floor_ratio);
            let mut rate = 5000u64;
            let mut probes = 0usize;
            loop {
                probes += 1;
                // Nothing ever passes.
                match s.advance(false, latency_for(rate)) {
                    SearchOutcome::Probe(next) => rate = next,
                    SearchOutcome::Done { knee } => return (probes, knee, s.floor_detected()),
                }
            }
        }

        let (probes_off, knee_off, floor_off) = run(0.0);
        assert_eq!(knee_off, None);
        assert!(!floor_off);
        assert!(
            probes_off >= 9,
            "with detection off the search should bisect to its step limit, took {probes_off}"
        );

        let (probes_on, knee_on, floor_on) = run(0.10);
        assert_eq!(knee_on, None, "a floor means no compliant rate");
        assert!(
            floor_on,
            "the floor should be reported, not just 'never met'"
        );
        assert!(
            probes_on <= 4,
            "detection should stop within a few probes, took {probes_on}"
        );
    }

    /// A genuinely load-bound target must still be bisected properly: latency
    /// that responds to rate is exactly the case the search exists for, and
    /// stopping early there would report no knee where one exists.
    #[test]
    fn floor_detection_does_not_fire_on_a_load_bound_target() {
        // Latency falls steeply with rate; the true knee is 12_345.
        let mut s = RateSearch::new(1000, 2.0, 1_000_000, 0.05, 8, 0, 0.10);
        let mut rate = 1000u64;
        let found = loop {
            let passed = rate <= 12_345;
            // Well under SLO when passing, and improving fast as rate drops.
            let latency = 100.0 + rate as f64 / 50.0;
            match s.advance(passed, latency) {
                SearchOutcome::Probe(next) => rate = next,
                SearchOutcome::Done { knee } => break knee,
            }
        };
        assert!(
            !s.floor_detected(),
            "load-bound target misreported as a floor"
        );
        let found = found.expect("a load-bound target has a knee");
        let rel = (found as f64 - 12_345.0).abs() / 12_345.0;
        assert!(rel <= 0.05, "found {found}, rel error {rel}");
    }

    /// Once a rate has passed, the bisection is narrowing a real interval and
    /// must run to tolerance -- floor detection must not cut that short.
    #[test]
    fn floor_detection_is_inert_once_a_rate_has_passed() {
        let mut s = RateSearch::new(1000, 2.0, 1_000_000, 0.05, 8, 0, 0.10);
        // Pass once so last_good is set, then fail with flat latency forever.
        assert!(matches!(s.advance(true, 100.0), SearchOutcome::Probe(_)));
        for _ in 0..8 {
            if let SearchOutcome::Done { knee } = s.advance(false, 900.0) {
                assert!(!s.floor_detected(), "floor fired despite a known-good rate");
                assert!(knee.is_some(), "a rate passed, so a knee must be reported");
                return;
            }
        }
        panic!("search did not terminate");
    }
}
