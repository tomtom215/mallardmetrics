use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// Idle time after which a bucket is eligible for removal.
const BUCKET_IDLE_SECS: u64 = 300;

/// Token-bucket rate limiter keyed by an arbitrary string (site ID or client IP).
///
/// Each key gets `capacity` tokens per second, refilled continuously.
///
/// The bucket map is capped: its keys come from attacker-influenced values, so
/// relying on the 15-minute cleanup sweep alone would let a flood of distinct
/// keys grow it without bound between sweeps.
#[derive(Clone)]
pub struct RateLimiter {
    buckets: Arc<Mutex<HashMap<String, Bucket>>>,
    capacity: u32,
    max_keys: usize,
    /// Number of times a new key was refused because the map was full.
    pub capacity_rejections: Arc<AtomicU64>,
}

struct Bucket {
    tokens: f64,
    last_refill: Instant,
}

/// Whether a bucket has gone untouched long enough to reclaim.
fn is_idle(bucket: &Bucket, now: Instant) -> bool {
    now.duration_since(bucket.last_refill).as_secs() >= BUCKET_IDLE_SECS
}

/// Move a bucket's last-use instant into the past, for tests.
#[cfg(test)]
fn age(bucket: &mut Bucket, by: std::time::Duration) {
    bucket.last_refill = bucket
        .last_refill
        .checked_sub(by)
        .unwrap_or(bucket.last_refill);
}

impl RateLimiter {
    /// Create a rate limiter. `capacity == 0` disables limiting entirely.
    ///
    /// `max_keys == 0` means unbounded (not recommended for untrusted input).
    pub fn new(capacity: u32, max_keys: usize) -> Self {
        Self {
            buckets: Arc::new(Mutex::new(HashMap::new())),
            capacity,
            max_keys,
            capacity_rejections: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Create a disabled rate limiter (used by tests and by "0 = no limit").
    pub fn disabled() -> Self {
        Self::new(0, 0)
    }

    /// Returns true when this limiter allows everything.
    pub const fn is_disabled(&self) -> bool {
        self.capacity == 0
    }

    /// Check whether a request for `key` is allowed.
    ///
    /// Returns `true` if allowed, `false` if rate-limited.
    pub fn check(&self, key: &str) -> bool {
        if self.capacity == 0 {
            return true;
        }

        let mut buckets = self.buckets.lock();
        let now = Instant::now();
        let cap = f64::from(self.capacity);

        if !buckets.contains_key(key) {
            if self.max_keys > 0 && buckets.len() >= self.max_keys {
                // Drop idle buckets first; only refuse if every tracked key is active.
                buckets.retain(|_, b| !is_idle(b, now));
                if buckets.len() >= self.max_keys {
                    self.capacity_rejections.fetch_add(1, Ordering::Relaxed);
                    return false;
                }
            }
            buckets.insert(
                key.to_string(),
                Bucket {
                    tokens: cap,
                    last_refill: now,
                },
            );
        }

        let bucket = buckets
            .get_mut(key)
            .expect("bucket was just inserted if absent");

        let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
        bucket.tokens = elapsed.mul_add(cap, bucket.tokens).min(cap);
        bucket.last_refill = now;

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Remove buckets untouched for longer than the idle threshold.
    pub fn cleanup(&self) {
        let now = Instant::now();
        self.buckets
            .lock()
            .retain(|_, bucket| !is_idle(bucket, now));
    }

    /// Number of tracked buckets (exposed for metrics and tests).
    pub fn tracked_keys(&self) -> usize {
        self.buckets.lock().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_disabled_rate_limiter() {
        let rl = RateLimiter::disabled();
        assert!(rl.check("example.com"));
        assert!(rl.check("example.com"));
        assert!(rl.is_disabled());
    }

    #[test]
    fn test_allows_within_limit() {
        let rl = RateLimiter::new(10, 1000);
        for _ in 0..10 {
            assert!(rl.check("example.com"));
        }
    }

    #[test]
    fn test_blocks_over_limit() {
        let rl = RateLimiter::new(2, 1000);
        assert!(rl.check("site.com"));
        assert!(rl.check("site.com"));
        assert!(!rl.check("site.com"));
    }

    #[test]
    fn test_separate_key_buckets() {
        let rl = RateLimiter::new(1, 1000);
        assert!(rl.check("site-a.com"));
        assert!(rl.check("site-b.com"));
        assert!(!rl.check("site-a.com"));
        assert!(!rl.check("site-b.com"));
    }

    #[test]
    fn test_cleanup_keeps_recent_buckets() {
        let rl = RateLimiter::new(10, 1000);
        rl.check("active.com");
        rl.cleanup();
        assert_eq!(rl.tracked_keys(), 1);
    }

    #[test]
    fn test_cleanup_drops_idle_buckets() {
        let rl = RateLimiter::new(10, 1000);
        rl.check("stale.com");
        {
            let mut buckets = rl.buckets.lock();
            age(
                buckets.get_mut("stale.com").unwrap(),
                std::time::Duration::from_secs(BUCKET_IDLE_SECS + 10),
            );
        }
        rl.cleanup();
        assert_eq!(rl.tracked_keys(), 0);
    }

    #[test]
    fn test_key_cap_is_enforced() {
        // Without a cap, a flood of distinct keys grows the map without bound
        // between the 15-minute cleanup sweeps.
        let rl = RateLimiter::new(10, 4);
        for i in 0..100 {
            rl.check(&format!("site-{i}.com"));
        }
        assert!(
            rl.tracked_keys() <= 4,
            "tracked keys ({}) must not exceed the cap",
            rl.tracked_keys()
        );
        assert!(rl.capacity_rejections.load(Ordering::Relaxed) > 0);
    }

    #[test]
    fn test_key_cap_reclaims_idle_buckets_before_refusing() {
        let rl = RateLimiter::new(10, 2);
        rl.check("a.com");
        rl.check("b.com");
        // Age both buckets past the idle threshold.
        {
            let mut buckets = rl.buckets.lock();
            for bucket in buckets.values_mut() {
                age(
                    bucket,
                    std::time::Duration::from_secs(BUCKET_IDLE_SECS + 10),
                );
            }
        }
        assert!(
            rl.check("c.com"),
            "idle buckets must be reclaimed rather than refusing a new key"
        );
    }

    #[test]
    fn test_existing_key_still_works_when_map_is_full() {
        let rl = RateLimiter::new(10, 1);
        assert!(rl.check("a.com"));
        // b.com cannot get a bucket, but a.com must keep working.
        assert!(!rl.check("b.com"));
        assert!(rl.check("a.com"));
    }

    #[test]
    fn test_unbounded_when_max_keys_zero() {
        let rl = RateLimiter::new(10, 0);
        for i in 0..50 {
            rl.check(&format!("s{i}"));
        }
        assert_eq!(rl.tracked_keys(), 50);
    }

    #[test]
    fn test_tokens_refill_over_time() {
        let rl = RateLimiter::new(1, 100);
        assert!(rl.check("site.com"));
        assert!(!rl.check("site.com"));
        {
            let mut buckets = rl.buckets.lock();
            age(
                buckets.get_mut("site.com").unwrap(),
                std::time::Duration::from_secs(2),
            );
        }
        assert!(rl.check("site.com"), "tokens must refill as time passes");
    }
}

#[cfg(test)]
mod prop_tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// Independence: exhausting one key's tokens does not affect another.
        #[test]
        fn prop_rate_limit_independence(
            capacity in 1u32..10u32,
            suffix_a in "a[a-z]{2,5}",
            suffix_b in "b[a-z]{2,5}",
        ) {
            let rl = RateLimiter::new(capacity, 1000);
            let site_a = format!("{suffix_a}.test");
            let site_b = format!("{suffix_b}.test");

            for _ in 0..capacity {
                rl.check(&site_a);
            }

            prop_assert!(!rl.check(&site_a));
            prop_assert!(rl.check(&site_b));
        }

        /// Once depleted, further requests stay blocked until time passes.
        #[test]
        fn prop_rate_limit_monotonic_depletion(
            capacity in 1u32..5u32,
            site in "[a-z]{3,8}",
        ) {
            let rl = RateLimiter::new(capacity, 1000);
            for _ in 0..capacity {
                rl.check(&site);
            }
            prop_assert!(!rl.check(&site));
            prop_assert!(!rl.check(&site));
        }

        /// The bucket map never exceeds its cap, whatever keys arrive.
        #[test]
        fn prop_bucket_map_is_bounded(
            cap in 1usize..20usize,
            keys in 1usize..200usize,
        ) {
            let rl = RateLimiter::new(5, cap);
            for i in 0..keys {
                rl.check(&format!("key-{i}"));
            }
            prop_assert!(rl.tracked_keys() <= cap);
        }
    }
}
