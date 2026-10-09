//! Caching infrastructure for the Autumn framework.
//!
//! This module provides:
//!
//! - [`Cache`] — a trait abstracting over cache backends (moka by default,
//!   swap in Redis, memcached, etc.)
//! - [`MokaCache`] — the default, lock-free, in-process cache powered by
//!   [moka](https://docs.rs/moka) (behind the `cache-moka` feature)
//! - [`CacheResponseLayer`] — a Tower middleware that caches HTTP GET
//!   responses, usable via `#[intercept(CacheResponseLayer::new(...))]`
//! - [`CacheableResult`] — helper trait used by `#[cached(result)]` to
//!   only cache `Ok` values
//!
//! The `#[cached]` proc macro generates a per-function static `MokaCache`
//! for function-level memoization. The `CacheResponseLayer` operates at
//! the HTTP level using a shared `Arc<dyn Cache>`.
//!
//! # Swapping backends
//!
//! Implement the [`Cache`] trait for your backend:
//!
//! ```rust,ignore
//! use autumn_web::cache::Cache;
//!
//! #[derive(Clone)]
//! struct RedisCache { /* ... */ }
//!
//! impl Cache for RedisCache {
//!     fn get_value(&self, key: &str) -> Option<Box<dyn std::any::Any + Send + Sync>> { /* ... */ }
//!     fn insert_value(&self, key: &str, value: Box<dyn std::any::Any + Send + Sync>) { /* ... */ }
//!     fn invalidate(&self, key: &str) { /* ... */ }
//!     fn clear(&self) { /* ... */ }
//! }
//! ```

pub mod coherence;
#[cfg(feature = "maud")]
mod fragment;
mod layer;
#[cfg(feature = "cache-moka")]
mod moka_impl;
mod read_through;

#[cfg(feature = "maud")]
pub use fragment::{
    cache_fragment, cache_fragment_global, cache_fragment_global_in, cache_fragment_in,
};
pub use layer::{CacheResponseLayer, CacheResponseService};
#[cfg(feature = "cache-moka")]
pub use moka_impl::MokaCache;
pub use read_through::{
    CacheFillError, GetOrComputeOptions, ReadThroughMetrics, ReadThroughMetricsSnapshot,
    get_or_compute, get_or_compute_with, jittered_ttl, read_through_metrics,
};

use std::any::Any;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Arc, RwLock};
use std::time::Duration;

// ── Global cache registry ────────────────────────────────────────────

/// Process-level shared cache backend.
///
/// Set once at startup by [`set_global_cache`]; read by every
/// `#[cached]`-annotated function to decide which store to use.
static GLOBAL_CACHE: RwLock<Option<Arc<dyn Cache>>> = RwLock::new(None);

/// Register (or replace) the process-level shared cache.
///
/// Called automatically by [`crate::app::AppBuilder`] when
/// `.with_cache_backend(...)` has been used. Also called by
/// [`crate::state::AppState::set_cache`] when a plugin installs a backend
/// during the startup-hook phase.
///
/// With the `reporting` feature the backend is wrapped once, so a failure
/// capsule records its removals and a replay never reaches it. Thus
/// [`global_cache`] returns the wrapper, not the `Arc` given here.
///
/// # Panics
///
/// Panics if the internal `RwLock` is poisoned.
pub fn set_global_cache(cache: Arc<dyn Cache>) {
    *GLOBAL_CACHE.write().expect("global cache lock poisoned") = Some(with_capsule_seam(cache));
}

/// Return a clone of the process-level shared cache, if one is registered.
///
/// `None` means no global backend has been set and `#[cached]` functions
/// fall back to their per-function Moka stores.
///
/// # Panics
///
/// Panics if the internal `RwLock` is poisoned.
#[must_use]
pub fn global_cache() -> Option<Arc<dyn Cache>> {
    GLOBAL_CACHE
        .read()
        .expect("global cache lock poisoned")
        .clone()
}

/// Set the global cache for `autumn replay` (#2351).
///
/// When production had a global cache, replay installs the capsule seam over
/// a backend that stores nothing. A cache call then takes the path it took in
/// production, and the tape answers it. Otherwise replay has no global cache.
///
/// Returns the installed cache. The caller also puts it in the app state, as
/// `AppBuilder::with_cache_backend` does in production.
#[cfg(feature = "reporting")]
pub(crate) fn install_replay_cache(
    recorded: &crate::capsule::CapsuleEffects,
) -> Option<Arc<dyn Cache>> {
    if recorded.global_cache {
        set_global_cache(Arc::new(ReplayBackend));
    } else {
        clear_global_cache();
    }
    global_cache()
}

/// The backend under the replay seam. It stores nothing.
#[cfg(feature = "reporting")]
struct ReplayBackend;

#[cfg(feature = "reporting")]
impl Cache for ReplayBackend {
    fn get_value(&self, _key: &str) -> Option<Arc<dyn Any + Send + Sync>> {
        None
    }
    fn insert_value(&self, _key: &str, _value: Arc<dyn Any + Send + Sync>) {}
    fn invalidate(&self, _key: &str) {}
    fn clear(&self) {}
    fn invalidate_namespace(&self, _namespace: &str) -> bool {
        true
    }
}

/// Remove the process-level shared cache.
///
/// Primarily useful in tests that need per-test isolation.
///
/// # Panics
///
/// Panics if the internal `RwLock` is poisoned.
pub fn clear_global_cache() {
    *GLOBAL_CACHE.write().expect("global cache lock poisoned") = None;
}

/// A boxed future returned by the async [`Cache`] methods.
///
/// Boxed so that `Cache` stays usable as `dyn Cache`.
pub type CacheFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// An invalidation that did not complete. Stale data can still be served.
///
/// Returned by [`Cache::invalidate_async`] and
/// [`Cache::invalidate_namespace_async`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("cache invalidation failed after {attempts} attempt(s): {reason}")]
pub struct InvalidationError {
    attempts: u32,
    reason: String,
}

impl InvalidationError {
    /// Make an error for an invalidation that failed after `attempts` tries.
    pub fn new(attempts: u32, reason: impl Into<String>) -> Self {
        Self {
            attempts,
            reason: reason.into(),
        }
    }

    /// How many times the backend tried.
    #[must_use]
    pub const fn attempts(&self) -> u32 {
        self.attempts
    }

    /// The last backend error, as text.
    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

/// Invalidations that failed after all retries, in this process.
static INVALIDATION_FAILURES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Count one invalidation that failed after all retries.
///
/// Shown as `autumn_cache_invalidation_failures_total`. Call it only where you
/// drop the error. Do not call it where you return the error. Then each
/// failure counts one time. A custom backend calls it from a sync method that
/// cannot return the error.
pub fn record_invalidation_failure() {
    INVALIDATION_FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Total of [`record_invalidation_failure`] calls in this process.
#[must_use]
pub fn invalidation_failures_total() -> u64 {
    INVALIDATION_FAILURES.load(std::sync::atomic::Ordering::Relaxed)
}

/// Serializes same-process test code that mutates [`GLOBAL_CACHE`] via
/// [`set_global_cache`]/[`clear_global_cache`].
///
/// `cargo test` runs a crate's unit tests on parallel threads in one
/// process, so two tests racing on this process-wide singleton can
/// interleave — e.g. one test's `clear_global_cache()` landing between
/// another's set-then-read sequence. Every same-process caller that
/// mutates the global cache for test purposes — [`crate::test::TestApp::build`]
/// and the `cache::fragment` unit tests — acquires this lock for the
/// duration of its critical section so they mutually exclude each other.
/// Poison-tolerant: acquire with `.lock().unwrap_or_else(PoisonError::into_inner)`
/// so one panicking test doesn't cascade into failing an unrelated one. See
/// issue #2218.
pub(crate) static GLOBAL_CACHE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

// ── Cache trait ──────────────────────────────────────────────────────

/// Raw JSON bytes stored by serializing cache backends (e.g. Redis).
///
/// Backends that cannot store `Arc<dyn Any>` directly (because values must
/// survive across process boundaries) return this from [`Cache::get_value`]
/// instead. [`get_cached`] and [`insert_cached`] transparently deserialize it
/// back into the concrete type `V` using `serde_json`.
#[derive(Clone)]
pub struct RawCacheBytes(pub Vec<u8>);

/// A type-erased, thread-safe cache store.
///
/// Implementations must be `Send + Sync` so they can be shared across
/// handlers and tasks. Values are stored as `Arc<dyn Any>` for type
/// erasure, allowing a single cache instance to store heterogeneous
/// types from different `#[cached]` functions.
///
/// Use the free functions [`get`] / [`insert`] for in-process-only values,
/// or [`get_cached`] / [`insert_cached`] for types that also implement
/// `serde`, which is required for cross-replica backends like Redis.
/// [`CacheResponseLayer`] uses the serde-aware path so HTTP response caching
/// works with both in-process and raw-byte backends.
pub trait Cache: Send + Sync + 'static {
    /// Retrieve a type-erased value by key. Returns `None` on miss.
    ///
    /// Backends that store serialized data (e.g. Redis) may return
    /// <code>Arc<[RawCacheBytes]></code> here; [`get_cached`] handles the
    /// JSON deserialization transparently.
    fn get_value(&self, key: &str) -> Option<Arc<dyn Any + Send + Sync>>;

    /// Store a type-erased value by key.
    fn insert_value(&self, key: &str, value: Arc<dyn Any + Send + Sync>);

    /// Remove a specific key.
    ///
    /// This method cannot report a failure. A backend that can fail must log
    /// the failure and call [`record_invalidation_failure`]. Prefer
    /// [`invalidate_async`](Cache::invalidate_async).
    fn invalidate(&self, key: &str);

    /// Remove all entries.
    fn clear(&self);

    /// Drop every entry belonging to one cached read — i.e. every key under
    /// `"{namespace}:"`, the prefix
    /// [`make_cache_key`] stamps on each of that read's keys.
    ///
    /// Returns whether the backend could actually do it. The default is
    /// `false`: a backend that cannot enumerate or pattern-match its key space
    /// must say so rather than silently leave stale entries behind, because the
    /// build-time coherence gate (issue #1716) reports this answer to the
    /// caller verbatim. Backends that CAN do it — `MokaCache` by iteration,
    /// `RedisCache` by `SCAN MATCH` — override it and return `true`.
    fn invalidate_namespace(&self, _namespace: &str) -> bool {
        false
    }

    /// Store pre-serialized JSON bytes for backends that persist data across
    /// process boundaries (e.g. Redis). The default is a no-op; in-process
    /// backends store values via [`insert_value`] instead.
    ///
    /// `ttl` carries the same time-to-live that was declared on the
    /// `#[cached(ttl = "…")]` attribute so backends can apply native expiry
    /// (e.g. Redis `SET EX`). `None` means no expiry.
    ///
    /// [`insert_value`]: Cache::insert_value
    fn insert_raw_bytes(&self, _key: &str, _bytes: Vec<u8>, _ttl: Option<std::time::Duration>) {}

    /// Try to acquire a cross-replica fill lock for `key`, used by
    /// [`get_or_compute_with`] to ensure at most one replica refills a hot
    /// key at a time.
    ///
    /// `token` identifies the caller so [`release_fill_lock`] can safely
    /// release only a lock it still owns. `ttl` bounds how long the lock is
    /// held if the caller crashes before releasing it.
    ///
    /// The default implementation reports [`FillLockStatus::Unsupported`],
    /// which degrades callers to in-process-only single-flight protection —
    /// safe for backends (like the in-process Moka cache) that have no
    /// cross-replica visibility.
    ///
    /// [`release_fill_lock`]: Cache::release_fill_lock
    fn try_acquire_fill_lock(&self, _key: &str, _token: &str, _ttl: Duration) -> FillLockStatus {
        FillLockStatus::Unsupported
    }

    /// Release the fill lock previously acquired with `token`, if this
    /// caller still owns it. The default is a no-op, matching the default
    /// [`try_acquire_fill_lock`] returning [`FillLockStatus::Unsupported`].
    ///
    /// [`try_acquire_fill_lock`]: Cache::try_acquire_fill_lock
    fn release_fill_lock(&self, _key: &str, _token: &str) {}

    /// Remove a specific key, and return a failure.
    ///
    /// The default calls [`invalidate`](Cache::invalidate) and returns `Ok`.
    /// A backend that does I/O (for example Redis) overrides it with a real
    /// async call that does not block a runtime worker, and returns the error.
    ///
    /// # Errors
    ///
    /// [`InvalidationError`] when the key could not be removed. The old value
    /// can still be served.
    fn invalidate_async<'a>(
        &'a self,
        key: &'a str,
    ) -> CacheFuture<'a, Result<(), InvalidationError>> {
        Box::pin(async move {
            self.invalidate(key);
            Ok(())
        })
    }

    /// Whether this backend is the framework's failure-capsule wrapper.
    /// Framework internal. A decorator that wraps an installed backend
    /// forwards it, so the backend is not wrapped twice.
    #[doc(hidden)]
    fn is_capsule_seam(&self) -> bool {
        false
    }

    /// Async form of [`invalidate_namespace`](Cache::invalidate_namespace).
    ///
    /// The default calls the sync method and maps `false` to an error.
    ///
    /// # Errors
    ///
    /// [`InvalidationError`] when the backend cannot drop a namespace, or its
    /// sweep failed. Entries of the namespace can still be served.
    fn invalidate_namespace_async<'a>(
        &'a self,
        namespace: &'a str,
    ) -> CacheFuture<'a, Result<(), InvalidationError>> {
        Box::pin(async move {
            if self.invalidate_namespace(namespace) {
                Ok(())
            } else {
                Err(InvalidationError::new(
                    1,
                    "the backend cannot drop a namespace, or its sweep failed",
                ))
            }
        })
    }
}

/// Outcome of [`Cache::try_acquire_fill_lock`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillLockStatus {
    /// The caller now holds the lock and must call
    /// [`Cache::release_fill_lock`] when the fill completes (success or
    /// failure).
    Acquired,
    /// Another replica currently holds the lock.
    Held,
    /// This backend has no cross-replica fill lock; callers fall back to
    /// in-process-only single-flight protection.
    Unsupported,
}

// ── Typed convenience functions ──────────────────────────────────────

/// Typed get: retrieve and downcast a cached value.
///
/// Returns `None` if the key is absent or the stored type doesn't
/// match `V`. Works with any `Cache` implementation.
///
/// For cross-replica backends (Redis) use [`get_cached`] instead, which
/// also handles JSON deserialization of [`RawCacheBytes`].
pub fn get<V: Clone + Send + Sync + 'static>(cache: &dyn Cache, key: &str) -> Option<V> {
    // Failure-capsule seam (#2351 item 2). The value is not serializable, so
    // a replay serves only a recorded miss, and capture can record only a
    // miss.
    if replayed_untyped_get(key) {
        return None;
    }
    let value = cache
        .get_value(key)
        .and_then(|arc| arc.downcast_ref::<V>().cloned());
    record_untyped_get(key, value.is_some());
    value
}

/// Typed insert: wrap the value in an `Arc` and store it.
///
/// Works with any `Cache` implementation.
///
/// For cross-replica backends (Redis) use [`insert_cached`] instead,
/// which also serializes the value for storage across process boundaries.
pub fn insert<V: Clone + Send + Sync + 'static>(cache: &dyn Cache, key: &str, value: V) {
    // Failure-capsule seam (#2351 item 2): a replay never writes, and capture
    // cannot record the value.
    if record_or_replay_untyped_insert(key) {
        return;
    }
    cache.insert_value(key, Arc::new(value));
}

/// Serde-aware get: retrieve a cached value, deserializing from JSON if needed.
///
/// First tries a direct in-memory downcast (fast path for `MokaCache`). If
/// that fails — because the backend stored [`RawCacheBytes`] (e.g. Redis) —
/// the bytes are deserialized with `serde_json`. This is what the `#[cached]`
/// macro uses so that values survive across replicas when a shared backend
/// is configured.
pub fn get_cached<V>(cache: &dyn Cache, key: &str) -> Option<V>
where
    V: Clone + serde::Serialize + serde::de::DeserializeOwned + Send + Sync + 'static,
{
    // Failure-capsule seam (#1634). A replay is served entirely from the
    // capsule: the live backend is never consulted, because the value it holds
    // now is not the value the failing request read.
    match replayed_cache_get::<V>(key) {
        ReplayedRead::NoTape => {}
        ReplayedRead::Miss => return None,
        ReplayedRead::Hit(value) => return Some(value),
    }
    let arc = cache.get_value(key);
    let value = arc.and_then(|arc| {
        // Fast path: in-memory backend stored the concrete type directly.
        if let Some(value) = arc.downcast_ref::<V>() {
            return Some(value.clone());
        }
        // Slow path: serializing backend (e.g. Redis) stored RawCacheBytes.
        arc.downcast_ref::<RawCacheBytes>()
            .and_then(|raw| serde_json::from_slice::<V>(&raw.0).ok())
    });
    // Recorded from the *typed* value rather than from the stored `Arc<dyn
    // Any>`: the in-process backend (Moka) stores `Arc<V>` and never
    // `RawCacheBytes`, so reading the erased value would record every hit from
    // the default backend as valueless — and replay would then take the miss
    // branch of a handler whose bug is on the hit branch. The `Serialize`
    // bound is what makes the recording possible at all; every caller already
    // pairs this with `insert_cached`, which requires it too.
    record_cache_get(key, value.as_ref());
    value
}

/// Serde-aware insert: store the value both in-memory and as JSON bytes.
///
/// Calls [`Cache::insert_value`] (for in-process backends like Moka) **and**
/// [`Cache::insert_raw_bytes`] (for cross-replica backends like Redis). This
/// is what the `#[cached]` macro uses so that the stored value is accessible
/// both within the same process and on other replicas.
///
/// `ttl` is forwarded verbatim to [`Cache::insert_raw_bytes`] so backends
/// like Redis can apply a native entry expiry (e.g. `SET EX`). In-process
/// backends (Moka) manage TTL via the per-function static cache instance
/// and ignore this parameter.
pub fn insert_cached<V>(cache: &dyn Cache, key: &str, value: V, ttl: Option<std::time::Duration>)
where
    V: Clone + serde::Serialize + Send + Sync + 'static,
{
    let bytes = serde_json::to_vec(&value).ok();
    // Failure-capsule seam (#1634): the write is recorded on capture, and on
    // replay it lands in the tape (so a read-back in the same run finds it)
    // instead of in a live backend.
    if record_or_replay_cache_insert(key, bytes.as_deref(), ttl) {
        return;
    }
    // In-memory path (MokaCache, CountingCache in tests, …)
    cache.insert_value(key, Arc::new(value));
    // Serialized path (RedisCache, any cross-replica backend)
    if let Some(bytes) = bytes {
        cache.insert_raw_bytes(key, bytes, ttl);
    }
}

// ── Failure-capsule seam (#1634) ─────────────────────────────────────────────
//
// The `capsule` module is behind the `reporting` feature, so each helper here
// has a no-op twin for builds without it — the seam stays one line at each
// call site whatever the feature set.

/// What a replay has to say about a cache read.
///
/// A named type rather than a nested `Option`: "no replay is in progress" and
/// "the replay says this key missed" are opposite instructions to the caller,
/// and `Option<Option<V>>` spells them the same way round as each other.
enum ReplayedRead<V> {
    /// No replay is in progress; read the real cache.
    NoTape,
    /// The tape has no value to serve — a recorded miss, an unrecorded key
    /// (already logged as a divergence), or a hit the recording could not
    /// serialize. The read replays as a miss.
    Miss,
    /// The recorded hit.
    Hit(V),
}

/// Serve a cache read from the capsule's effect tape, when one is active.
#[cfg(feature = "reporting")]
fn replayed_cache_get<V>(key: &str) -> ReplayedRead<V>
where
    V: Clone + serde::de::DeserializeOwned + Send + Sync + 'static,
{
    use crate::capsule::effects::CachedValue;
    let Some(tape) = crate::capsule::effects::current_tape() else {
        return ReplayedRead::NoTape;
    };
    match tape.cache_get(key) {
        CachedValue::Hit(bytes) => {
            serde_json::from_slice::<V>(&bytes).map_or(ReplayedRead::Miss, ReplayedRead::Hit)
        }
        CachedValue::Miss | CachedValue::Unrecorded => ReplayedRead::Miss,
    }
}

/// No capsule support compiled in: never a replay.
#[cfg(not(feature = "reporting"))]
const fn replayed_cache_get<V>(_key: &str) -> ReplayedRead<V>
where
    V: Clone + serde::de::DeserializeOwned + Send + Sync + 'static,
{
    ReplayedRead::NoTape
}

/// Tee a cache read into the in-flight request's capsule.
///
/// A miss is recorded as a *keyed* entry with no value: replay then knows the
/// key was read (so it does not call it unrecorded) while still being honest
/// that it has nothing to serve, and the handler takes its miss branch.
///
/// A **hit** whose value will not serialize cannot be recorded that way. It
/// would be indistinguishable from a miss, so replay would take the miss branch
/// without a divergence and grade a run that never happened — the handler read
/// a value in production and reads nothing here. There is no honest recording
/// of a value that cannot be encoded, so the capsule declares itself
/// incomplete instead.
#[cfg(feature = "reporting")]
fn record_cache_get<V: serde::Serialize>(key: &str, value: Option<&V>) {
    let Some(scope) = crate::capsule::current_scope() else {
        return;
    };
    let encoded = value.map(|value| {
        serde_json::to_vec(value).map(|bytes| {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD.encode(&bytes)
        })
    });
    let encoded = match encoded {
        None => None,
        Some(Ok(encoded)) => Some(encoded),
        Some(Err(_)) => {
            scope.note(UNRECORDABLE_CACHE_HIT_NOTE);
            scope.mark_truncated();
            None
        }
    };
    scope.record_cache(crate::capsule::CacheEffect::Get {
        key: key.to_owned(),
        value: encoded,
    });
}

/// Why a capsule that wrote an unserializable cache value is not replayable.
#[cfg(feature = "reporting")]
const UNRECORDABLE_CACHE_WRITE_NOTE: &str = "a cache write held a value that could not be serialized into the capsule, so replay would \
     suppress a mutation the recorded run really made";

/// Why a capsule that read an unserializable cache hit is not replayable.
#[cfg(feature = "reporting")]
const UNRECORDABLE_CACHE_HIT_NOTE: &str = "a cache read hit a value that could not be serialized into the capsule, and a hit with no \
     recorded value is indistinguishable from a miss; replay would take the miss branch the \
     recorded run never took";

/// No capsule support compiled in: nothing to record.
#[cfg(not(feature = "reporting"))]
const fn record_cache_get<V: serde::Serialize>(_key: &str, _value: Option<&V>) {}

/// Record a cache write, or divert it into the replay tape.
///
/// Returns `true` when a replay handled the write and the live backend must
/// not be touched.
#[cfg(feature = "reporting")]
fn record_or_replay_cache_insert(
    key: &str,
    bytes: Option<&[u8]>,
    ttl: Option<std::time::Duration>,
) -> bool {
    if let Some(tape) = crate::capsule::effects::current_tape() {
        if let Some(bytes) = bytes {
            tape.cache_insert(key, bytes, ttl.map(|ttl| ttl.as_secs()));
        }
        return true;
    }
    if let Some(scope) = crate::capsule::current_scope() {
        if let Some(bytes) = bytes {
            use base64::Engine as _;
            scope.record_cache(crate::capsule::CacheEffect::Insert {
                key: key.to_owned(),
                value: base64::engine::general_purpose::STANDARD.encode(bytes),
                ttl_secs: ttl.map(|ttl| ttl.as_secs()),
            });
        } else {
            // The live backend accepted this write; the capsule cannot hold it.
            // Recording nothing would let replay suppress a real cache mutation
            // and still grade the run clean, which is the same falsification the
            // unserializable *read* refuses — caught here a function later.
            scope.note(UNRECORDABLE_CACHE_WRITE_NOTE);
            scope.mark_truncated();
        }
    }
    false
}

/// No capsule support compiled in: the live backend always takes the write.
#[cfg(not(feature = "reporting"))]
const fn record_or_replay_cache_insert(
    _key: &str,
    _bytes: Option<&[u8]>,
    _ttl: Option<std::time::Duration>,
) -> bool {
    false
}

/// Serve an untyped read during a replay. Returns `true` when a replay
/// handled it; the read is then a miss.
#[cfg(feature = "reporting")]
fn replayed_untyped_get(key: &str) -> bool {
    let Some(tape) = crate::capsule::effects::current_tape() else {
        return false;
    };
    tape.cache_untyped_get(key);
    true
}

/// No capsule support compiled in: never a replay.
#[cfg(not(feature = "reporting"))]
const fn replayed_untyped_get(_key: &str) -> bool {
    false
}

/// Record an untyped read. A miss records like a typed miss. A hit has no
/// value the capsule can hold, so the capsule is marked incomplete.
#[cfg(feature = "reporting")]
fn record_untyped_get(key: &str, hit: bool) {
    let Some(scope) = crate::capsule::current_scope() else {
        return;
    };
    if hit {
        scope.note(UNRECORDABLE_CACHE_HIT_NOTE);
        scope.mark_truncated();
    }
    scope.record_cache(crate::capsule::CacheEffect::Get {
        key: key.to_owned(),
        value: None,
    });
}

/// No capsule support compiled in: nothing to record.
#[cfg(not(feature = "reporting"))]
const fn record_untyped_get(_key: &str, _hit: bool) {}

/// Record an untyped write, or divert it during a replay. Returns `true` when
/// a replay handled it and the backend must not be touched.
#[cfg(feature = "reporting")]
fn record_or_replay_untyped_insert(key: &str) -> bool {
    if let Some(tape) = crate::capsule::effects::current_tape() {
        tape.cache_untyped_insert(key);
        return true;
    }
    if let Some(scope) = crate::capsule::current_scope() {
        scope.note(UNRECORDABLE_CACHE_WRITE_NOTE);
        scope.mark_truncated();
    }
    false
}

/// No capsule support compiled in: the backend always takes the write.
#[cfg(not(feature = "reporting"))]
const fn record_or_replay_untyped_insert(_key: &str) -> bool {
    false
}

// ── Capsule seam for the removal methods (#2351 item 2) ──────────────────

/// Wrap `cache` so a capsule records its removals and a replay never reaches
/// it. Idempotent: a wrapped backend is returned as it is, so `Arc` identity
/// (which single-flight keys on) stays stable.
#[cfg(feature = "reporting")]
#[must_use]
pub(crate) fn with_capsule_seam(cache: Arc<dyn Cache>) -> Arc<dyn Cache> {
    if cache.is_capsule_seam() {
        return cache;
    }
    Arc::new(CapsuleSeamCache(cache))
}

/// No capsule support compiled in: the backend is used as it is.
#[cfg(not(feature = "reporting"))]
#[must_use]
pub(crate) fn with_capsule_seam(cache: Arc<dyn Cache>) -> Arc<dyn Cache> {
    cache
}

/// The installed backend, with the removal methods on the capsule seam.
///
/// Reads and writes pass through: `get_cached` / `insert_cached` and the
/// untyped functions carry their own seam.
#[cfg(feature = "reporting")]
struct CapsuleSeamCache(Arc<dyn Cache>);

#[cfg(feature = "reporting")]
impl CapsuleSeamCache {
    /// Record a removal, with the result the backend gave.
    fn record(effect: crate::capsule::CacheEffect) {
        if let Some(scope) = crate::capsule::current_scope() {
            scope.record_cache(effect);
        }
    }

    /// Run a removal that returns no result. A backend counts a failed one
    /// with [`record_invalidation_failure`]. When the count goes up, replay
    /// cannot know the result, so the capsule is marked incomplete. A failure
    /// on another task can also do this; replay then only refuses.
    fn unchecked_removal(remove: impl FnOnce()) {
        let before = invalidation_failures_total();
        remove();
        if invalidation_failures_total() != before
            && let Some(scope) = crate::capsule::current_scope()
        {
            scope.note(UNCHECKED_REMOVAL_NOTE);
            scope.mark_truncated();
        }
    }
}

/// Why a capsule with a removal of unknown result is not replayable.
#[cfg(feature = "reporting")]
const UNCHECKED_REMOVAL_NOTE: &str = "a cache removal with no result reported a failure; \
     replay cannot know whether the entry stayed";

/// Why a capsule that met a distributed fill lock is not replayable.
#[cfg(feature = "reporting")]
const FILL_LOCK_NOTE: &str = "a cache fill met a distributed fill lock; the lock outcome \
     is not recorded, so replay cannot take the same path";

/// A recorded [`InvalidationError`].
#[cfg(feature = "reporting")]
fn recorded_error(
    result: &Result<(), InvalidationError>,
) -> Option<crate::capsule::CacheInvalidationError> {
    result
        .as_ref()
        .err()
        .map(|error| crate::capsule::CacheInvalidationError {
            attempts: error.attempts(),
            reason: error.reason().to_owned(),
        })
}

#[cfg(feature = "reporting")]
impl Cache for CapsuleSeamCache {
    // During a replay the backend is never reached. A typed or untyped read
    // or write is answered by its own seam before it gets here; anything
    // that still arrives is a miss, a dropped write, or no fill lock.
    fn get_value(&self, key: &str) -> Option<Arc<dyn Any + Send + Sync>> {
        if crate::capsule::effects::tape_active() {
            return None;
        }
        self.0.get_value(key)
    }

    fn insert_value(&self, key: &str, value: Arc<dyn Any + Send + Sync>) {
        if crate::capsule::effects::tape_active() {
            return;
        }
        self.0.insert_value(key, value);
    }

    fn invalidate(&self, key: &str) {
        if let Some(tape) = crate::capsule::effects::current_tape() {
            let _ = tape.cache_invalidate(key);
            return;
        }
        Self::record(crate::capsule::CacheEffect::Invalidate {
            key: key.to_owned(),
            error: None,
        });
        Self::unchecked_removal(|| self.0.invalidate(key));
    }

    fn clear(&self) {
        if let Some(tape) = crate::capsule::effects::current_tape() {
            tape.cache_clear();
            return;
        }
        Self::record(crate::capsule::CacheEffect::Clear);
        Self::unchecked_removal(|| self.0.clear());
    }

    fn invalidate_namespace(&self, namespace: &str) -> bool {
        if let Some(tape) = crate::capsule::effects::current_tape() {
            return tape.cache_invalidate_namespace(namespace).is_ok();
        }
        let done = self.0.invalidate_namespace(namespace);
        let result = if done {
            Ok(())
        } else {
            Err(InvalidationError::new(
                1,
                "the backend cannot drop a namespace, or its sweep failed",
            ))
        };
        Self::record(crate::capsule::CacheEffect::InvalidateNamespace {
            namespace: namespace.to_owned(),
            error: recorded_error(&result),
        });
        done
    }

    fn insert_raw_bytes(&self, key: &str, bytes: Vec<u8>, ttl: Option<Duration>) {
        if crate::capsule::effects::tape_active() {
            return;
        }
        self.0.insert_raw_bytes(key, bytes, ttl);
    }

    fn try_acquire_fill_lock(&self, key: &str, token: &str, ttl: Duration) -> FillLockStatus {
        if crate::capsule::effects::tape_active() {
            return FillLockStatus::Unsupported;
        }
        let status = self.0.try_acquire_fill_lock(key, token, ttl);
        // Replay answers `Unsupported`, so a lock path cannot replay.
        if status != FillLockStatus::Unsupported
            && let Some(scope) = crate::capsule::current_scope()
        {
            scope.note(FILL_LOCK_NOTE);
            scope.mark_truncated();
        }
        status
    }

    fn release_fill_lock(&self, key: &str, token: &str) {
        if crate::capsule::effects::tape_active() {
            return;
        }
        self.0.release_fill_lock(key, token);
    }

    fn invalidate_async<'a>(
        &'a self,
        key: &'a str,
    ) -> CacheFuture<'a, Result<(), InvalidationError>> {
        // The tape and the capture scope are task-locals, so they are read
        // here, on the calling task, and carried into the future.
        if let Some(tape) = crate::capsule::effects::current_tape() {
            return Box::pin(async move { tape.cache_invalidate(key) });
        }
        // The scope is read now; the tape position is taken when the future
        // first runs, which is when replay consumes its entry. It is filled
        // when the removal ends, so a cancelled removal leaves an unfilled
        // slot that marks the capsule incomplete.
        let scope = crate::capsule::current_scope();
        Box::pin(async move {
            let slot = scope.and_then(|scope| scope.reserve_cache().map(|index| (scope, index)));
            let result = self.0.invalidate_async(key).await;
            if let Some((scope, index)) = slot {
                scope.fill_cache(
                    index,
                    crate::capsule::CacheEffect::Invalidate {
                        key: key.to_owned(),
                        error: recorded_error(&result),
                    },
                );
            }
            result
        })
    }

    fn invalidate_namespace_async<'a>(
        &'a self,
        namespace: &'a str,
    ) -> CacheFuture<'a, Result<(), InvalidationError>> {
        if let Some(tape) = crate::capsule::effects::current_tape() {
            return Box::pin(async move { tape.cache_invalidate_namespace(namespace) });
        }
        let scope = crate::capsule::current_scope();
        Box::pin(async move {
            let slot = scope.and_then(|scope| scope.reserve_cache().map(|index| (scope, index)));
            let result = self.0.invalidate_namespace_async(namespace).await;
            if let Some((scope, index)) = slot {
                scope.fill_cache(
                    index,
                    crate::capsule::CacheEffect::InvalidateNamespace {
                        namespace: namespace.to_owned(),
                        error: recorded_error(&result),
                    },
                );
            }
            result
        })
    }

    fn is_capsule_seam(&self) -> bool {
        true
    }
}

// ── CacheableResult trait ────────────────────────────────────────────

/// Helper trait used by `#[cached(result)]` to extract the `Ok` type
/// from a `Result<T, E>` return type at the type level.
///
/// This avoids the need for the proc macro to syntactically parse
/// generic arguments out of the return type.
pub trait CacheableResult {
    /// The success type to cache.
    type Ok: Clone;
    /// The error type (passed through uncached).
    type Err;

    /// Convert into a standard `Result` for pattern matching.
    ///
    /// # Errors
    ///
    /// Returns `Err` if the original result was an error.
    fn into_result(self) -> Result<Self::Ok, Self::Err>;
    /// Wrap a cached `Ok` value back into the original result type.
    fn from_ok(ok: Self::Ok) -> Self;
}

impl<T: Clone, E> CacheableResult for Result<T, E> {
    type Ok = T;
    type Err = E;

    fn into_result(self) -> Self {
        self
    }

    fn from_ok(ok: T) -> Self {
        Ok(ok)
    }
}

// ── Cache key helper ─────────────────────────────────────────────────

/// Build a cache key from a function name and its hashable arguments.
///
/// Used by `#[cached]` macro-generated code. The key is
/// `"{fn_name}:{hash_hex}"` where the hash is a 64-bit `DefaultHasher`
/// digest of the argument tuple.
#[must_use]
pub fn make_cache_key<K: Hash>(fn_name: &str, args: &K) -> String {
    let mut hasher = DefaultHasher::new();
    args.hash(&mut hasher);
    format!("{}:{:x}", fn_name, hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_key_deterministic() {
        let k1 = make_cache_key("get_user", &(42_i64,));
        let k2 = make_cache_key("get_user", &(42_i64,));
        assert_eq!(k1, k2);
    }

    #[test]
    fn cache_key_differs_by_fn_name() {
        let k1 = make_cache_key("get_user", &(42_i64,));
        let k2 = make_cache_key("find_user", &(42_i64,));
        assert_ne!(k1, k2);
    }

    #[test]
    fn cache_key_differs_by_args() {
        let k1 = make_cache_key("get_user", &(1_i64,));
        let k2 = make_cache_key("get_user", &(2_i64,));
        assert_ne!(k1, k2);
    }

    #[test]
    fn cache_key_no_args() {
        let k = make_cache_key("get_config", &());
        assert!(k.starts_with("get_config:"));
    }

    #[cfg(feature = "cache-moka")]
    #[test]
    fn insert_cached_and_get_cached_round_trip() {
        let cache = MokaCache::new(10, None);
        insert_cached(&cache, "key", "hello".to_string(), None);
        let val: Option<String> = get_cached(&cache, "key");
        assert_eq!(val.as_deref(), Some("hello"));
    }

    #[cfg(feature = "cache-moka")]
    #[test]
    fn get_cached_raw_bytes_slow_path() {
        // Simulate a cross-replica backend: store RawCacheBytes directly, then
        // verify get_cached deserializes it back to the concrete type.
        let cache = MokaCache::new(10, None);
        let bytes = serde_json::to_vec(&42_i32).unwrap();
        cache.insert_value("k", Arc::new(RawCacheBytes(bytes)));
        let val: Option<i32> = get_cached(&cache, "k");
        assert_eq!(val, Some(42));
    }

    #[cfg(feature = "cache-moka")]
    #[test]
    fn get_cached_miss_returns_none() {
        let cache = MokaCache::new(10, None);
        let val: Option<String> = get_cached(&cache, "missing");
        assert!(val.is_none());
    }

    /// A backend that keeps only the required methods, to test the defaults.
    struct MinimalBackend {
        invalidated: std::sync::Mutex<Vec<String>>,
    }

    impl Cache for MinimalBackend {
        fn get_value(&self, _key: &str) -> Option<Arc<dyn Any + Send + Sync>> {
            None
        }
        fn insert_value(&self, _key: &str, _value: Arc<dyn Any + Send + Sync>) {}
        fn invalidate(&self, key: &str) {
            self.invalidated
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(key.to_owned());
        }
        fn clear(&self) {}
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(future)
    }

    #[test]
    fn default_invalidate_async_calls_the_sync_method() {
        let backend = MinimalBackend {
            invalidated: std::sync::Mutex::new(Vec::new()),
        };
        block_on(backend.invalidate_async("k")).expect("the default cannot fail");
        assert_eq!(
            *backend
                .invalidated
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            vec!["k".to_owned()]
        );
    }

    #[test]
    fn default_invalidate_namespace_async_reports_an_unsupported_backend() {
        let backend = MinimalBackend {
            invalidated: std::sync::Mutex::new(Vec::new()),
        };
        let err = block_on(backend.invalidate_namespace_async("ns"))
            .expect_err("a backend that cannot sweep must say so");
        assert_eq!(err.attempts(), 1);
    }

    /// Logs every call that reaches the backend.
    #[cfg(feature = "reporting")]
    #[derive(Default)]
    struct SpyBackend {
        calls: std::sync::Mutex<Vec<String>>,
    }

    #[cfg(feature = "reporting")]
    impl SpyBackend {
        fn log(&self, call: impl Into<String>) {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(call.into());
        }
        fn calls(&self) -> Vec<String> {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
    }

    #[cfg(feature = "reporting")]
    impl Cache for SpyBackend {
        fn get_value(&self, key: &str) -> Option<Arc<dyn Any + Send + Sync>> {
            self.log(format!("get {key}"));
            Some(Arc::new(7_u32))
        }
        fn insert_value(&self, key: &str, _value: Arc<dyn Any + Send + Sync>) {
            self.log(format!("insert {key}"));
        }
        fn invalidate(&self, key: &str) {
            self.log(format!("invalidate {key}"));
        }
        fn clear(&self) {
            self.log("clear");
        }
        fn invalidate_namespace(&self, namespace: &str) -> bool {
            self.log(format!("invalidate_namespace {namespace}"));
            true
        }
    }

    /// #2351 item 2: a replayed run never reaches the installed backend
    /// through the untyped functions or the removal methods.
    #[cfg(feature = "reporting")]
    #[test]
    fn a_replayed_run_never_reaches_the_installed_backend() {
        let spy = Arc::new(SpyBackend::default());
        let state =
            crate::state::AppState::for_test().with_cache(Arc::clone(&spy) as Arc<dyn Cache>);
        let tape = Arc::new(crate::capsule::ReplayEffects::new(
            crate::capsule::CapsuleEffects::default(),
        ));
        block_on(crate::capsule::with_effect_tape(Arc::clone(&tape), async {
            let cache = state.cache().expect("installed");
            cache.invalidate("widgets");
            cache.clear();
            let _ = cache.invalidate_namespace("widgets");
            let _ = cache.invalidate_async("widgets").await;
            insert(cache.as_ref(), "widgets", 41_u32);
            assert_eq!(get::<u32>(cache.as_ref(), "widgets"), None);
        }));
        assert!(spy.calls().is_empty(), "{:?}", spy.calls());
        assert_eq!(tape.divergences().len(), 6, "{:?}", tape.divergences());
    }

    /// #2351 item 2: capture records a removal, and the backend still gets it.
    #[cfg(feature = "reporting")]
    #[test]
    fn a_captured_removal_is_recorded_and_still_applied() {
        let spy = Arc::new(SpyBackend::default());
        let state =
            crate::state::AppState::for_test().with_cache(Arc::clone(&spy) as Arc<dyn Cache>);
        let scope = Arc::new(crate::capsule::CaptureScope::new(
            "cache-test".to_owned(),
            Arc::new(crate::capsule::CaptureSettings::default()),
            Arc::new(crate::log::filter::ParameterFilter::default()),
        ));
        block_on(crate::capsule::capture::with_capture_scope(
            Arc::clone(&scope),
            async {
                let cache = state.cache().expect("installed");
                cache.invalidate("widgets");
                cache.clear();
            },
        ));
        assert_eq!(spy.calls(), vec!["invalidate widgets", "clear"]);
        assert_eq!(
            scope.effects_snapshot().cache,
            vec![
                crate::capsule::CacheEffect::Invalidate {
                    key: "widgets".to_owned(),
                    error: None,
                },
                crate::capsule::CacheEffect::Clear,
            ]
        );
    }

    /// #2351 item 2: an untyped hit cannot be serialized, so the capsule says
    /// it is incomplete.
    #[cfg(feature = "reporting")]
    #[test]
    fn a_captured_untyped_hit_or_write_marks_the_capsule_incomplete() {
        let spy = Arc::new(SpyBackend::default());
        let scope = Arc::new(crate::capsule::CaptureScope::new(
            "cache-test".to_owned(),
            Arc::new(crate::capsule::CaptureSettings::default()),
            Arc::new(crate::log::filter::ParameterFilter::default()),
        ));
        block_on(crate::capsule::capture::with_capture_scope(
            Arc::clone(&scope),
            async {
                assert_eq!(get::<u32>(spy.as_ref(), "widgets"), Some(7));
            },
        ));
        assert!(scope.is_truncated());
        let scope = Arc::new(crate::capsule::CaptureScope::new(
            "cache-test".to_owned(),
            Arc::new(crate::capsule::CaptureSettings::default()),
            Arc::new(crate::log::filter::ParameterFilter::default()),
        ));
        block_on(crate::capsule::capture::with_capture_scope(
            Arc::clone(&scope),
            async {
                insert(spy.as_ref(), "widgets", 1_u32);
            },
        ));
        assert!(scope.is_truncated());
    }

    /// Review fix: during a replay the wrapper reaches the backend for no
    /// read, write or fill lock.
    #[cfg(feature = "reporting")]
    #[test]
    fn the_wrapper_reaches_no_backend_during_a_replay() {
        let spy = Arc::new(SpyBackend::default());
        let cache = with_capsule_seam(Arc::clone(&spy) as Arc<dyn Cache>);
        let tape = Arc::new(crate::capsule::ReplayEffects::new(
            crate::capsule::CapsuleEffects::default(),
        ));
        block_on(crate::capsule::with_effect_tape(tape, async {
            assert!(cache.get_value("k").is_none());
            cache.insert_value("k", Arc::new(1_u32));
            cache.insert_raw_bytes("k", b"1".to_vec(), None);
            assert_eq!(
                cache.try_acquire_fill_lock("k", "t", Duration::from_secs(1)),
                FillLockStatus::Unsupported
            );
            cache.release_fill_lock("k", "t");
        }));
        assert!(spy.calls().is_empty(), "{:?}", spy.calls());
    }

    /// An async removal takes its tape position when it first runs, the
    /// point where replay consumes its entry (Codex review on #3222).
    #[cfg(feature = "reporting")]
    #[test]
    fn an_async_removal_is_recorded_when_it_first_runs() {
        let spy = Arc::new(SpyBackend::default());
        let cache = with_capsule_seam(Arc::clone(&spy) as Arc<dyn Cache>);
        let scope = Arc::new(crate::capsule::CaptureScope::new(
            "cache-test".to_owned(),
            Arc::new(crate::capsule::CaptureSettings::default()),
            Arc::new(crate::log::filter::ParameterFilter::default()),
        ));
        block_on(crate::capsule::capture::with_capture_scope(
            Arc::clone(&scope),
            async {
                let first = cache.invalidate_async("a");
                cache.clear();
                first.await.expect("removed");
            },
        ));
        assert_eq!(
            scope.effects_snapshot().cache,
            vec![
                crate::capsule::CacheEffect::Clear,
                crate::capsule::CacheEffect::Invalidate {
                    key: "a".to_owned(),
                    error: None,
                },
            ]
        );
    }

    /// Codex review on #3222: when production had a global cache, a replay
    /// answers a repository invalidation from the tape, so it is consumed.
    #[cfg(feature = "reporting")]
    #[test]
    fn replay_installs_a_seam_cache_when_production_had_a_global_cache() {
        let _guard = GLOBAL_CACHE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let recorded = crate::capsule::CapsuleEffects {
            cache: vec![crate::capsule::CacheEffect::InvalidateNamespace {
                namespace: "posts".to_owned(),
                error: None,
            }],
            global_cache: true,
            ..Default::default()
        };
        let installed = install_replay_cache(&recorded).expect("installed");
        assert!(installed.is_capsule_seam());
        let tape = Arc::new(crate::capsule::ReplayEffects::new(recorded));
        let complete = block_on(crate::capsule::with_effect_tape(
            Arc::clone(&tape),
            coherence::invalidate_namespace_async("posts"),
        ));
        clear_global_cache();
        assert!(complete);
        assert!(tape.finish().is_empty(), "{:?}", tape.finish());

        assert!(install_replay_cache(&crate::capsule::CapsuleEffects::default()).is_none());
        assert!(global_cache().is_none());
    }

    /// A backend whose sync removals fail, and whose fill lock is held.
    #[cfg(feature = "reporting")]
    struct FailingSyncBackend;

    #[cfg(feature = "reporting")]
    impl Cache for FailingSyncBackend {
        fn get_value(&self, _key: &str) -> Option<Arc<dyn Any + Send + Sync>> {
            None
        }
        fn insert_value(&self, _key: &str, _value: Arc<dyn Any + Send + Sync>) {}
        fn invalidate(&self, _key: &str) {
            record_invalidation_failure();
        }
        fn clear(&self) {
            record_invalidation_failure();
        }
        fn try_acquire_fill_lock(
            &self,
            _key: &str,
            _token: &str,
            _ttl: Duration,
        ) -> FillLockStatus {
            FillLockStatus::Held
        }
    }

    #[cfg(feature = "reporting")]
    fn capture_scope() -> Arc<crate::capsule::CaptureScope> {
        Arc::new(crate::capsule::CaptureScope::new(
            "cache-test".to_owned(),
            Arc::new(crate::capsule::CaptureSettings::default()),
            Arc::new(crate::log::filter::ParameterFilter::default()),
        ))
    }

    /// Codex review on #3222: a sync removal that reports a failure marks
    /// the capsule incomplete.
    #[cfg(feature = "reporting")]
    #[test]
    fn a_failed_sync_removal_marks_the_capsule_incomplete() {
        for remove in [
            (|cache: &dyn Cache| cache.invalidate("k")) as fn(&dyn Cache),
            |cache: &dyn Cache| cache.clear(),
        ] {
            let cache = with_capsule_seam(Arc::new(FailingSyncBackend));
            let scope = capture_scope();
            block_on(crate::capsule::capture::with_capture_scope(
                Arc::clone(&scope),
                async { remove(cache.as_ref()) },
            ));
            assert!(scope.is_truncated());
        }
    }

    /// Codex review on #3222: a fill lock outcome other than `Unsupported`
    /// marks the capsule incomplete.
    #[cfg(feature = "reporting")]
    #[test]
    fn a_distributed_fill_lock_marks_the_capsule_incomplete() {
        let cache = with_capsule_seam(Arc::new(FailingSyncBackend));
        let scope = capture_scope();
        let status = block_on(crate::capsule::capture::with_capture_scope(
            Arc::clone(&scope),
            async { cache.try_acquire_fill_lock("k", "t", Duration::from_secs(1)) },
        ));
        assert_eq!(status, FillLockStatus::Held);
        assert!(scope.is_truncated());

        let moka = with_capsule_seam(Arc::new(SpyBackend::default()));
        let scope = capture_scope();
        block_on(crate::capsule::capture::with_capture_scope(
            Arc::clone(&scope),
            async {
                let _ = moka.try_acquire_fill_lock("k", "t", Duration::from_secs(1));
            },
        ));
        assert!(
            !scope.is_truncated(),
            "a backend with no lock is replayable"
        );
    }

    /// The seam wraps a backend once, so `Arc` identity stays stable.
    #[cfg(feature = "reporting")]
    #[test]
    fn the_capsule_seam_wraps_a_backend_once() {
        let spy: Arc<dyn Cache> = Arc::new(SpyBackend::default());
        let once = with_capsule_seam(spy);
        let twice = with_capsule_seam(Arc::clone(&once));
        assert!(Arc::ptr_eq(&once, &twice));
    }

    #[test]
    fn invalidation_error_reports_attempts_and_reason() {
        let err = InvalidationError::new(3, "READONLY");
        assert_eq!(err.attempts(), 3);
        assert_eq!(err.reason(), "READONLY");
        assert_eq!(
            err.to_string(),
            "cache invalidation failed after 3 attempt(s): READONLY"
        );
    }

    #[test]
    fn invalidation_failures_counter_is_monotonic() {
        let before = invalidation_failures_total();
        record_invalidation_failure();
        assert!(invalidation_failures_total() > before);
    }

    #[test]
    fn cacheable_result_ok_round_trips() {
        let r: Result<i32, &str> = Result::from_ok(42);
        assert_eq!(r, Ok(42));
        assert_eq!(r.into_result(), Ok(42));
    }

    #[test]
    fn cacheable_result_err_passes_through() {
        let r: Result<i32, &str> = Err("oops");
        assert_eq!(r.into_result(), Err("oops"));
    }
}
