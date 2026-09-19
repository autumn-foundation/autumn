//! Default cache backend powered by [moka](https://docs.rs/moka).
//!
//! `MokaCache` wraps `moka::sync::Cache` with type-erased values
//! (`Arc<dyn Any>`) so a single instance can serve all `#[cached]`
//! functions and the `CacheResponseLayer`.

use std::any::Any;
use std::sync::Arc;
use std::time::Duration;

use moka::sync::Cache as SyncCache;

use super::Cache;

/// Lock-free, in-process cache backed by moka.
///
/// This is the default `Cache` implementation when the `cache-moka`
/// feature is enabled. Supports max-capacity (LRU eviction) and
/// time-to-live expiration.
///
/// # Examples
///
/// ```rust
/// use autumn_web::cache::{self, MokaCache};
///
/// let c = MokaCache::builder().max_capacity(100).build();
/// cache::insert(&c, "key", 42_i32);
/// assert_eq!(cache::get::<i32>(&c, "key"), Some(42));
/// ```
#[derive(Clone)]
pub struct MokaCache {
    inner: SyncCache<String, Arc<dyn Any + Send + Sync>>,
}

/// Builder for constructing a [`MokaCache`] with capacity and TTL settings.
pub struct MokaCacheBuilder {
    max_capacity: u64,
    ttl: Option<Duration>,
}

impl MokaCacheBuilder {
    /// Set the maximum number of entries (default: 10,000).
    #[must_use]
    pub const fn max_capacity(mut self, max: u64) -> Self {
        self.max_capacity = max;
        self
    }

    /// Set the time-to-live for entries. `None` means entries never expire
    /// based on time (only evicted by capacity pressure).
    #[must_use]
    pub const fn ttl(mut self, ttl: Duration) -> Self {
        self.ttl = Some(ttl);
        self
    }

    /// Build the [`MokaCache`].
    #[must_use]
    pub fn build(self) -> MokaCache {
        let mut builder = SyncCache::builder().max_capacity(self.max_capacity);
        if let Some(ttl) = self.ttl {
            builder = builder.time_to_live(ttl);
        }
        MokaCache {
            inner: builder.build(),
        }
    }
}

impl MokaCache {
    /// Start building a new `MokaCache`.
    #[must_use]
    pub const fn builder() -> MokaCacheBuilder {
        MokaCacheBuilder {
            max_capacity: 10_000,
            ttl: None,
        }
    }

    /// Create a `MokaCache` directly from capacity and optional TTL.
    ///
    /// This is the constructor used by `#[cached]` macro-generated code.
    #[must_use]
    pub fn new(max_capacity: u64, ttl: Option<Duration>) -> Self {
        let mut b = Self::builder().max_capacity(max_capacity);
        if let Some(ttl) = ttl {
            b = b.ttl(ttl);
        }
        b.build()
    }
}

impl Cache for MokaCache {
    fn get_value(&self, key: &str) -> Option<Arc<dyn Any + Send + Sync>> {
        self.inner.get(key)
    }

    fn insert_value(&self, key: &str, value: Arc<dyn Any + Send + Sync>) {
        self.inner.insert(key.to_owned(), value);
    }

    fn invalidate_namespace(&self, namespace: &str) -> bool {
        // Iterate rather than `invalidate_entries_if`: moka's predicate API
        // requires the cache to be built with `support_invalidation_closures()`,
        // which taxes every ordinary get/insert, and applies lazily on the next
        // read. Namespace invalidation is rare and must take effect
        // immediately, so paying O(entries) here — and nothing on the hot path
        // — is the right trade.
        let prefix = format!("{namespace}:");
        let doomed: Vec<String> = self
            .inner
            .iter()
            .map(|(k, _)| (*k).clone())
            .filter(|k| k.starts_with(&prefix))
            .collect();
        for key in doomed {
            self.inner.invalidate(&key);
        }
        true
    }

    fn invalidate(&self, key: &str) {
        self.inner.invalidate(key);
    }

    fn clear(&self) {
        self.inner.invalidate_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache;
    use std::thread;

    #[test]
    fn basic_insert_and_get() {
        let c = MokaCache::new(100, None);
        cache::insert(&c, "a", 1_i32);
        assert_eq!(cache::get::<i32>(&c, "a"), Some(1));
        assert_eq!(cache::get::<i32>(&c, "b"), None);
    }

    #[test]
    fn type_mismatch_returns_none() {
        let c = MokaCache::new(100, None);
        cache::insert(&c, "a", 1_i32);
        assert_eq!(cache::get::<String>(&c, "a"), None);
    }

    #[test]
    fn ttl_expiry() {
        let c = MokaCache::new(100, Some(Duration::from_millis(50)));
        cache::insert(&c, "a", 1_i32);
        assert_eq!(cache::get::<i32>(&c, "a"), Some(1));
        thread::sleep(Duration::from_millis(80));
        c.inner.run_pending_tasks();
        assert_eq!(cache::get::<i32>(&c, "a"), None);
    }

    #[test]
    fn max_capacity_evicts() {
        // Moka eviction is async; insert many more entries than capacity
        // and verify the cache eventually stabilises below the limit.
        let c = MokaCache::new(10, None);
        for i in 0..100 {
            cache::insert(&c, &format!("k{i}"), i);
            // Periodically flush to give moka a chance to evict.
            if i % 20 == 0 {
                c.inner.run_pending_tasks();
            }
        }
        c.inner.run_pending_tasks();
        let count = (0..100)
            .filter(|i| cache::get::<i32>(&c, &format!("k{i}")).is_some())
            .count();
        // Moka may temporarily overshoot, but should be in the right
        // ballpark. Allow some slack.
        assert!(
            count <= 20,
            "expected roughly <=10 entries (with slack), got {count}"
        );
        assert!(count > 0, "cache should not be empty");
    }

    #[test]
    fn clear_removes_all() {
        let c = MokaCache::new(100, None);
        cache::insert(&c, "a", 1_i32);
        cache::insert(&c, "b", 2_i32);
        c.clear();
        c.inner.run_pending_tasks();
        assert_eq!(cache::get::<i32>(&c, "a"), None);
        assert_eq!(cache::get::<i32>(&c, "b"), None);
    }

    #[test]
    fn invalidate_removes_key() {
        let c = MokaCache::new(100, None);
        cache::insert(&c, "a", 1_i32);
        c.invalidate("a");
        c.inner.run_pending_tasks();
        assert_eq!(cache::get::<i32>(&c, "a"), None);
    }

    #[test]
    fn concurrent_access() {
        let c = MokaCache::new(1000, None);
        let handles: Vec<_> = (0_i32..10)
            .map(|i| {
                let c = c.clone();
                thread::spawn(move || {
                    let key = format!("key-{i}");
                    cache::insert(&c, &key, i * 10);
                    (i, cache::get::<i32>(&c, &key))
                })
            })
            .collect();

        for h in handles {
            let (i, val) = h.join().unwrap();
            assert_eq!(val, Some(i * 10));
        }
    }

    #[test]
    fn heterogeneous_types() {
        let c = MokaCache::new(100, None);
        cache::insert(&c, "int", 42_i32);
        cache::insert(&c, "string", "hello".to_string());
        cache::insert(&c, "vec", vec![1_u8, 2, 3]);

        assert_eq!(cache::get::<i32>(&c, "int"), Some(42));
        assert_eq!(
            cache::get::<String>(&c, "string"),
            Some("hello".to_string())
        );
        assert_eq!(cache::get::<Vec<u8>>(&c, "vec"), Some(vec![1, 2, 3]));
    }

    #[test]
    fn builder_pattern() {
        let c = MokaCache::builder()
            .max_capacity(500)
            .ttl(Duration::from_secs(60))
            .build();
        cache::insert(&c, "x", 99_i32);
        assert_eq!(cache::get::<i32>(&c, "x"), Some(99));
    }
}
