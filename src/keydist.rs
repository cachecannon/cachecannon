//! Steady-state key-id sampling.
//!
//! `keyspace.distribution` selects how key ids are drawn from `0..count` during
//! the measured phase. Prefill and backfill are unaffected: prefill walks the
//! whole keyspace exactly once, and backfill re-sends a key that just missed.
//!
//! Why this module exists at all: `Distribution::Zipf` was accepted by the
//! config parser but never consulted by the worker, which drew keys with
//! `rng.random_range(0..key_count)` unconditionally. A run configured with
//! `distribution = "zipf"` therefore produced a uniform workload and reported
//! it as a successful run — the failure was silent in both directions, since
//! nothing errored and the numbers looked plausible. Skew changes cache hit
//! rate more than almost any other parameter, so that made every "working set
//! exceeds RAM" measurement a worst case being read as a typical one.

use rand::Rng;
use rand::distr::Distribution as _;
use rand_distr::Zipf as ZipfDist;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// How steady-state key ids are drawn from `0..n`.
#[derive(Debug, Clone)]
pub enum KeyDist {
    /// Every key equally likely. The historical (and default) behavior.
    Uniform { n: usize },
    /// Power-law skew: low ids are hot. See [`Zipfian`].
    Zipf(Zipfian),
    /// Newest keys are hot, in units of append batches. See [`Recency`].
    Recency(Recency),
}

impl KeyDist {
    pub fn uniform(n: usize) -> Self {
        KeyDist::Uniform { n }
    }

    /// `theta` is the YCSB skew parameter: larger is more skewed, 0.99 is the
    /// YCSB default. Must be > 0 and != 1 (the generator divides by `1 - theta`).
    pub fn zipf(n: usize, theta: f64) -> Self {
        KeyDist::Zipf(Zipfian::new(n, theta))
    }

    /// Build the sampler a `[workload]` section asks for.
    ///
    /// This is the single place config maps to behavior. It is a named function
    /// rather than an inline `match` in the runner so a test can assert that
    /// `distribution = "zipf"` actually yields a skewed sampler -- the original
    /// bug was that the field parsed and was then never consulted, so a test of
    /// `Zipfian` alone would not have caught it.
    ///
    /// `recency` reads the batch size from `[workload.append]`; config
    /// validation guarantees the section is present when the distribution is.
    pub fn from_workload(w: &crate::config::Workload) -> Self {
        let ks = &w.keyspace;
        match ks.distribution {
            crate::config::Distribution::Uniform => KeyDist::uniform(ks.count),
            crate::config::Distribution::Zipf => KeyDist::zipf(ks.count, ks.zipf_theta),
            crate::config::Distribution::Recency => {
                let batch =
                    w.append.as_ref().map(|a| a.batch).expect(
                        "recency distribution requires [workload.append]; validated by Config",
                    );
                KeyDist::Recency(Recency::new(
                    ks.count,
                    batch,
                    ks.hot_generations,
                    ks.hot_decay,
                    ks.hot_fraction,
                ))
            }
        }
    }

    #[inline]
    pub fn sample<R: Rng + ?Sized>(&self, rng: &mut R) -> usize {
        match self {
            KeyDist::Uniform { n } => rng.random_range(0..*n),
            KeyDist::Zipf(z) => z.sample(rng),
            KeyDist::Recency(r) => r.sample(rng),
        }
    }

    /// Number of distinct key ids this can produce right now. For `recency`
    /// this is the published head, which grows as append batches commit.
    pub fn len(&self) -> usize {
        match self {
            KeyDist::Uniform { n } => *n,
            KeyDist::Zipf(z) => z.n,
            KeyDist::Recency(r) => r.head(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The shared head for a `recency` sampler, so the append role can publish
    /// a committed batch. `None` for the stationary distributions.
    pub fn head_handle(&self) -> Option<Arc<AtomicUsize>> {
        match self {
            KeyDist::Recency(r) => Some(Arc::clone(&r.head)),
            _ => None,
        }
    }
}

/// Newest-is-hottest sampler over a keyspace that grows by append batches.
///
/// The id space is `0..head`. Ids arrive in fixed-size batches, so the
/// "generation" of an id is how many batches ago it was written, counted back
/// from the head: generation 0 is `[head - batch, head)`, generation 1 the
/// batch before it, and so on. Hotness attaches to the generation, not to the
/// key: a read picks a generation by age, then a key uniformly within it. That
/// is what "the recent appends are hot" means for data that lands in batches;
/// a rank-based skew from the head would instead concentrate a large share of
/// reads on a single newest key.
///
/// - `hot_fraction` of reads go to the `generations` newest batches, with
///   generation `a` weighted `decay^a` (normalized).
/// - The rest go uniformly to the body below the hot window.
/// - Before the first append lands the head is the prefilled `count`, so the
///   hot window is the newest `generations * batch` ids of the prefilled body:
///   the appends that happened before the run started.
///
/// The head is shared through an `Arc<AtomicUsize>` and advanced by the runner
/// when a batch is confirmed, so a read never targets an id that has not been
/// written. Sampling is a relaxed load plus arithmetic; no locks, no per-batch
/// table, because every batch is the same size.
#[derive(Debug, Clone)]
pub struct Recency {
    head: Arc<AtomicUsize>,
    batch: usize,
    generations: usize,
    hot_fraction: f64,
    /// Cumulative generation weights, normalized so the last entry is 1.0.
    cdf: Vec<f64>,
}

impl Recency {
    pub fn new(
        initial_head: usize,
        batch: usize,
        generations: usize,
        decay: f64,
        hot_fraction: f64,
    ) -> Self {
        assert!(batch >= 1, "recency batch must be >= 1");
        assert!(generations >= 1, "recency generations must be >= 1");
        assert!(
            decay.is_finite() && decay > 0.0 && decay <= 1.0,
            "recency decay must be in (0, 1]"
        );
        assert!(
            hot_fraction.is_finite() && (0.0..=1.0).contains(&hot_fraction),
            "recency hot_fraction must be in [0, 1]"
        );
        let mut cdf = Vec::with_capacity(generations);
        let mut acc = 0.0;
        let mut w = 1.0;
        for _ in 0..generations {
            acc += w;
            cdf.push(acc);
            w *= decay;
        }
        for c in cdf.iter_mut() {
            *c /= acc;
        }
        Self {
            head: Arc::new(AtomicUsize::new(initial_head)),
            batch,
            generations,
            hot_fraction,
            cdf,
        }
    }

    #[inline]
    pub fn head(&self) -> usize {
        self.head.load(Ordering::Relaxed)
    }

    /// Draw a generation index in `0..generations` by age weight.
    #[inline]
    fn sample_generation<R: Rng + ?Sized>(&self, rng: &mut R) -> usize {
        let u: f64 = rng.random();
        self.cdf
            .partition_point(|c| *c <= u)
            .min(self.generations - 1)
    }

    #[inline]
    pub fn sample<R: Rng + ?Sized>(&self, rng: &mut R) -> usize {
        let head = self.head();
        if head == 0 {
            return 0;
        }
        let hot_span = self.generations.saturating_mul(self.batch);
        let body_hi = head.saturating_sub(hot_span);
        let u: f64 = rng.random();
        if u < self.hot_fraction {
            let a = self.sample_generation(rng);
            let hi = head.saturating_sub(a * self.batch);
            let lo = head.saturating_sub((a + 1) * self.batch);
            if hi > lo {
                return rng.random_range(lo..hi);
            }
            // Generation clipped to nothing (keyspace smaller than the hot
            // window): fall through to a draw over everything that exists.
            return rng.random_range(0..head);
        }
        if body_hi == 0 {
            // No body below the hot window yet.
            return rng.random_range(0..head);
        }
        rng.random_range(0..body_hi)
    }
}

/// Power-law key sampler, wrapping `rand_distr::Zipf`.
///
/// `rand_distr` uses rejection-inversion (Hörmann & Derflinger), so construction
/// is O(1) — no `zeta(n, theta)` prefix sum, which would have been an O(keyspace)
/// startup cost (hundreds of milliseconds at 15M keys). It is still built once by
/// the runner and shared through an `Arc`, but now for simplicity rather than to
/// amortise setup.
///
/// `theta` is the exponent: P(rank k) is proportional to k^-theta, the same
/// parameter published cache benchmarks quote as "zipfian 0.99", so configs stay
/// comparable to those numbers.
///
/// Rank 1 (key id 0) is the hottest. That is fine for a cache benchmark even
/// though it looks degenerate: `write_key` renders an id into the LOW-order hex
/// digits at the tail of the key, so servers that shard by trailing hex
/// characters still spread the hot ids across their buckets.
#[derive(Debug, Clone)]
pub struct Zipfian {
    n: usize,
    theta: f64,
    inner: ZipfDist<f64>,
}

impl Zipfian {
    pub fn new(n: usize, theta: f64) -> Self {
        assert!(n >= 1, "zipf keyspace must be non-empty");
        let inner = ZipfDist::new(n as f64, theta)
            .expect("zipf parameters must be validated by Config::validate");
        Self { n, theta, inner }
    }

    #[inline]
    pub fn sample<R: Rng + ?Sized>(&self, rng: &mut R) -> usize {
        // rand_distr yields a rank in [1, n] as a float; keys are 0-based.
        let rank = self.inner.sample(rng) as usize;
        rank.saturating_sub(1).min(self.n - 1)
    }

    pub fn theta(&self) -> f64 {
        self.theta
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use rand_xoshiro::Xoshiro256PlusPlus;

    fn rng() -> Xoshiro256PlusPlus {
        Xoshiro256PlusPlus::seed_from_u64(0xC0FFEE)
    }

    #[test]
    fn uniform_stays_in_range_and_is_flat() {
        let d = KeyDist::uniform(10);
        let mut r = rng();
        let mut counts = [0usize; 10];
        for _ in 0..100_000 {
            counts[d.sample(&mut r)] += 1;
        }
        // Every bucket within 20% of 10k.
        for c in counts {
            assert!((8_000..12_000).contains(&c), "uniform bucket skewed: {c}");
        }
    }

    #[test]
    fn zipf_is_actually_skewed() {
        let n = 10_000;
        let d = KeyDist::zipf(n, 0.99);
        let mut r = rng();
        let samples = 200_000;
        let mut hot = 0usize;
        for _ in 0..samples {
            if d.sample(&mut r) < n / 100 {
                hot += 1;
            }
        }
        let frac = hot as f64 / samples as f64;
        // The top 1% of keys should take a large share. Uniform would give 0.01;
        // this test is the regression guard for the bug this module fixes, so it
        // asserts a value uniform sampling cannot reach by chance.
        assert!(
            frac > 0.25,
            "top 1% of keys got only {frac:.3} of samples; distribution looks uniform"
        );
    }

    #[test]
    fn zipf_never_exceeds_keyspace() {
        let n = 1_000;
        let d = KeyDist::zipf(n, 1.2);
        let mut r = rng();
        for _ in 0..200_000 {
            assert!(d.sample(&mut r) < n);
        }
    }

    #[test]
    fn zipf_handles_degenerate_keyspace() {
        let d = KeyDist::zipf(1, 0.99);
        let mut r = rng();
        for _ in 0..1_000 {
            assert_eq!(d.sample(&mut r), 0);
        }
    }

    /// The regression guard for the bug this module fixes: a config asking for
    /// zipf must produce a sampler that is actually skewed. Asserted from TOML so
    /// it covers parsing, defaulting, and the config->sampler mapping together.
    #[test]
    fn zipf_config_produces_a_skewed_sampler() {
        let cfg: crate::config::Config = toml::from_str(
            r#"
            [target]
            endpoints = ["127.0.0.1:11211"]
            protocol = "memcache-binary"
            [workload.keyspace]
            count = 10000
            distribution = "zipf"
            "#,
        )
        .expect("config should parse");
        let d = KeyDist::from_workload(&cfg.workload);
        assert!(
            matches!(d, KeyDist::Zipf(_)),
            "config asked for zipf but got {d:?}"
        );
        let mut r = rng();
        let mut hot = 0usize;
        for _ in 0..100_000 {
            if d.sample(&mut r) < 100 {
                hot += 1;
            }
        }
        let frac = hot as f64 / 100_000.0;
        assert!(
            frac > 0.25,
            "sampler built from a zipf config behaves uniformly ({frac:.3})"
        );
    }

    #[test]
    fn uniform_config_produces_a_uniform_sampler() {
        let cfg: crate::config::Config = toml::from_str(
            r#"
            [target]
            endpoints = ["127.0.0.1:11211"]
            protocol = "memcache-binary"
            [workload.keyspace]
            count = 10000
            "#,
        )
        .expect("config should parse");
        assert!(matches!(
            KeyDist::from_workload(&cfg.workload),
            KeyDist::Uniform { .. }
        ));
    }

    #[test]
    fn higher_theta_is_more_skewed() {
        let n = 10_000;
        let share = |theta: f64| {
            let d = KeyDist::zipf(n, theta);
            let mut r = rng();
            let mut hot = 0usize;
            for _ in 0..100_000 {
                if d.sample(&mut r) < n / 100 {
                    hot += 1;
                }
            }
            hot as f64 / 100_000.0
        };
        assert!(share(1.2) > share(0.7));
    }

    // ── recency ──────────────────────────────────────────────────────────

    /// Count samples per generation (0 = newest) plus a "body" bucket.
    fn recency_histogram(r: &Recency, samples: usize) -> (Vec<usize>, usize) {
        let head = r.head();
        let mut rng = rng();
        let mut gens = vec![0usize; r.generations];
        let mut body = 0usize;
        for _ in 0..samples {
            let id = r.sample(&mut rng);
            assert!(id < head, "sampled id {id} at or beyond head {head}");
            let age = (head - 1 - id) / r.batch;
            if age < r.generations {
                gens[age] += 1;
            } else {
                body += 1;
            }
        }
        (gens, body)
    }

    #[test]
    fn recency_newest_generation_is_hottest_and_decays() {
        // 100k body, batch 1000, 4 hot generations, decay 0.5, 80% hot.
        let r = Recency::new(100_000, 1_000, 4, 0.5, 0.8);
        let (gens, body) = recency_histogram(&r, 200_000);
        // Hot share ~80%, body ~20%.
        let hot: usize = gens.iter().sum();
        let hot_frac = hot as f64 / 200_000.0;
        assert!((0.78..0.82).contains(&hot_frac), "hot share {hot_frac}");
        assert!(body > 0);
        // Each older generation gets about half of the one before it.
        for a in 1..4 {
            let ratio = gens[a] as f64 / gens[a - 1] as f64;
            assert!((0.4..0.6).contains(&ratio), "gen {a} ratio {ratio}");
        }
        // The body is uniform over 96k ids while a hot generation covers 1k,
        // so per-id density in the newest generation dwarfs the body.
        let newest_per_id = gens[0] as f64 / 1_000.0;
        let body_per_id = body as f64 / 96_000.0;
        assert!(newest_per_id > 50.0 * body_per_id);
    }

    #[test]
    fn recency_follows_the_head_when_it_advances() {
        let r = Recency::new(10_000, 100, 2, 0.5, 1.0);
        let handle = KeyDist::Recency(r.clone())
            .head_handle()
            .expect("recency exposes its head");
        // hot_fraction = 1.0: every draw is in the top 200 ids below head.
        let mut rng = rng();
        for _ in 0..10_000 {
            let id = r.sample(&mut rng);
            assert!((9_800..10_000).contains(&id));
        }
        // Commit a batch: the window moves with the head.
        handle.store(10_100, Ordering::Release);
        assert_eq!(r.head(), 10_100);
        let mut seen_new = false;
        for _ in 0..10_000 {
            let id = r.sample(&mut rng);
            assert!((9_900..10_100).contains(&id));
            seen_new |= id >= 10_000;
        }
        assert!(seen_new, "no draw landed in the newly committed batch");
    }

    #[test]
    fn recency_never_exceeds_head_when_keyspace_is_tiny() {
        // Keyspace smaller than one generation: everything clips to [0, head).
        let r = Recency::new(5, 100, 8, 0.5, 0.8);
        let mut rng = rng();
        for _ in 0..10_000 {
            assert!(r.sample(&mut rng) < 5);
        }
        // And a single-key keyspace stays put.
        let r = Recency::new(1, 100, 8, 0.5, 0.8);
        for _ in 0..1_000 {
            assert_eq!(r.sample(&mut rng), 0);
        }
    }

    #[test]
    fn recency_decay_one_is_flat_across_generations() {
        let r = Recency::new(100_000, 1_000, 4, 1.0, 1.0);
        let (gens, body) = recency_histogram(&r, 200_000);
        assert_eq!(body, 0);
        for g in &gens {
            let frac = *g as f64 / 200_000.0;
            assert!((0.23..0.27).contains(&frac), "flat gen share {frac}");
        }
    }

    #[test]
    fn recency_config_produces_a_recency_sampler() {
        let cfg: crate::config::Config = toml::from_str(
            r#"
            [target]
            endpoints = ["127.0.0.1:6379"]
            protocol = "resp"
            [workload.keyspace]
            count = 10000
            distribution = "recency"
            hot_generations = 4
            [workload.append]
            batch = 100
            every = "1s"
            "#,
        )
        .expect("config should parse");
        let d = KeyDist::from_workload(&cfg.workload);
        assert!(matches!(d, KeyDist::Recency(_)), "got {d:?}");
        assert_eq!(d.len(), 10_000);
        let mut r = rng();
        let mut hot = 0usize;
        for _ in 0..100_000 {
            // Hot window is the top 400 ids.
            if d.sample(&mut r) >= 9_600 {
                hot += 1;
            }
        }
        let frac = hot as f64 / 100_000.0;
        assert!((0.78..0.82).contains(&frac), "hot share {frac}");
    }
}
