//! Dynamic rate limiter with lock-free token bucket.
//!
//! This rate limiter supports changing the target rate at runtime without
//! locks, making it suitable for saturation search where the rate is
//! incrementally increased.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// A lock-free token bucket rate limiter that supports dynamic rate changes.
///
/// The rate limiter uses a token bucket algorithm where:
/// - Tokens are added at the configured rate per second
/// - Each request consumes one token
/// - Burst is limited to 1 second worth of tokens
/// - A rate of 0 means unlimited (no rate limiting)
pub struct DynamicRateLimiter {
    /// Target rate in requests per second. 0 = unlimited.
    target_rate: AtomicU64,
    /// Available tokens, scaled by TOKEN_SCALE for sub-token precision.
    tokens: AtomicU64,
    /// Last refill timestamp in nanoseconds since creation.
    last_refill_ns: AtomicU64,
    /// Creation instant for relative timing.
    created: Instant,
}

/// Scale factor for token precision (allows fractional tokens).
const TOKEN_SCALE: u64 = 1_000_000;

impl DynamicRateLimiter {
    /// Create a new rate limiter with the specified rate.
    ///
    /// A rate of 0 means unlimited (no rate limiting).
    pub fn new(rate: u64) -> Self {
        let now = Instant::now();
        // Start with a full bucket (1 second worth of tokens)
        let initial_tokens = if rate > 0 { rate * TOKEN_SCALE } else { 0 };
        Self {
            target_rate: AtomicU64::new(rate),
            tokens: AtomicU64::new(initial_tokens),
            last_refill_ns: AtomicU64::new(0),
            created: now,
        }
    }

    /// Get the current target rate.
    #[inline]
    pub fn rate(&self) -> u64 {
        self.target_rate.load(Ordering::Relaxed)
    }

    /// Set a new target rate.
    ///
    /// This takes effect immediately. A rate of 0 means unlimited.
    /// Tokens are NOT reset - the bucket will naturally fill at the new rate.
    /// This avoids microbursts when increasing the rate.
    pub fn set_rate(&self, rate: u64) {
        self.target_rate.store(rate, Ordering::Release);
    }

    /// Try to acquire a token for one request.
    ///
    /// Returns `true` if the request is allowed, `false` if rate limited.
    /// When the rate is 0 (unlimited), always returns `true`.
    #[inline]
    pub fn try_acquire(&self) -> bool {
        let rate = self.target_rate.load(Ordering::Relaxed);
        if rate == 0 {
            return true; // Unlimited
        }

        // Refill tokens based on elapsed time
        self.refill(rate);

        // Try to consume one token
        let cost = TOKEN_SCALE;
        loop {
            let current = self.tokens.load(Ordering::Relaxed);
            if current < cost {
                return false; // Rate limited
            }

            match self.tokens.compare_exchange_weak(
                current,
                current - cost,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(_) => continue, // Retry on contention
            }
        }
    }

    /// Refill tokens based on elapsed time.
    #[inline]
    fn refill(&self, rate: u64) {
        let now_ns = self.created.elapsed().as_nanos() as u64;
        let last_ns = self.last_refill_ns.load(Ordering::Relaxed);
        let elapsed_ns = now_ns.saturating_sub(last_ns);

        // Only refill if at least 1us has passed (avoid busy spinning)
        if elapsed_ns < 1_000 {
            return;
        }

        // Calculate tokens to add: rate * elapsed_time
        // tokens = rate * (elapsed_ns / 1_000_000_000) * TOKEN_SCALE
        // To avoid overflow: tokens = (rate * elapsed_ns * TOKEN_SCALE) / 1_000_000_000
        // But this can overflow, so we reorder:
        // tokens = (rate * TOKEN_SCALE / 1_000_000_000) * elapsed_ns
        // Which is: tokens = rate * elapsed_ns / 1000 (since TOKEN_SCALE = 1M)
        let new_tokens = (rate as u128 * elapsed_ns as u128 / 1_000) as u64;

        if new_tokens == 0 {
            return;
        }

        // Try to update last_refill_ns (CAS to avoid races)
        if self
            .last_refill_ns
            .compare_exchange(last_ns, now_ns, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            return; // Another thread updated it, skip this refill
        }

        // Add tokens, capped at 1 second worth (burst limit)
        let max_tokens = rate * TOKEN_SCALE;
        loop {
            let current = self.tokens.load(Ordering::Relaxed);
            let new_total = current.saturating_add(new_tokens).min(max_tokens);
            if new_total <= current {
                break; // Already at max
            }
            match self.tokens.compare_exchange_weak(
                current,
                new_total,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(_) => continue,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn test_unlimited() {
        let rl = DynamicRateLimiter::new(0);
        assert!(rl.try_acquire());
        assert!(rl.try_acquire());
        assert!(rl.try_acquire());
    }

    #[test]
    fn test_basic_limiting() {
        let rl = DynamicRateLimiter::new(10);
        // Should be able to acquire 10 tokens (the initial bucket)
        for _ in 0..10 {
            assert!(rl.try_acquire());
        }
        // 11th should fail
        assert!(!rl.try_acquire());
    }

    #[test]
    fn test_refill() {
        let rl = DynamicRateLimiter::new(1000);
        // Drain the bucket
        for _ in 0..1000 {
            rl.try_acquire();
        }
        assert!(!rl.try_acquire());

        // Wait for some tokens to refill (100ms = ~100 tokens at 1000/s)
        thread::sleep(Duration::from_millis(100));

        // Should have some tokens now
        assert!(rl.try_acquire());
    }

    #[test]
    fn test_dynamic_rate_change() {
        let rl = DynamicRateLimiter::new(10);

        // Drain at 10/s
        for _ in 0..10 {
            rl.try_acquire();
        }
        assert!(!rl.try_acquire());

        // Increase rate to 100/s - tokens don't reset, but refill faster
        rl.set_rate(100);

        // Wait for some tokens to refill at the new rate
        thread::sleep(Duration::from_millis(50));

        // Should have ~5 tokens (100/s * 0.05s)
        assert!(rl.try_acquire());
    }
}
