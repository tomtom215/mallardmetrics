use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Thread-safe query result cache with TTL expiry and LRU eviction.
#[derive(Clone)]
pub struct QueryCache {
    inner: Arc<Mutex<Inner>>,
    ttl: Duration,
    /// Maximum number of entries. 0 = unlimited.
    max_entries: usize,
    /// Running totals for the Prometheus endpoint.
    pub hits: Arc<AtomicU64>,
    pub misses: Arc<AtomicU64>,
    pub evictions: Arc<AtomicU64>,
}

struct Inner {
    entries: HashMap<String, CacheEntry>,
    /// Monotonic counter used to order entries by last access for LRU eviction.
    clock: u64,
}

struct CacheEntry {
    value: Arc<str>,
    inserted_at: Instant,
    last_used: u64,
    /// The site this entry's data belongs to, for exact-match invalidation.
    site_id: Arc<str>,
}

impl QueryCache {
    /// Create a cache with the given TTL in seconds and entry cap.
    ///
    /// - `ttl_secs = 0` — caching disabled (all lookups miss, inserts are no-ops).
    /// - `max_entries = 0` — no entry cap (bounded only by the TTL sweep).
    pub fn new(ttl_secs: u64, max_entries: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                entries: HashMap::new(),
                clock: 0,
            })),
            ttl: Duration::from_secs(ttl_secs),
            max_entries,
            hits: Arc::new(AtomicU64::new(0)),
            misses: Arc::new(AtomicU64::new(0)),
            evictions: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Returns true when caching is switched off entirely.
    pub const fn is_disabled(&self) -> bool {
        self.ttl.is_zero()
    }

    /// Look up a cached value. Returns `None` if missing or expired.
    pub fn get(&self, key: &str) -> Option<Arc<str>> {
        if self.is_disabled() {
            return None;
        }
        let result = {
            let mut inner = self.inner.lock();
            inner.clock += 1;
            let now = inner.clock;
            match inner.entries.get_mut(key) {
                Some(entry) if entry.inserted_at.elapsed() <= self.ttl => {
                    entry.last_used = now;
                    Some(Arc::clone(&entry.value))
                }
                Some(_) => {
                    // Expired: drop it now rather than waiting for the sweep.
                    inner.entries.remove(key);
                    None
                }
                None => None,
            }
        };
        if result.is_some() {
            self.hits.fetch_add(1, Ordering::Relaxed);
        } else {
            self.misses.fetch_add(1, Ordering::Relaxed);
        }
        result
    }

    /// Insert a value, evicting the least-recently-used entry if the cache is full.
    ///
    /// The previous implementation dropped the *new* entry when the cache was
    /// full, so a saturated cache froze permanently: every subsequent query
    /// missed until the TTL sweep happened to clear space, and hot keys could
    /// never displace cold ones.
    pub fn insert(&self, key: CacheKey, value: impl Into<Arc<str>>) {
        if self.is_disabled() {
            return;
        }
        let CacheKey { key, site_id } = key;
        let value = value.into();
        let mut inner = self.inner.lock();
        inner.clock += 1;
        let now = inner.clock;

        if self.max_entries > 0 && !inner.entries.contains_key(&key) {
            let ttl = self.ttl;
            if inner.entries.len() >= self.max_entries {
                inner.entries.retain(|_, e| e.inserted_at.elapsed() <= ttl);
            }
            while inner.entries.len() >= self.max_entries {
                let Some(victim) = inner
                    .entries
                    .iter()
                    .min_by_key(|(_, e)| e.last_used)
                    .map(|(k, _)| k.clone())
                else {
                    break;
                };
                inner.entries.remove(&victim);
                self.evictions.fetch_add(1, Ordering::Relaxed);
            }
        }

        inner.entries.insert(
            key,
            CacheEntry {
                value,
                inserted_at: Instant::now(),
                last_used: now,
                site_id,
            },
        );
    }

    /// Remove expired entries.
    pub fn cleanup_expired(&self) {
        let ttl = self.ttl;
        self.inner
            .lock()
            .entries
            .retain(|_, entry| entry.inserted_at.elapsed() <= ttl);
    }

    /// Drop every entry whose data belongs to `site_id`.
    ///
    /// Called after a GDPR erasure so the dashboard cannot keep serving deleted
    /// data from cache for up to `cache_ttl_secs`.
    ///
    /// The owning site is stored on the entry and compared exactly. Matching
    /// `":{site_id}:"` inside the key text was ambiguous, because a site ID may
    /// itself contain `:` (`example.com:8080` is a valid one) — so erasing
    /// `b.com` also cleared entries for `a.com:b.com`, and erasing `a.com`
    /// missed them.
    pub fn invalidate_site(&self, site_id: &str) {
        self.inner
            .lock()
            .entries
            .retain(|_, entry| &*entry.site_id != site_id);
    }

    /// Remove every entry.
    pub fn clear(&self) {
        self.inner.lock().entries.clear();
    }

    /// Number of entries currently held.
    pub fn len(&self) -> usize {
        self.inner.lock().entries.len()
    }

    /// True when the cache holds no entries.
    pub fn is_empty(&self) -> bool {
        self.inner.lock().entries.is_empty()
    }
}

/// A cache key together with the site whose data it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheKey {
    key: String,
    site_id: Arc<str>,
}

/// Build a cache key from an endpoint name, a site ID, and any extra parts.
///
/// Every component is length-prefixed, which makes the encoding injective: two
/// different `(endpoint, site_id, parts)` tuples cannot produce one key.
///
/// Joining the components with `:` was not injective, and the ambiguity was
/// reachable. A site ID may contain `:` and so may a part — funnel steps are
/// spelled `page://pricing`, for one — so `("main", "a:b", ["p"])` and
/// `("main", "a", ["b", "p"])` both rendered as `main:a:b:p:`. Two different
/// sites could therefore be served each other's cached results.
pub fn cache_key(endpoint: &str, site_id: &str, parts: &[&str]) -> CacheKey {
    use std::fmt::Write;
    let mut key = String::with_capacity(
        endpoint.len() + site_id.len() + parts.iter().map(|p| p.len() + 8).sum::<usize>() + 16,
    );
    for component in std::iter::once(endpoint)
        .chain(std::iter::once(site_id))
        .chain(parts.iter().copied())
    {
        let _ = write!(key, "{}:{component}", component.len());
    }
    CacheKey::new(key, site_id)
}

impl CacheKey {
    /// Build a key from an already-encoded string and its owning site.
    ///
    /// [`cache_key`] is the builder to use from handler code; this is the
    /// primitive it is built on, and what tests use to make a key directly.
    pub fn new(key: impl Into<String>, site_id: &str) -> Self {
        Self {
            key: key.into(),
            site_id: Arc::from(site_id),
        }
    }

    /// The encoded key, for lookups.
    pub fn as_str(&self) -> &str {
        &self.key
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A key for a single test site, so tests read as plainly as before.
    fn k(key: &str) -> CacheKey {
        CacheKey::new(key, "test.site")
    }

    #[test]
    fn test_cache_insert_and_get() {
        let cache = QueryCache::new(60, 0);
        cache.insert(k("key1"), "value1");
        assert_eq!(cache.get("key1").as_deref(), Some("value1"));
    }

    #[test]
    fn test_cache_miss() {
        let cache = QueryCache::new(60, 0);
        assert!(cache.get("nonexistent").is_none());
    }

    #[test]
    fn test_cache_disabled_with_zero_ttl() {
        let cache = QueryCache::new(0, 0);
        cache.insert(k("key1"), "value1");
        assert!(cache.get("key1").is_none());
        assert_eq!(cache.len(), 0);
        assert!(cache.is_disabled());
    }

    #[test]
    fn test_cache_len() {
        let cache = QueryCache::new(60, 0);
        assert_eq!(cache.len(), 0);
        cache.insert(k("a"), "1");
        cache.insert(k("b"), "2");
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn test_cache_overwrite() {
        let cache = QueryCache::new(60, 0);
        cache.insert(k("key"), "old");
        cache.insert(k("key"), "new");
        assert_eq!(cache.get("key").as_deref(), Some("new"));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn test_cache_cleanup() {
        let cache = QueryCache::new(0, 0);
        cache.cleanup_expired();
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn test_cache_clone_shares_state() {
        let cache1 = QueryCache::new(60, 0);
        let cache2 = cache1.clone();
        cache1.insert(k("shared"), "data");
        assert_eq!(cache2.get("shared").as_deref(), Some("data"));
    }

    #[test]
    fn test_cache_evicts_least_recently_used_when_full() {
        let cache = QueryCache::new(60, 3);
        cache.insert(k("a"), "1");
        cache.insert(k("b"), "2");
        cache.insert(k("c"), "3");
        assert_eq!(cache.len(), 3);

        // Touch a and c so b becomes the least recently used.
        assert!(cache.get("a").is_some());
        assert!(cache.get("c").is_some());

        cache.insert(k("d"), "4");

        assert_eq!(cache.len(), 3, "cap must be respected");
        assert!(
            cache.get("d").is_some(),
            "a new entry must be admitted, not dropped"
        );
        assert!(cache.get("b").is_none(), "the LRU entry must be evicted");
        assert!(cache.get("a").is_some());
        assert!(cache.get("c").is_some());
        assert!(cache.evictions.load(Ordering::Relaxed) >= 1);
    }

    #[test]
    fn test_full_cache_does_not_freeze() {
        // Regression: previously a full cache rejected every new key, so a
        // saturated cache served nothing but misses until the TTL swept it.
        let cache = QueryCache::new(60, 2);
        for i in 0..20 {
            cache.insert(k(&format!("k{i}")), format!("v{i}"));
        }
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.get("k19").as_deref(), Some("v19"));
    }

    #[test]
    fn test_overwriting_existing_key_does_not_evict() {
        let cache = QueryCache::new(60, 2);
        cache.insert(k("a"), "1");
        cache.insert(k("b"), "2");
        cache.insert(k("a"), "1b");
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.get("a").as_deref(), Some("1b"));
        assert_eq!(cache.get("b").as_deref(), Some("2"));
    }

    #[test]
    fn test_cache_hits_misses_counters() {
        let cache = QueryCache::new(60, 0);
        cache.insert(k("k"), "v");
        assert!(cache.get("k").is_some());
        assert!(cache.get("nope").is_none());
        assert_eq!(cache.hits.load(Ordering::Relaxed), 1);
        assert_eq!(cache.misses.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_expired_entry_is_a_miss_and_is_dropped() {
        let cache = QueryCache::new(1, 0);
        cache.insert(k("k"), "v");
        // Rewind the insertion time past the TTL rather than sleeping.
        {
            let mut inner = cache.inner.lock();
            let entry = inner.entries.get_mut("k").unwrap();
            entry.inserted_at = entry
                .inserted_at
                .checked_sub(Duration::from_secs(5))
                .expect("test clock is far enough from the epoch");
        }
        assert!(cache.get("k").is_none());
        assert_eq!(cache.len(), 0, "an expired entry must not linger");
    }

    #[test]
    fn test_cache_key_shape() {
        assert_eq!(
            cache_key("main", "a.com", &["d1", "d2"]).as_str(),
            "4:main5:a.com2:d12:d2"
        );
        assert_eq!(cache_key("main", "a.com", &[]).as_str(), "4:main5:a.com");
    }

    #[test]
    fn test_cache_key_is_injective() {
        // Regression: components were joined with ':', but both a site ID and a
        // part may contain ':' — so two different queries, for two different
        // sites, produced one key and were served each other's results.
        assert_ne!(
            cache_key("main", "a:b", &["p"]).as_str(),
            cache_key("main", "a", &["b", "p"]).as_str()
        );
        assert_ne!(
            cache_key("main", "a", &["bc"]).as_str(),
            cache_key("main", "ab", &["c"]).as_str()
        );
        assert_ne!(
            cache_key("mainx", "a", &[]).as_str(),
            cache_key("main", "xa", &[]).as_str()
        );
        assert_ne!(
            cache_key("main", "a", &["b", ""]).as_str(),
            cache_key("main", "a", &["b"]).as_str()
        );
    }

    #[test]
    fn test_colliding_keys_do_not_share_a_cache_entry() {
        let cache = QueryCache::new(60, 0);
        cache.insert(cache_key("main", "a:b", &["p"]), "site-a-colon-b");
        cache.insert(cache_key("main", "a", &["b", "p"]), "site-a");
        assert_eq!(
            cache
                .get(cache_key("main", "a:b", &["p"]).as_str())
                .as_deref(),
            Some("site-a-colon-b")
        );
        assert_eq!(
            cache
                .get(cache_key("main", "a", &["b", "p"]).as_str())
                .as_deref(),
            Some("site-a")
        );
    }

    #[test]
    fn test_invalidate_site_is_exact_even_when_ids_contain_colons() {
        // `example.com:8080` is a valid site ID, so substring matching on the
        // key text could not tell these two apart.
        let cache = QueryCache::new(60, 0);
        cache.insert(cache_key("main", "example.com:8080", &["x"]), "with-port");
        cache.insert(cache_key("main", "example.com", &["x"]), "without-port");

        cache.invalidate_site("example.com");

        assert!(
            cache
                .get(cache_key("main", "example.com", &["x"]).as_str())
                .is_none()
        );
        assert!(
            cache
                .get(cache_key("main", "example.com:8080", &["x"]).as_str())
                .is_some(),
            "a different site that merely shares a prefix must survive"
        );
    }

    #[test]
    fn test_invalidate_site_removes_only_that_site() {
        let cache = QueryCache::new(60, 0);
        cache.insert(cache_key("main", "a.com", &["x"]), "1");
        cache.insert(cache_key("pages", "a.com", &["x"]), "2");
        cache.insert(cache_key("main", "b.com", &["x"]), "3");

        cache.invalidate_site("a.com");

        assert!(
            cache
                .get(cache_key("main", "a.com", &["x"]).as_str())
                .is_none()
        );
        assert!(
            cache
                .get(cache_key("pages", "a.com", &["x"]).as_str())
                .is_none()
        );
        assert!(
            cache
                .get(cache_key("main", "b.com", &["x"]).as_str())
                .is_some()
        );
    }

    #[test]
    fn test_invalidate_site_does_not_match_substring_sites() {
        let cache = QueryCache::new(60, 0);
        cache.insert(cache_key("main", "example.com", &["x"]), "1");
        cache.invalidate_site("ample.com");
        assert!(
            cache
                .get(cache_key("main", "example.com", &["x"]).as_str())
                .is_some(),
            "a site whose name is a substring of another must not be invalidated"
        );
    }

    #[test]
    fn test_clear() {
        let cache = QueryCache::new(60, 0);
        cache.insert(k("a"), "1");
        cache.clear();
        assert!(cache.is_empty());
    }
}

#[cfg(test)]
mod prop_tests {
    use super::*;
    use proptest::prelude::*;

    fn k(key: &str) -> CacheKey {
        CacheKey::new(key, "test.site")
    }

    proptest! {
        /// Round-trip: a value inserted with a positive TTL is retrievable.
        #[test]
        fn prop_cache_round_trip(
            key in "[a-z]{1,20}",
            value in "[A-Za-z0-9]{1,100}",
            ttl in 1u64..3600u64,
        ) {
            let cache = QueryCache::new(ttl, 0);
            cache.insert(k(&key), value.clone());
            let found = cache.get(&key);
            prop_assert_eq!(found.as_deref(), Some(value.as_str()));
        }

        /// A disabled cache always misses.
        #[test]
        fn prop_cache_disabled_always_misses(
            key in "[a-z]{1,20}",
            value in "[A-Za-z0-9]{1,100}",
        ) {
            let cache = QueryCache::new(0, 0);
            cache.insert(k(&key), value);
            prop_assert!(cache.get(&key).is_none());
        }

        /// The entry cap is never exceeded, regardless of insertion order.
        #[test]
        fn prop_cache_respects_cap(
            cap in 1usize..16usize,
            inserts in 1usize..200usize,
        ) {
            let cache = QueryCache::new(600, cap);
            for i in 0..inserts {
                cache.insert(k(&format!("k{i}")), "v");
            }
            prop_assert!(cache.len() <= cap);
            // The most recent insert always survives.
            let newest = format!("k{}", inserts - 1);
            prop_assert!(cache.get(&newest).is_some(), "newest key was evicted");
        }
    }
}
