//! Tests verifying that `#[cached]` uses the global cache backend when one is
//! registered, and falls back to the per-function Moka store otherwise.
//!
//! The global cache is process-wide, so these tests hold a mutex to prevent
//! interference from other tests that also manipulate it.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use autumn_web::cache::{
    Cache, MokaCache, clear_global_cache, global_cache, make_cache_key, set_global_cache,
};
use autumn_web::prelude::*;

// ── Serialise access to GLOBAL_CACHE across all tests in this file ────────────
static LOCK: Mutex<()> = Mutex::new(());

// ── Counting cache wrapper ────────────────────────────────────────────────────

#[derive(Clone)]
struct CountingCache {
    inner: MokaCache,
    inserts: Arc<AtomicUsize>,
}

impl CountingCache {
    fn new() -> (Self, Arc<AtomicUsize>) {
        let counter = Arc::new(AtomicUsize::new(0));
        (
            Self {
                inner: MokaCache::new(100, None),
                inserts: counter.clone(),
            },
            counter,
        )
    }
}

impl Cache for CountingCache {
    fn get_value(&self, key: &str) -> Option<Arc<dyn std::any::Any + Send + Sync>> {
        self.inner.get_value(key)
    }
    fn insert_value(&self, key: &str, value: Arc<dyn std::any::Any + Send + Sync>) {
        self.inserts.fetch_add(1, Ordering::SeqCst);
        self.inner.insert_value(key, value);
    }
    fn invalidate(&self, key: &str) {
        self.inner.invalidate(key);
    }
    fn clear(&self) {
        self.inner.clear();
    }
}

// ── Cached test functions ─────────────────────────────────────────────────────

#[cached]
fn double_a(x: i32) -> i32 {
    x * 2
}

#[cached]
fn double_b(x: i32) -> i32 {
    x * 2
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// `set_global_cache` / `global_cache` / `clear_global_cache` round-trips.
#[test]
fn global_cache_registry_round_trip() {
    let _g = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    clear_global_cache();

    assert!(global_cache().is_none());

    let moka = Arc::new(MokaCache::new(10, None));
    set_global_cache(moka);
    assert!(global_cache().is_some());

    clear_global_cache();
    assert!(global_cache().is_none());
}

/// Pre-populate the global cache with a "wrong" value, then call a `#[cached]`
/// function. The function must return the pre-cached value, proving it reads
/// from the global backend rather than computing.
#[test]
fn cached_fn_reads_from_global_on_hit() {
    let _g = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    clear_global_cache();

    let global_moka = Arc::new(MokaCache::new(100, None));
    let key = make_cache_key(concat!(module_path!(), "::double_a"), &(42_i32,));
    // Store 999 under the key that double_a(42) would normally compute as 84.
    autumn_web::cache::insert(global_moka.as_ref(), &key, 999_i32);
    set_global_cache(global_moka);

    let result = double_a(42);
    assert_eq!(
        result, 999,
        "must return the global-cached value (999), not the computed value (84)"
    );

    clear_global_cache();
}

/// On a global cache miss, `#[cached]` must compute, write to the global, and
/// the global must hold the result afterward.
#[test]
fn cached_fn_writes_to_global_on_miss() {
    let _g = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    clear_global_cache();

    let (counting, inserts) = CountingCache::new();
    set_global_cache(Arc::new(counting.clone()) as Arc<dyn Cache>);

    // Ensure the key is absent from the global before calling
    let key = make_cache_key(concat!(module_path!(), "::double_b"), &(5_i32,));
    counting.inner.invalidate(&key);

    let result = double_b(5);
    assert_eq!(result, 10);
    assert_eq!(
        inserts.load(Ordering::SeqCst),
        1,
        "must insert once into the global"
    );

    // Global must now hold the computed value
    let stored = global_cache().and_then(|c| autumn_web::cache::get::<i32>(c.as_ref(), &key));
    assert_eq!(stored, Some(10), "global must hold the result after a miss");

    clear_global_cache();
}
