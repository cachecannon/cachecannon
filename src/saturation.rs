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
    pub fn new(config: SaturationSearch, ratelimiter: Arc<Ratelimiter>) -> Self {
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
        let latency_reason = self.slo_fail_reason(p50, p99, p999);
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
    match hist.percentiles(&[p]) {
        Ok(Some(results)) => {
            if let Some((_pct, bucket)) = results.first() {
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
            stop_after_failures: 3,
            max_rate: 100_000_000,
            min_throughput_ratio: 0.9,
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
