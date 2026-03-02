use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Thread-safe query result cache with TTL-based expiration and optional entry cap.
#[derive(Clone)]
pub struct QueryCache {
    entries: Arc<Mutex<HashMap<String, CacheEntry>>>,
    ttl: Duration,
    /// Maximum number of entries.  0 = unlimited.
    max_entries: usize,
    /// Running totals for Prometheus metrics.
    pub hits: Arc<std::sync::atomic::AtomicU64>,
    pub misses: Arc<std::sync::atomic::AtomicU64>,
}

struct CacheEntry {
    value: String,
    inserted_at: Instant,
}

impl QueryCache {
    /// Create a new cache with the given TTL in seconds and optional entry cap.
    ///
    /// - `ttl_secs = 0` — caching disabled (all lookups miss, inserts are no-ops).
    /// - `max_entries = 0` — no entry count limit (unbounded growth between GC runs).
    pub fn new(ttl_secs: u64, max_entries: usize) -> Self {
        Self {
            entries: Arc::new(Mutex::new(HashMap::new())),
            ttl: Duration::from_secs(ttl_secs),
            max_entries,
            hits: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            misses: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    /// Look up a cached value by key. Returns `None` if missing or expired.
    pub fn get(&self, key: &str) -> Option<String> {
        if self.ttl.is_zero() {
            return None;
        }
        let result = self.entries.lock().get(key).and_then(|entry| {
            if entry.inserted_at.elapsed() > self.ttl {
                None
            } else {
                Some(entry.value.clone())
            }
        });
        if result.is_some() {
            self.hits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        } else {
            self.misses
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        result
    }

    /// Insert a value into the cache.
    ///
    /// If `max_entries > 0` and the cache is full, expired entries are evicted
    /// first.  If the cache is still full after eviction the insert is silently
    /// skipped (entry counts as a capacity miss — the query result is still
    /// returned to the caller, just not cached).
    pub fn insert(&self, key: String, value: String) {
        if self.ttl.is_zero() {
            return;
        }
        let mut entries = self.entries.lock();
        // Enforce max entry cap: evict expired entries first, then drop if still full.
        if self.max_entries > 0 && entries.len() >= self.max_entries {
            let ttl = self.ttl;
            entries.retain(|_, e| e.inserted_at.elapsed() <= ttl);
            if entries.len() >= self.max_entries {
                return; // still full after GC — skip insert
            }
        }
        entries.insert(
            key,
            CacheEntry {
                value,
                inserted_at: Instant::now(),
            },
        );
    }

    /// Remove expired entries from the cache.
    pub fn cleanup_expired(&self) {
        let mut entries = self.entries.lock();
        entries.retain(|_, entry| entry.inserted_at.elapsed() <= self.ttl);
    }

    /// Returns the number of entries currently in the cache.
    pub fn len(&self) -> usize {
        self.entries.lock().len()
    }

    /// Returns `true` if the cache contains no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.lock().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_insert_and_get() {
        let cache = QueryCache::new(60, 0);
        cache.insert("key1".to_string(), "value1".to_string());
        assert_eq!(cache.get("key1"), Some("value1".to_string()));
    }

    #[test]
    fn test_cache_miss() {
        let cache = QueryCache::new(60, 0);
        assert_eq!(cache.get("nonexistent"), None);
    }

    #[test]
    fn test_cache_disabled_with_zero_ttl() {
        let cache = QueryCache::new(0, 0);
        cache.insert("key1".to_string(), "value1".to_string());
        assert_eq!(cache.get("key1"), None);
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn test_cache_len() {
        let cache = QueryCache::new(60, 0);
        assert_eq!(cache.len(), 0);
        cache.insert("a".to_string(), "1".to_string());
        cache.insert("b".to_string(), "2".to_string());
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn test_cache_overwrite() {
        let cache = QueryCache::new(60, 0);
        cache.insert("key".to_string(), "old".to_string());
        cache.insert("key".to_string(), "new".to_string());
        assert_eq!(cache.get("key"), Some("new".to_string()));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn test_cache_cleanup() {
        let cache = QueryCache::new(0, 0);
        // With zero TTL, nothing is inserted
        cache.cleanup_expired();
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn test_cache_clone_shares_state() {
        let cache1 = QueryCache::new(60, 0);
        let cache2 = cache1.clone();
        cache1.insert("shared".to_string(), "data".to_string());
        assert_eq!(cache2.get("shared"), Some("data".to_string()));
    }

    #[test]
    fn test_cache_max_entries_cap() {
        let cache = QueryCache::new(60, 3);
        cache.insert("a".to_string(), "1".to_string());
        cache.insert("b".to_string(), "2".to_string());
        cache.insert("c".to_string(), "3".to_string());
        assert_eq!(cache.len(), 3);
        // 4th insert should be rejected (no expired entries to evict)
        cache.insert("d".to_string(), "4".to_string());
        assert_eq!(
            cache.len(),
            3,
            "insert beyond max_entries should be silently dropped"
        );
        // Previously inserted values still accessible
        assert!(cache.get("a").is_some() || cache.get("b").is_some() || cache.get("c").is_some());
    }

    #[test]
    fn test_cache_hits_misses_counters() {
        use std::sync::atomic::Ordering;
        let cache = QueryCache::new(60, 0);
        cache.insert("k".to_string(), "v".to_string());
        assert!(cache.get("k").is_some()); // hit
        assert!(cache.get("nope").is_none()); // miss
        assert_eq!(cache.hits.load(Ordering::Relaxed), 1);
        assert_eq!(cache.misses.load(Ordering::Relaxed), 1);
    }
}

#[cfg(test)]
mod prop_tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// Round-trip: a value inserted with a positive TTL is immediately retrievable.
        #[test]
        fn prop_cache_round_trip(
            key in "[a-z]{1,20}",
            value in "[A-Za-z0-9]{1,100}",
            ttl in 1u64..3600u64,
        ) {
            let cache = QueryCache::new(ttl, 0);
            cache.insert(key.clone(), value.clone());
            prop_assert_eq!(cache.get(&key), Some(value));
        }

        /// Expiry: a cache with TTL=0 behaves as if always disabled — inserts are no-ops
        /// and all lookups return None.
        #[test]
        fn prop_cache_disabled_always_misses(
            key in "[a-z]{1,20}",
            value in "[A-Za-z0-9]{1,100}",
        ) {
            let cache = QueryCache::new(0, 0);
            cache.insert(key.clone(), value);
            prop_assert_eq!(cache.get(&key), None);
        }
    }
}
