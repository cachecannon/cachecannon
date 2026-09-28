//! One rate limiter per worker, splitting a total rate by connection share.
//!
//! Each worker's token dispatcher used to draw on a single shared limiter. The
//! shared limiter's wait estimate is for the next token fleet-wide, so every
//! worker's dispatcher woke for every token: one funded a claim and the rest
//! found the bucket empty and slept again for microseconds (#183). Giving each
//! worker its own limiter at its share of the rate removes the race: a
//! dispatcher sleeps until its own next token and only its own claims wake it.
//!
//! A worker's share is `rate * conns / total_conns`, which is usually
//! fractional and can be below one token a second. `ratelimit` treats a rate of
//! 0 as unlimited, so the share cannot be rounded to an integer rate. It is
//! expressed exactly instead, as `rate * conns` tokens per `total_conns`
//! seconds; the limiter refills in fixed point, so this stays accurate.

use ratelimit::Ratelimiter;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// The per-worker limiters for one run, and the total rate they split.
pub struct WorkerLimiters {
    /// Indexed by worker id. `None` for a worker with no connections, which
    /// has nothing to limit; giving it a rate-0 limiter would make it
    /// unlimited.
    limiters: Vec<Option<Arc<Ratelimiter>>>,
    /// Connections per worker, the weights of the split.
    weights: Vec<u64>,
    /// Sum of `weights`, and the limiters' period in seconds.
    total_weight: u64,
    rate: AtomicU64,
}

impl WorkerLimiters {
    /// Split `rate` across workers in proportion to `connections` (one entry
    /// per worker). Each limiter starts full and holds at most its share of
    /// one second of tokens, but never less than `min_burst`, so a claim for a
    /// full batch is always fundable.
    pub fn new(rate: u64, connections: &[usize], min_burst: u64) -> Self {
        let weights: Vec<u64> = connections.iter().map(|&c| c as u64).collect();
        let total_weight = weights.iter().sum::<u64>().max(1);
        let period = Duration::from_secs(total_weight);
        let limiters = weights
            .iter()
            .map(|&w| {
                if w == 0 {
                    return None;
                }
                let burst = share_ceil(rate, w, total_weight).max(min_burst);
                let rl = Ratelimiter::builder(rate.saturating_mul(w))
                    .period(period)
                    .max_tokens(burst)
                    .initial_available(burst)
                    .build()
                    .expect("failed to build worker rate limiter");
                Some(Arc::new(rl))
            })
            .collect();
        Self {
            limiters,
            weights,
            total_weight,
            rate: AtomicU64::new(rate),
        }
    }

    /// This worker's limiter, or `None` if it has no connections.
    pub fn worker(&self, id: usize) -> Option<Arc<Ratelimiter>> {
        self.limiters.get(id).cloned().flatten()
    }

    /// Change the total rate, re-splitting it across workers. Burst capacity
    /// is left as it was built, as it was with the single shared limiter.
    pub fn set_rate(&self, rate: u64) {
        let period = Duration::from_secs(self.total_weight);
        for (rl, &w) in self.limiters.iter().zip(&self.weights) {
            if let Some(rl) = rl {
                rl.set_rate_per(rate.saturating_mul(w), period);
            }
        }
        self.rate.store(rate, Ordering::Relaxed);
    }

    /// The total rate in requests per second.
    pub fn rate(&self) -> u64 {
        self.rate.load(Ordering::Relaxed)
    }

    /// Tokens dropped for exceeding burst capacity, summed over workers.
    pub fn dropped(&self) -> u64 {
        self.limiters.iter().flatten().map(|rl| rl.dropped()).sum()
    }
}

/// `ceil(rate * weight / total_weight)`, the worker's share of one second.
fn share_ceil(rate: u64, weight: u64, total_weight: u64) -> u64 {
    let num = (rate as u128) * (weight as u128);
    num.div_ceil(total_weight.max(1) as u128) as u64
}

/// How far behind its schedule a limiter is: unspent tokens times the time
/// each one represents. `rate` is per `period`, which is not a second for a
/// per-worker limiter, so this cannot be `available / rate` seconds.
pub fn slip_ns(rl: &Ratelimiter) -> u64 {
    let rate = rl.rate();
    if rate == 0 {
        return 0;
    }
    ((rl.available() as u128) * rl.period().as_nanos() / rate as u128).min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tokens per second a limiter is configured for.
    fn per_second(rl: &Ratelimiter) -> f64 {
        rl.rate() as f64 / rl.period().as_secs_f64()
    }

    #[test]
    fn shares_follow_connections_and_sum_to_the_rate() {
        // 64 connections over 8 workers, and an uneven 10 over 3.
        for (rate, conns) in [(100_000u64, vec![8usize; 8]), (17_100, vec![4, 3, 3])] {
            let l = WorkerLimiters::new(rate, &conns, 1);
            let total: usize = conns.iter().sum();
            let mut sum = 0.0;
            for (i, &c) in conns.iter().enumerate() {
                let got = per_second(&l.worker(i).unwrap());
                let want = rate as f64 * c as f64 / total as f64;
                assert!((got - want).abs() < 1e-6, "worker {i}: {got} vs {want}");
                sum += got;
            }
            assert!((sum - rate as f64).abs() < 1e-6, "sum {sum} vs {rate}");
        }
    }

    #[test]
    fn a_share_below_one_token_a_second_is_not_unlimited() {
        // 10 req/s over 64 connections on 8 workers: each worker's share is
        // 1.25/s. Rounding to an integer rate would give 1, and anything that
        // rounded to 0 would be unlimited.
        let l = WorkerLimiters::new(10, &[8; 8], 1);
        let rl = l.worker(0).unwrap();
        assert!((per_second(&rl) - 1.25).abs() < 1e-9);
        assert!(rl.rate() > 0);
    }

    #[test]
    fn a_worker_without_connections_gets_no_limiter() {
        let l = WorkerLimiters::new(1000, &[2, 0, 2], 1);
        assert!(l.worker(0).is_some());
        assert!(l.worker(1).is_none());
        assert!(l.worker(2).is_some());
    }

    #[test]
    fn set_rate_re_splits_and_reports_the_total() {
        let l = WorkerLimiters::new(1000, &[8; 8], 1);
        l.set_rate(80_000);
        assert_eq!(l.rate(), 80_000);
        for i in 0..8 {
            assert!((per_second(&l.worker(i).unwrap()) - 10_000.0).abs() < 1e-6);
        }
    }

    #[test]
    fn burst_is_a_share_of_one_second_but_at_least_a_batch() {
        let l = WorkerLimiters::new(100_000, &[8; 8], 32);
        assert_eq!(l.worker(0).unwrap().max_tokens(), 12_500);
        let l = WorkerLimiters::new(100, &[8; 8], 32);
        assert_eq!(
            l.worker(0).unwrap().max_tokens(),
            32,
            "floored at the batch"
        );
    }

    #[test]
    fn slip_accounts_for_the_period() {
        // One worker, 1000/s expressed as 8000 per 8 s, starting full with
        // 125 tokens: 125 unspent tokens are 125 ms of schedule.
        let rl = Ratelimiter::builder(8000)
            .period(Duration::from_secs(8))
            .max_tokens(125)
            .initial_available(125)
            .build()
            .unwrap();
        let slip = slip_ns(&rl);
        assert!((125_000_000..=126_000_000).contains(&slip), "slip {slip}");
    }
}
