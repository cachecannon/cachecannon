//! Saturation search state management.
//!
//! This module implements the saturation search algorithm that finds the
//! maximum throughput while maintaining SLO compliance.

use crate::config::SaturationSearch;
use crate::metrics;
use crate::output::{OutputFormatter, SaturationResults, SaturationStep};
use crate::ratelimit::DynamicRateLimiter;

use metriken::histogram::Histogram;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// State machine for saturation search.
pub struct SaturationSearchState {
    /// Configuration for the search.
    config: SaturationSearch,
    /// Dynamic rate limiter (shared with workers).
    ratelimiter: Arc<DynamicRateLimiter>,

    /// Current target rate.
    current_rate: u64,
    /// Last rate that met SLO.
    last_good_rate: Option<u64>,
    /// Consecutive SLO failures.
    consecutive_failures: u32,
    /// When the current step started.
    step_start: Instant,
    /// Histogram snapshot at step start (for delta calculation).
    step_histogram: Option<Histogram>,
    /// Response count at step start.
    step_responses: u64,

    /// All step results.
    results: Vec<SaturationStep>,
    /// Whether the search has completed.
    completed: bool,
    /// Whether we've printed the header yet.
    header_printed: bool,
}

impl SaturationSearchState {
    /// Create a new saturation search state.
    pub fn new(config: SaturationSearch, ratelimiter: Arc<DynamicRateLimiter>) -> Self {
        let start_rate = config.start_rate;

        // Set initial rate
        ratelimiter.set_rate(start_rate);
        metrics::TARGET_RATE.set(start_rate as i64);

        Self {
            config,
            ratelimiter,
            current_rate: start_rate,
            last_good_rate: None,
            consecutive_failures: 0,
            step_start: Instant::now(),
            step_histogram: metrics::RESPONSE_LATENCY.load(),
            step_responses: metrics::RESPONSES_RECEIVED.value(),
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

        // Check if sample window has elapsed
        if self.step_start.elapsed() < self.config.sample_window {
            return false;
        }

        // Print header on first step
        if !self.header_printed {
            formatter.print_saturation_header();
            self.header_printed = true;
        }

        // Calculate delta histogram for this step
        let current_histogram = metrics::RESPONSE_LATENCY.load();
        let current_responses = metrics::RESPONSES_RECEIVED.value();

        let delta_responses = current_responses.saturating_sub(self.step_responses);
        let elapsed_secs = self.step_start.elapsed().as_secs_f64();
        let achieved_rate = delta_responses as f64 / elapsed_secs;

        // Get percentiles from delta histogram
        let (p50, p99, p999) = match (&current_histogram, &self.step_histogram) {
            (Some(current), Some(previous)) => {
                if let Ok(delta) = current.wrapping_sub(previous) {
                    (
                        percentile_from_histogram(&delta, 50.0),
                        percentile_from_histogram(&delta, 99.0),
                        percentile_from_histogram(&delta, 99.9),
                    )
                } else {
                    (0.0, 0.0, 0.0)
                }
            }
            (Some(current), None) => (
                percentile_from_histogram(current, 50.0),
                percentile_from_histogram(current, 99.0),
                percentile_from_histogram(current, 99.9),
            ),
            _ => (0.0, 0.0, 0.0),
        };

        // Check throughput ratio (detect saturation)
        let throughput_ratio = achieved_rate / self.current_rate as f64;
        let throughput_ok = throughput_ratio >= self.config.min_throughput_ratio;

        // Check SLO compliance (latency + throughput)
        let slo_passed = throughput_ok && self.check_slo(p50, p99, p999);

        // Record step
        let step = SaturationStep {
            target_rate: self.current_rate,
            achieved_rate,
            p50_us: p50,
            p99_us: p99,
            p999_us: p999,
            slo_passed,
        };
        formatter.print_saturation_step(&step);
        self.results.push(step);

        // Update state based on SLO result
        if slo_passed {
            self.last_good_rate = Some(self.current_rate);
            self.consecutive_failures = 0;
        } else {
            self.consecutive_failures += 1;
        }

        // Check if we should stop
        if self.consecutive_failures >= self.config.stop_after_failures {
            self.completed = true;
            return true;
        }

        // Advance to next rate
        let next_rate = (self.current_rate as f64 * self.config.step_multiplier) as u64;
        if next_rate > self.config.max_rate {
            self.completed = true;
            return true;
        }

        self.current_rate = next_rate;
        self.ratelimiter.set_rate(next_rate);
        metrics::TARGET_RATE.set(next_rate as i64);

        // Reset step tracking
        self.step_start = Instant::now();
        self.step_histogram = current_histogram;
        self.step_responses = current_responses;

        true
    }

    /// Check if SLO is met for the given latencies (in microseconds).
    fn check_slo(&self, p50_us: f64, p99_us: f64, p999_us: f64) -> bool {
        let slo = &self.config.slo;

        // Check p50 threshold if specified
        if let Some(threshold) = slo.p50 {
            let threshold_us = threshold.as_micros() as f64;
            if p50_us > threshold_us {
                return false;
            }
        }

        // Check p99 threshold if specified
        if let Some(threshold) = slo.p99 {
            let threshold_us = threshold.as_micros() as f64;
            if p99_us > threshold_us {
                return false;
            }
        }

        // Check p99.9 threshold if specified
        if let Some(threshold) = slo.p999 {
            let threshold_us = threshold.as_micros() as f64;
            if p999_us > threshold_us {
                return false;
            }
        }

        true
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
        }
    }
}

/// Get a percentile from a histogram snapshot (in microseconds).
fn percentile_from_histogram(hist: &Histogram, p: f64) -> f64 {
    if let Ok(Some(results)) = hist.percentiles(&[p])
        && let Some((_pct, bucket)) = results.first()
    {
        // Histogram stores nanoseconds, convert to microseconds
        return bucket.end() as f64 / 1000.0;
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
            stop_after_failures: 3,
            max_rate: 100_000_000,
            min_throughput_ratio: 0.9,
        };

        let rl = Arc::new(DynamicRateLimiter::new(1000));
        let state = SaturationSearchState::new(config, rl);

        // Under thresholds - should pass
        assert!(state.check_slo(50.0, 500.0, 800.0));

        // p50 over threshold - should fail
        assert!(!state.check_slo(150.0, 500.0, 800.0));

        // p999 over threshold - should fail
        assert!(!state.check_slo(50.0, 500.0, 1500.0));
    }
}
