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
//! `i/n` of a claim above the others, and starts holding that fraction. A
//! worker spends whole claims, so the fraction it holds persists (from the
//! start, and again whenever it fills to its cap), and the workers stay spread
//! across the claim interval.
//!
//! Starting at the stagger rather than the cap also means the run does not
//! open with a burst. The limiters are built before connections open and
//! prefill runs; a limiter that started full, or refilled to its cap
//! meanwhile, released a second's share of the rate at once when warmup
//! began. At 26,820 req/s of 1 MiB values, the sending host's NIC then queued
//! or dropped up to 1.2M packets a second for the first ten seconds of
//! warmup. `WorkerLimiters::restart` resets each limiter to its stagger as
//! warmup begins.

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
    /// What the limiter holds at the start of the run, in sub-units: its
    /// stagger. See [`restart`](Self::restart).
    start: u64,
}

impl WorkerLimiter {
    /// `rate` requests per `period`, holding at most `burst` requests plus
    /// `stagger` sub-units, and starting with `start` sub-units (`None`: full).
    /// Claims are `claim` requests.
    fn build(
        rate: u64,
        period: Duration,
        burst: u64,
        stagger: u64,
        claim: u64,
        start: Option<u64>,
    ) -> Self {
        let cap = burst.saturating_mul(SUB).saturating_add(stagger);
        let inner = Ratelimiter::builder(rate.saturating_mul(SUB))
            .period(period)
            .max_tokens(cap)
            .initial_available(start.unwrap_or(cap).min(cap))
            .build()
            .expect("failed to build worker rate limiter");
        Self {
            inner,
            claim: claim.max(1).saturating_mul(SUB),
            start: start.unwrap_or(cap).min(cap),
        }
    }

    /// Set the bucket back to what it held when built, discarding whatever
    /// accrued since. The limiters are built before the connections open and
    /// before prefill, and refill to their cap meanwhile; without this the run
    /// would open by releasing that cap at once.
    ///
    /// Nothing may take tokens while this runs: the cap is briefly narrowed to
    /// less than one claim, so a concurrent take could not fund a claim, and a
    /// concurrent refill could re-add the time since the last take under the
    /// restored cap. The runner calls it before setting the Warmup phase, and
    /// every taker waits for Warmup or Running.
    ///
    /// `ratelimit` refills lazily, so lowering the cap alone is not enough:
    /// the next take would refill the time since the last one under the
    /// restored cap. The cap is narrowed to one sub-unit above the start, a
    /// take forces the refill under it (leaving exactly the start), and then
    /// the cap is restored.
    pub fn restart(&self) {
        let cap = self.inner.max_tokens();
        self.inner.set_max_tokens(self.start.saturating_add(1));
        let _ = self.inner.try_wait_n(1);
        self.inner.set_max_tokens(cap);
    }

    /// A limiter at `rate` requests per second holding `burst`, unstaggered,
    /// with one-request claims, starting full.
    pub fn per_second(rate: u64, burst: u64) -> Self {
        Self::build(rate, Duration::from_secs(1), burst, 0, 1, None)
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
    /// per worker). Each limiter's cap is its share of one second of tokens,
    /// at least `claim` (a full batch, so a batch is always fundable), plus
    /// its stagger; it starts holding only the stagger. `claim` is also the
    /// unit the workers are staggered across.
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
                    Some(stagger),
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

    /// [`WorkerLimiter::restart`] every worker's limiter. Called as the run's
    /// traffic starts.
    pub fn restart(&self) {
        for rl in self.limiters.iter().flatten() {
            rl.restart();
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
    fn workers_start_staggered_across_one_claim() {
        // Each worker starts with only its stagger, i/n of a claim, so worker
        // 0 holds nothing and none holds a claim: the first claims are
        // fundable at different times, and nothing is released as a burst.
        // 100/s over 8 workers is 12.5/s each, so the half request worker 7
        // lacks takes 40 ms to accrue: only a delay that long could fund a
        // claim.
        let l = WorkerLimiters::new(100, &[8; 8], 4);
        for i in 0..8u64 {
            let rl = l.worker(i as usize).unwrap();
            assert_eq!(rl.inner.available(), i * 4 * SUB / 8, "worker {i}");
            assert!(
                rl.try_wait_n(4).is_err(),
                "worker {i} could fund a claim at start"
            );
        }
    }

    #[test]
    fn a_fresh_limiter_releases_no_more_than_its_rate() {
        // At 100K req/s over 8 workers, a full start released 12,500 requests
        // per worker at once. Starting at the stagger, the first 10 ms release
        // about 10 ms of the rate.
        let start = std::time::Instant::now();
        let l = WorkerLimiters::new(100_000, &[8; 8], 1);
        let mut taken = 0u64;
        while start.elapsed() < Duration::from_millis(10) {
            for i in 0..8 {
                while l.worker(i).unwrap().try_wait().is_ok() {
                    taken += 1;
                }
            }
        }
        let allowed = (100_000.0 * start.elapsed().as_secs_f64()) as u64 + 8;
        assert!(taken <= allowed, "took {taken}, rate allows {allowed}");
    }

    #[test]
    fn restart_discards_what_accrued_before_the_run() {
        // 100K req/s over 8 workers is 12,500/s each: 200 ms accrues about
        // 2,500 requests per worker, all of which restart must discard.
        let l = WorkerLimiters::new(100_000, &[8; 8], 4);
        std::thread::sleep(Duration::from_millis(200));
        l.restart();
        for i in 0..8u64 {
            let rl = l.worker(i as usize).unwrap();
            let held = rl.inner.available();
            // Its stagger, plus at most what accrued since the restart.
            assert!(held >= i * 4 * SUB / 8, "worker {i}: {held}");
            assert!(held < i * 4 * SUB / 8 + SUB, "worker {i}: {held}");
            assert_eq!(
                rl.max_tokens(),
                12_500 + (i * 4 / 8),
                "worker {i}: cap restored"
            );
            // `available` does not refill; a take does. A restart that only
            // narrowed and restored the cap would refill the 200 ms (about
            // 2,500 requests) here; 1,250 is 100 ms of this worker's rate.
            assert!(
                rl.try_wait_n(1250).is_err(),
                "worker {i} still held what accrued before restart"
            );
        }
    }

    #[test]
    fn a_fresh_limiter_reports_no_slip() {
        let l = WorkerLimiters::new(100_000, &[8; 8], 4);
        for i in 0..8 {
            assert_eq!(l.worker(i).unwrap().slip_ns(), 0, "worker {i}");
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
        let rl = WorkerLimiter::build(8000, Duration::from_secs(8), 125, 0, 5, None);
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
