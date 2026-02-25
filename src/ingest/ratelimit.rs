use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

/// Per-site token-bucket rate limiter.
///
/// Each site gets `capacity` tokens per second. Tokens are refilled
/// continuously based on elapsed time since the last check.
#[derive(Clone)]
pub struct RateLimiter {
    buckets: Arc<Mutex<HashMap<String, Bucket>>>,
    capacity: u32,
}

struct Bucket {
    tokens: f64,
    last_refill: Instant,
}

impl RateLimiter {
    /// Create a new rate limiter.
    /// A capacity of 0 disables rate limiting (all requests are allowed).
    pub fn new(capacity: u32) -> Self {
        Self {
            buckets: Arc::new(Mutex::new(HashMap::new())),
            capacity,
        }
    }

    /// Check if a request for the given site_id is allowed.
    /// Returns `true` if allowed, `false` if rate-limited.
    #[allow(clippy::significant_drop_tightening)]
    pub fn check(&self, site_id: &str) -> bool {
        if self.capacity == 0 {
            return true;
        }

        let mut buckets = self.buckets.lock();
        let now = Instant::now();
        let cap = f64::from(self.capacity);

        let bucket = buckets.entry(site_id.to_string()).or_insert(Bucket {
            tokens: cap,
            last_refill: now,
        });

        // Refill tokens based on elapsed time
        let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
        bucket.tokens = elapsed.mul_add(cap, bucket.tokens).min(cap);
        bucket.last_refill = now;

        // Try to consume a token
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Remove stale buckets that haven't been accessed in over 5 minutes.
    pub fn cleanup(&self) {
        let mut buckets = self.buckets.lock();
        let now = Instant::now();
        buckets.retain(|_, bucket| now.duration_since(bucket.last_refill).as_secs() < 300);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_disabled_rate_limiter() {
        let rl = RateLimiter::new(0);
        assert!(rl.check("example.com"));
        assert!(rl.check("example.com"));
    }

    #[test]
    fn test_allows_within_limit() {
        let rl = RateLimiter::new(10);
        for _ in 0..10 {
            assert!(rl.check("example.com"));
        }
    }

    #[test]
    fn test_blocks_over_limit() {
        let rl = RateLimiter::new(2);
        assert!(rl.check("site.com"));
        assert!(rl.check("site.com"));
        // Third request should be blocked (only 2 tokens per second)
        assert!(!rl.check("site.com"));
    }

    #[test]
    fn test_separate_site_buckets() {
        let rl = RateLimiter::new(1);
        assert!(rl.check("site-a.com"));
        assert!(rl.check("site-b.com"));
        // site-a should be blocked, site-b should be fine
        assert!(!rl.check("site-a.com"));
        assert!(!rl.check("site-b.com"));
    }

    #[test]
    fn test_cleanup_stale_buckets() {
        let rl = RateLimiter::new(10);
        rl.check("active.com");
        rl.cleanup();
        // Recent bucket should survive cleanup
        assert!(rl.buckets.lock().contains_key("active.com"));
    }
}

#[cfg(test)]
mod prop_tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// Independence: exhausting site A's tokens does not affect site B's allowance.
        ///
        /// suffix_a starts with 'a', suffix_b starts with 'b', so the sites are always distinct.
        #[test]
        fn prop_rate_limit_independence(
            capacity in 1u32..10u32,
            suffix_a in "a[a-z]{2,5}",
            suffix_b in "b[a-z]{2,5}",
        ) {
            let rl = RateLimiter::new(capacity);
            let site_a = format!("{suffix_a}.test");
            let site_b = format!("{suffix_b}.test");

            for _ in 0..capacity {
                rl.check(&site_a);
            }

            prop_assert!(!rl.check(&site_a), "site_a must be rate-limited after token depletion");
            prop_assert!(rl.check(&site_b), "site_b must have its own independent token bucket");
        }

        /// Monotonicity: once tokens are depleted, all subsequent requests are blocked
        /// until time passes (no time passes within a single test, so all remain blocked).
        #[test]
        fn prop_rate_limit_monotonic_depletion(
            capacity in 1u32..5u32,
            site in "[a-z]{3,8}",
        ) {
            let rl = RateLimiter::new(capacity);
            for _ in 0..capacity {
                rl.check(&site);
            }
            prop_assert!(!rl.check(&site), "first check after depletion must be blocked");
            prop_assert!(!rl.check(&site), "second check after depletion must also be blocked");
        }
    }
}
