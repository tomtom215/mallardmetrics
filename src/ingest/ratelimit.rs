use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// Idle time after which a bucket is eligible for removal.
const BUCKET_IDLE_SECS: u64 = 300;

/// The key under which a client address is rate limited or locked out.
///
/// IPv4 addresses are used as they are. An IPv6 address is reduced to its /64:
/// a single subscriber or VPS routinely owns a whole /64 and can send each
/// request from a different /128, which gave every request a fresh token bucket
/// and a fresh login-attempt record. An IPv4-mapped IPv6 address
/// (`::ffff:192.0.2.1`, what a dual-stack socket reports for an IPv4 peer) is
/// the IPv4 address it wraps, so one client does not hold two keys. Text that is
/// not an address (the `"unknown"` placeholder) is returned unchanged.
///
/// The full address stays in use for the visitor hash and GeoIP, which want
/// the most specific value; only the abuse-control keys are coarsened.
pub fn network_key(ip: &str) -> std::borrow::Cow<'_, str> {
    use std::net::IpAddr;
    match ip.parse::<IpAddr>().map(|a| a.to_canonical()) {
        Ok(IpAddr::V6(v6)) => {
            let [a, b, c, d, ..] = v6.segments();
            std::borrow::Cow::Owned(format!("{a:x}:{b:x}:{c:x}:{d:x}::/64"))
        }
        Ok(IpAddr::V4(v4)) => {
            let canonical = v4.to_string();
            if canonical == ip {
                std::borrow::Cow::Borrowed(ip)
            } else {
                std::borrow::Cow::Owned(canonical)
            }
        }
        Err(_) => std::borrow::Cow::Borrowed(ip),
    }
}

/// Token-bucket rate limiter keyed by an arbitrary string (site ID or client IP).
///
/// Each key gets `capacity` tokens per second, refilled continuously.
///
/// The bucket map is capped: its keys come from attacker-influenced values, so
/// relying on the 15-minute cleanup sweep alone would let a flood of distinct
/// keys grow it without bound between sweeps. At the cap the limiter makes room
/// instead of refusing new keys: refusing meant that 10,000 cheap requests with
/// distinct keys turned every genuine new visitor away with a 429 for as long as
/// the attacker kept it up. Room is made in batches (an eighth of the cap at a
/// time), so the scan over the map is paid once per batch rather than once per
/// request while holding the lock every ingest request needs.
#[derive(Clone)]
pub struct RateLimiter {
    buckets: Arc<Mutex<HashMap<String, Bucket>>>,
    capacity: u32,
    max_keys: usize,
    /// Number of buckets dropped early because the map was full.
    pub capacity_evictions: Arc<AtomicU64>,
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
            capacity_evictions: Arc::new(AtomicU64::new(0)),
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

        let now = Instant::now();
        let cap = f64::from(self.capacity);
        let mut buckets = self.buckets.lock();

        if let Some(bucket) = buckets.get_mut(key) {
            let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
            bucket.tokens = elapsed.mul_add(cap, bucket.tokens).min(cap);
            bucket.last_refill = now;
            return if bucket.tokens >= 1.0 {
                bucket.tokens -= 1.0;
                true
            } else {
                false
            };
        }

        if self.max_keys > 0 && buckets.len() >= self.max_keys {
            self.make_room(&mut buckets, now);
        }
        // A new key starts with a full bucket, less the request being checked.
        buckets.insert(
            key.to_string(),
            Bucket {
                tokens: cap - 1.0,
                last_refill: now,
            },
        );
        true
    }

    /// Free at least an eighth of the cap: idle buckets first, then the least
    /// recently used.
    fn make_room(&self, buckets: &mut HashMap<String, Bucket>, now: Instant) {
        let target = (self.max_keys / 8).max(1);
        let before = buckets.len();
        buckets.retain(|_, b| !is_idle(b, now));
        let freed = before - buckets.len();
        if freed >= target {
            return;
        }

        let need = (target - freed).min(buckets.len());
        if need == 0 {
            return;
        }
        let mut stamps: Vec<Instant> = buckets.values().map(|b| b.last_refill).collect();
        let (_, cutoff, _) = stamps.select_nth_unstable(need - 1);
        let cutoff = *cutoff;
        let before = buckets.len();
        buckets.retain(|_, b| b.last_refill > cutoff);
        self.capacity_evictions.fetch_add(
            u64::try_from(before - buckets.len()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
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
        assert!(rl.capacity_evictions.load(Ordering::Relaxed) > 0);
    }

    #[test]
    fn test_key_cap_reclaims_idle_buckets_before_evicting() {
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
        assert!(rl.check("c.com"));
        assert_eq!(
            rl.capacity_evictions.load(Ordering::Relaxed),
            0,
            "idle buckets are reclaimed first; nothing active is sacrificed"
        );
    }

    #[test]
    fn test_existing_key_still_works_when_map_is_full() {
        let rl = RateLimiter::new(10, 1);
        assert!(rl.check("a.com"));
        assert!(rl.check("a.com"));
        assert_eq!(rl.tracked_keys(), 1);
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
    fn test_new_key_is_admitted_when_the_map_is_full_of_active_keys() {
        // SI-07 regression: at the cap every new key was refused, so 10,000
        // cheap requests with distinct keys turned genuine new visitors away
        // with a 429 for as long as the attacker kept it up.
        let rl = RateLimiter::new(10, 8);
        for i in 0..8 {
            assert!(rl.check(&format!("active-{i}")));
        }
        assert!(
            rl.check("a-real-visitor"),
            "a new key must get a bucket even when every tracked key is active"
        );
        assert!(rl.tracked_keys() <= 8, "the cap still holds");
    }

    #[test]
    fn test_pressure_evicts_the_least_recently_used_bucket() {
        let rl = RateLimiter::new(10, 8);
        for i in 0..8 {
            rl.check(&format!("k{i}"));
        }
        // Age everything except k5, so k5 is the one worth keeping.
        {
            let mut buckets = rl.buckets.lock();
            for (key, bucket) in buckets.iter_mut() {
                if key != "k5" {
                    age(bucket, std::time::Duration::from_secs(10));
                }
            }
        }
        rl.check("newcomer");
        let buckets = rl.buckets.lock();
        assert!(buckets.contains_key("k5"), "the recently used key survives");
        assert!(buckets.contains_key("newcomer"));
        assert!(buckets.len() <= 8);
    }

    #[test]
    fn test_eviction_is_batched_not_per_request() {
        // The scan that makes room is O(n) while the lock every ingest request
        // needs is held. It must free a batch, so a flood of new keys pays it
        // once per batch rather than once per request.
        let rl = RateLimiter::new(10, 800);
        for i in 0..800 {
            rl.check(&format!("seed-{i}"));
        }
        let before = rl.capacity_evictions.load(Ordering::Relaxed);
        rl.check("overflow-1");
        let first_batch = rl.capacity_evictions.load(Ordering::Relaxed) - before;
        assert!(
            first_batch >= 100,
            "freed {first_batch}, expected an eighth of the cap"
        );
        for i in 0..50 {
            rl.check(&format!("overflow-b{i}"));
        }
        assert_eq!(
            rl.capacity_evictions.load(Ordering::Relaxed) - before,
            first_batch,
            "the next 50 new keys fit in the room already made"
        );
    }

    #[test]
    fn test_network_key_collapses_ipv6_to_its_64() {
        assert_eq!(network_key("2001:db8:1:2::1"), "2001:db8:1:2::/64");
        assert_eq!(
            network_key("2001:db8:1:2:ffff:ffff:ffff:ffff"),
            "2001:db8:1:2::/64"
        );
        assert_ne!(
            network_key("2001:db8:1:2::1"),
            network_key("2001:db8:1:3::1")
        );
        assert_eq!(network_key("203.0.113.9"), "203.0.113.9");
        assert_eq!(network_key("::ffff:203.0.113.9"), "203.0.113.9");
        assert_eq!(network_key("unknown"), "unknown");
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
