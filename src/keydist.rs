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

/// How steady-state key ids are drawn from `0..n`.
#[derive(Debug, Clone)]
pub enum KeyDist {
    /// Every key equally likely. The historical (and default) behavior.
    Uniform { n: usize },
    /// Power-law skew: low ids are hot. See [`Zipfian`].
    Zipf(Zipfian),
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

    /// Build the sampler a `[workload.keyspace]` section asks for.
    ///
    /// This is the single place config maps to behavior. It is a named function
    /// rather than an inline `match` in the runner so a test can assert that
    /// `distribution = "zipf"` actually yields a skewed sampler -- the original
    /// bug was that the field parsed and was then never consulted, so a test of
    /// `Zipfian` alone would not have caught it.
    pub fn from_keyspace(ks: &crate::config::Keyspace) -> Self {
        match ks.distribution {
            crate::config::Distribution::Uniform => KeyDist::uniform(ks.count),
            crate::config::Distribution::Zipf => KeyDist::zipf(ks.count, ks.zipf_theta),
        }
    }

    #[inline]
    pub fn sample<R: Rng + ?Sized>(&self, rng: &mut R) -> usize {
        match self {
            KeyDist::Uniform { n } => rng.random_range(0..*n),
            KeyDist::Zipf(z) => z.sample(rng),
        }
    }

    /// Number of distinct key ids this can produce.
    pub fn len(&self) -> usize {
        match self {
            KeyDist::Uniform { n } => *n,
            KeyDist::Zipf(z) => z.n,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
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
        let d = KeyDist::from_keyspace(&cfg.workload.keyspace);
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
            KeyDist::from_keyspace(&cfg.workload.keyspace),
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
}
