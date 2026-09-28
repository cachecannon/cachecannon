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
//!
//! Separate limiters with the same rate, started together, also fund claims
//! together: every worker crosses its next claim at the same instant and the
//! server sees all of them at once. Measured on the rack at 2048 connections,
//! pipeline 32 and 100K req/s, that was 8 x 32 requests every 2.6 ms and GET
//! p99 rose from 647 us to 860 us. The workers are therefore staggered: each
//! limiter counts in 1/`SUB` of a request, and worker `i` of `n` gets a cap
//! `i/n` of a claim above the others. A worker spends whole claims, so the
//! fraction it holds when it fills to its cap persists, and the workers stay
//! spread across the claim interval.

use ratelimit::{Ratelimiter, TryWaitError};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Limiter units per request. Fine enough to stagger any practical number of
/// workers within one claim.
const SUB: u64 = 1024;

/// One worker's limiter. Counts requests; internally counts in 1/`SUB` of a
/// request so its phase can be offset by a fraction of a claim.
pub struct WorkerLimiter {
    inner: Ratelimiter,
    /// Size of one claim in sub-units: the batch every claim asks for.
    claim: u64,
}

impl WorkerLimiter {
    /// `rate` requests per `period`, holding at most `burst` requests plus
    /// `stagger` sub-units, and starting full. Claims are `claim` requests.
    fn build(rate: u64, period: Duration, burst: u64, stagger: u64, claim: u64) -> Self {
        let cap = burst.saturating_mul(SUB).saturating_add(stagger);
        let inner = Ratelimiter::builder(rate.saturating_mul(SUB))
            .period(period)
            .max_tokens(cap)
            .initial_available(cap)
            .build()
            .expect("failed to build worker rate limiter");
        Self {
            inner,
            claim: claim.max(1).saturating_mul(SUB),
        }
    }

    /// A limiter at `rate` requests per second holding `burst`, unstaggered,
    /// with one-request claims.
    pub fn per_second(rate: u64, burst: u64) -> Self {
        Self::build(rate, Duration::from_secs(1), burst, 0, 1)
    }

    /// Take `n` requests' worth of tokens, or report how long until they are
    /// available.
    pub fn try_wait_n(&self, n: u64) -> Result<(), TryWaitError> {
        self.inner.try_wait_n(n.saturating_mul(SUB))
    }

    pub fn try_wait(&self) -> Result<(), TryWaitError> {
        self.try_wait_n(1)
    }

    /// Largest claim, in whole requests, this limiter can fund.
    pub fn max_tokens(&self) -> u64 {
        self.inner.max_tokens() / SUB
    }

    /// How far behind its schedule this worker is: the time since its next
    /// claim became fundable, `(available - claim) / rate`, or zero while it
    /// holds less than a claim. Tokens short of a claim cannot be spent yet, so
    /// they are not lag; counting them would report the worker's position
    /// within the claim interval (which the stagger deliberately varies) as
    /// slip. The units cancel, and `rate` is per `period`, which is not a
    /// second for a per-worker limiter.
    pub fn slip_ns(&self) -> u64 {
        let rate = self.inner.rate();
        if rate == 0 {
            return 0;
        }
        let behind = self.inner.available().saturating_sub(self.claim);
        ((behind as u128) * self.inner.period().as_nanos() / rate as u128).min(u64::MAX as u128)
            as u64
    }

    /// Requests' worth of tokens dropped for exceeding the cap.
    pub fn dropped(&self) -> u64 {
        self.inner.dropped() / SUB
    }

    /// Configured requests per second.
    pub fn rate_per_second(&self) -> f64 {
        self.inner.rate() as f64 / SUB as f64 / self.inner.period().as_secs_f64()
    }

    fn set_rate_per(&self, rate: u64, period: Duration) {
        self.inner.set_rate_per(rate.saturating_mul(SUB), period);
    }

    /// Set the cap in requests, for tests that need an unfundable claim.
    #[cfg(test)]
    pub fn set_max_tokens(&self, requests: u64) {
        self.inner.set_max_tokens(requests.saturating_mul(SUB));
    }
}

/// The per-worker limiters for one run, and the total rate they split.
pub struct WorkerLimiters {
    /// Indexed by worker id. `None` for a worker with no connections, which
    /// has nothing to limit; giving it a rate-0 limiter would make it
    /// unlimited.
    limiters: Vec<Option<Arc<WorkerLimiter>>>,
    /// Connections per worker, the weights of the split.
    weights: Vec<u64>,
    /// Sum of `weights`, and the limiters' period in seconds.
    total_weight: u64,
    rate: AtomicU64,
}

impl WorkerLimiters {
    /// Split `rate` across workers in proportion to `connections` (one entry
    /// per worker). Each limiter holds its share of one second of tokens, at
    /// least `claim` (a full batch, so a batch is always fundable), plus its
    /// stagger. `claim` is also the unit the workers are staggered across.
    pub fn new(rate: u64, connections: &[usize], claim: u64) -> Self {
        let weights: Vec<u64> = connections.iter().map(|&c| c as u64).collect();
        let total_weight = weights.iter().sum::<u64>().max(1);
        let period = Duration::from_secs(total_weight);
        let active = weights.iter().filter(|&&w| w > 0).count().max(1) as u64;
        let claim = claim.max(1);
        let mut index = 0u64;
        let limiters = weights
            .iter()
            .map(|&w| {
                if w == 0 {
                    return None;
                }
                let burst = share_ceil(rate, w, total_weight).max(claim);
                let stagger = index * claim * SUB / active;
                index += 1;
                Some(Arc::new(WorkerLimiter::build(
                    rate.saturating_mul(w),
                    period,
                    burst,
                    stagger,
                    claim,
                )))
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
    pub fn worker(&self, id: usize) -> Option<Arc<WorkerLimiter>> {
        self.limiters.get(id).cloned().flatten()
    }

    /// Change the total rate, re-splitting it across workers. Caps are left as
    /// they were built, as the single shared limiter's was.
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

    /// Requests dropped for exceeding a cap, summed over workers.
    pub fn dropped(&self) -> u64 {
        self.limiters.iter().flatten().map(|rl| rl.dropped()).sum()
    }
}

/// `ceil(rate * weight / total_weight)`, the worker's share of one second.
fn share_ceil(rate: u64, weight: u64, total_weight: u64) -> u64 {
    let num = (rate as u128) * (weight as u128);
    num.div_ceil(total_weight.max(1) as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shares_follow_connections_and_sum_to_the_rate() {
        // 64 connections over 8 workers, and an uneven 10 over 3.
        for (rate, conns) in [(100_000u64, vec![8usize; 8]), (17_100, vec![4, 3, 3])] {
            let l = WorkerLimiters::new(rate, &conns, 1);
            let total: usize = conns.iter().sum();
            let mut sum = 0.0;
            for (i, &c) in conns.iter().enumerate() {
                let got = l.worker(i).unwrap().rate_per_second();
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
        assert!((rl.rate_per_second() - 1.25).abs() < 1e-9);
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
            assert!((l.worker(i).unwrap().rate_per_second() - 10_000.0).abs() < 1e-6);
        }
    }

    #[test]
    fn burst_is_a_share_of_one_second_but_at_least_a_claim() {
        let l = WorkerLimiters::new(100_000, &[8; 8], 32);
        assert_eq!(l.worker(0).unwrap().max_tokens(), 12_500);
        let l = WorkerLimiters::new(100, &[8; 8], 32);
        for i in 0..8 {
            // At least one claim, and the stagger adds less than another.
            let cap = l.worker(i).unwrap().max_tokens();
            assert!((32..64).contains(&cap), "worker {i}: {cap}");
        }
    }

    #[test]
    fn workers_are_staggered_across_one_claim() {
        // With the buckets full, each worker holds a different fraction of a
        // claim, i/n of the way through it. 100/s over 64 connections is 12.5/s
        // per worker, so each holds 13 requests before its stagger.
        let l = WorkerLimiters::new(100, &[8; 8], 4);
        let held: Vec<u64> = (0..8)
            .map(|i| l.worker(i).unwrap().inner.available())
            .collect();
        for (i, &h) in held.iter().enumerate() {
            assert_eq!(h, 13 * SUB + i as u64 * 4 * SUB / 8, "worker {i}");
        }
    }

    #[test]
    fn staggered_workers_cross_a_claim_at_different_times() {
        // Drain each worker to below one claim; the time until its next claim
        // is fundable differs by worker, spread across the claim interval.
        let l = WorkerLimiters::new(800, &[1; 8], 1);
        let mut waits = Vec::new();
        for i in 0..8 {
            let rl = l.worker(i).unwrap();
            while rl.try_wait().is_ok() {}
            match rl.try_wait() {
                Err(TryWaitError::Insufficient(d)) => waits.push(d),
                other => panic!("worker {i}: {other:?}"),
            }
        }
        // 100/s per worker: a claim every 10 ms. The waits should be spread
        // over that interval, not identical.
        let min = waits.iter().min().unwrap();
        let max = waits.iter().max().unwrap();
        assert!(*max - *min >= Duration::from_millis(7), "waits {waits:?}");
    }

    #[test]
    fn slip_accounts_for_the_period() {
        // One worker at 1000/s expressed as 8000 per 8 s, full at 125
        // requests with 5-request claims: 120 requests beyond the next claim
        // are 120 ms of schedule.
        let rl = WorkerLimiter::build(8000, Duration::from_secs(8), 125, 0, 5);
        let slip = rl.slip_ns();
        assert!((120_000_000..=121_000_000).contains(&slip), "slip {slip}");
    }

    #[test]
    fn less_than_a_claim_is_not_slip() {
        // Drained to below one claim, a worker holds only a fraction of its
        // next claim; nothing is fundable, so nothing is late.
        let l = WorkerLimiters::new(800, &[1; 8], 4);
        for i in 0..8 {
            let rl = l.worker(i).unwrap();
            while rl.try_wait_n(4).is_ok() {}
            assert_eq!(rl.slip_ns(), 0, "worker {i}");
        }
    }
}
