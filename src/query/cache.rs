use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Thread-safe query result cache with TTL-based expiration.
#[derive(Clone)]
pub struct QueryCache {
    entries: Arc<Mutex<HashMap<String, CacheEntry>>>,
    ttl: Duration,
}

struct CacheEntry {
    value: String,
    inserted_at: Instant,
}

impl QueryCache {
    /// Create a new cache with the given TTL in seconds.
    /// A TTL of 0 disables caching (all lookups miss).
    pub fn new(ttl_secs: u64) -> Self {
        Self {
            entries: Arc::new(Mutex::new(HashMap::new())),
            ttl: Duration::from_secs(ttl_secs),
        }
    }

    /// Look up a cached value by key. Returns `None` if missing or expired.
    pub fn get(&self, key: &str) -> Option<String> {
        if self.ttl.is_zero() {
            return None;
        }
        self.entries.lock().get(key).and_then(|entry| {
            if entry.inserted_at.elapsed() > self.ttl {
                None
            } else {
                Some(entry.value.clone())
            }
        })
    }

    /// Insert a value into the cache.
    pub fn insert(&self, key: String, value: String) {
        if self.ttl.is_zero() {
            return;
        }
        let mut entries = self.entries.lock();
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
        let cache = QueryCache::new(60);
        cache.insert("key1".to_string(), "value1".to_string());
        assert_eq!(cache.get("key1"), Some("value1".to_string()));
    }

    #[test]
    fn test_cache_miss() {
        let cache = QueryCache::new(60);
        assert_eq!(cache.get("nonexistent"), None);
    }

    #[test]
    fn test_cache_disabled_with_zero_ttl() {
        let cache = QueryCache::new(0);
        cache.insert("key1".to_string(), "value1".to_string());
        assert_eq!(cache.get("key1"), None);
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn test_cache_len() {
        let cache = QueryCache::new(60);
        assert_eq!(cache.len(), 0);
        cache.insert("a".to_string(), "1".to_string());
        cache.insert("b".to_string(), "2".to_string());
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn test_cache_overwrite() {
        let cache = QueryCache::new(60);
        cache.insert("key".to_string(), "old".to_string());
        cache.insert("key".to_string(), "new".to_string());
        assert_eq!(cache.get("key"), Some("new".to_string()));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn test_cache_cleanup() {
        let cache = QueryCache::new(0);
        // With zero TTL, nothing is inserted
        cache.cleanup_expired();
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn test_cache_clone_shares_state() {
        let cache1 = QueryCache::new(60);
        let cache2 = cache1.clone();
        cache1.insert("shared".to_string(), "data".to_string());
        assert_eq!(cache2.get("shared"), Some("data".to_string()));
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
            let cache = QueryCache::new(ttl);
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
            let cache = QueryCache::new(0);
            cache.insert(key.clone(), value);
            prop_assert_eq!(cache.get(&key), None);
        }
    }
}
