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
/// # Panics
///
/// Panics if the internal `RwLock` is poisoned.
pub fn set_global_cache(cache: Arc<dyn Cache>) {
    *GLOBAL_CACHE.write().expect("global cache lock poisoned") = Some(cache);
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

    /// Read the namespace's **shared** fill epoch.
    ///
    /// A cross-replica backend keeps one epoch for each namespace in the
    /// shared store.
    /// [`invalidate_namespace`](Cache::invalidate_namespace) must raise it
    /// **before** it sweeps. A fill reads it after a miss and before it
    /// computes. Then the fill inserts with
    /// [`insert_raw_bytes_if_epoch`](Cache::insert_raw_bytes_if_epoch). Then no
    /// fill can write back a value that an invalidation has dropped. This holds
    /// on all replicas.
    ///
    /// The default is [`FillEpoch::Unsupported`]. The fence stays per process.
    fn fill_epoch(&self, _namespace: &str) -> FillEpoch {
        FillEpoch::Unsupported
    }

    /// Store pre-serialized bytes only if the shared epoch still equals
    /// `sampled`. The check and the store must be one atomic step.
    ///
    /// Returns `true` when stored. A backend that overrides
    /// [`fill_epoch`](Cache::fill_epoch) must override this too. The default
    /// stores without a check.
    ///
    /// When the epoch is [`FillEpoch::Sampled`], `insert_value` is not called.
    /// Fill any local tier here.
    fn insert_raw_bytes_if_epoch(
        &self,
        key: &str,
        bytes: Vec<u8>,
        ttl: Option<std::time::Duration>,
        _namespace: &str,
        _sampled: u64,
    ) -> bool {
        self.insert_raw_bytes(key, bytes, ttl);
        true
    }

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

/// A namespace's shared fill epoch, as read by [`Cache::fill_epoch`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillEpoch {
    /// The backend has no shared epoch. Only the per-process fence applies.
    Unsupported,
    /// The epoch at the time of the read.
    Sampled(u64),
    /// The backend has a shared epoch but could not read it. A fill must not
    /// insert. The result is one extra cache miss.
    Unavailable,
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
    cache
        .get_value(key)
        .and_then(|arc| arc.downcast_ref::<V>().cloned())
}

/// Typed insert: wrap the value in an `Arc` and store it.
///
/// Works with any `Cache` implementation.
///
/// For cross-replica backends (Redis) use [`insert_cached`] instead,
/// which also serializes the value for storage across process boundaries.
pub fn insert<V: Clone + Send + Sync + 'static>(cache: &dyn Cache, key: &str, value: V) {
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

/// Like [`insert_cached`], but store only if the shared epoch did not change.
///
/// `epoch` is the [`Cache::fill_epoch`] read after the miss and before the
/// compute. Returns `true` when the value was stored. `false` means the fill
/// was fenced out, or the epoch was [`FillEpoch::Unavailable`].
///
/// With [`FillEpoch::Unsupported`] this is [`insert_cached`].
pub fn insert_cached_fenced<V>(
    cache: &dyn Cache,
    key: &str,
    value: V,
    ttl: Option<std::time::Duration>,
    namespace: &str,
    epoch: FillEpoch,
) -> bool
where
    V: Clone + serde::Serialize + Send + Sync + 'static,
{
    let sampled = match epoch {
        FillEpoch::Unsupported => {
            insert_cached(cache, key, value, ttl);
            return true;
        }
        FillEpoch::Unavailable => return false,
        FillEpoch::Sampled(sampled) => sampled,
    };
    let Some(bytes) = serde_json::to_vec(&value).ok() else {
        return false;
    };
    // Raw bytes only: `insert_value` would write without the check.
    let stored = cache.insert_raw_bytes_if_epoch(key, bytes.clone(), ttl, namespace, sampled);
    if stored {
        // Capture records the write after it happened. A replay never gets
        // here: `sample_fill_epoch` returns `Unsupported` during a replay.
        let _ = record_or_replay_cache_insert(key, Some(&bytes), ttl);
    }
    stored
}

/// Read the shared fill epoch for a fill that just missed.
///
/// A replay serves cache effects from the tape (#1634), so it never reads the
/// backend. It gets [`FillEpoch::Unsupported`], and [`insert_cached_fenced`]
/// then writes to the tape.
#[must_use]
pub fn sample_fill_epoch(cache: &dyn Cache, namespace: &str) -> FillEpoch {
    if replaying() {
        return FillEpoch::Unsupported;
    }
    cache.fill_epoch(namespace)
}

/// Whether an effect tape serves the current task.
#[cfg(feature = "reporting")]
fn replaying() -> bool {
    crate::capsule::effects::tape_active()
}

/// No capsule support compiled in: there is no replay.
#[cfg(not(feature = "reporting"))]
const fn replaying() -> bool {
    false
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
mod shared_fence_tests {
    //! A fake cross-replica backend: two handles share one store and one epoch.
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Shared {
        data: HashMap<String, Vec<u8>>,
        epochs: HashMap<String, u64>,
    }

    #[derive(Clone, Default)]
    struct Replica {
        shared: Arc<Mutex<Shared>>,
        epoch_down: bool,
    }

    impl Cache for Replica {
        fn get_value(&self, key: &str) -> Option<Arc<dyn Any + Send + Sync>> {
            let bytes = self.shared.lock().unwrap().data.get(key).cloned()?;
            Some(Arc::new(RawCacheBytes(bytes)))
        }
        fn insert_value(&self, _key: &str, _value: Arc<dyn Any + Send + Sync>) {
            panic!("a shared-fence backend must not take the unfenced path");
        }
        fn invalidate(&self, key: &str) {
            self.shared.lock().unwrap().data.remove(key);
        }
        fn clear(&self) {
            self.shared.lock().unwrap().data.clear();
        }
        fn invalidate_namespace(&self, namespace: &str) -> bool {
            let mut shared = self.shared.lock().unwrap();
            *shared.epochs.entry(namespace.to_owned()).or_default() += 1;
            let prefix = format!("{namespace}:");
            shared.data.retain(|key, _| !key.starts_with(&prefix));
            true
        }
        fn insert_raw_bytes(&self, key: &str, bytes: Vec<u8>, _ttl: Option<Duration>) {
            self.shared
                .lock()
                .unwrap()
                .data
                .insert(key.to_owned(), bytes);
        }
        fn fill_epoch(&self, namespace: &str) -> FillEpoch {
            if self.epoch_down {
                return FillEpoch::Unavailable;
            }
            let shared = self.shared.lock().unwrap();
            FillEpoch::Sampled(shared.epochs.get(namespace).copied().unwrap_or(0))
        }
        fn insert_raw_bytes_if_epoch(
            &self,
            key: &str,
            bytes: Vec<u8>,
            _ttl: Option<Duration>,
            namespace: &str,
            sampled: u64,
        ) -> bool {
            let mut shared = self.shared.lock().unwrap();
            if shared.epochs.get(namespace).copied().unwrap_or(0) != sampled {
                return false;
            }
            shared.data.insert(key.to_owned(), bytes);
            true
        }
    }

    #[test]
    fn a_fill_on_another_replica_cannot_resurrect_an_invalidated_value() {
        let a = Replica::default();
        let b = Replica {
            shared: Arc::clone(&a.shared),
            epoch_down: false,
        };
        // B misses and samples the shared epoch, then starts computing.
        let epoch = b.fill_epoch("ns");
        // A commits a write and invalidates the namespace.
        assert!(a.invalidate_namespace("ns"));
        // B finishes and tries to insert the pre-write value.
        let inserted = insert_cached_fenced(&b, "ns:k", "old".to_string(), None, "ns", epoch);
        assert!(
            !inserted,
            "the shared epoch moved, so the fill is fenced out"
        );
        assert!(get_cached::<String>(&a, "ns:k").is_none());
    }

    #[test]
    fn a_fill_inserts_when_no_invalidation_landed() {
        let a = Replica::default();
        let epoch = a.fill_epoch("ns");
        assert!(insert_cached_fenced(
            &a,
            "ns:k",
            "new".to_string(),
            None,
            "ns",
            epoch
        ));
        assert_eq!(get_cached::<String>(&a, "ns:k").as_deref(), Some("new"));
    }

    #[test]
    fn an_unreadable_epoch_skips_the_insert() {
        let a = Replica {
            epoch_down: true,
            ..Replica::default()
        };
        let epoch = a.fill_epoch("ns");
        assert_eq!(epoch, FillEpoch::Unavailable);
        assert!(!insert_cached_fenced(
            &a,
            "ns:k",
            "v".to_string(),
            None,
            "ns",
            epoch
        ));
        assert!(get_cached::<String>(&a, "ns:k").is_none());
    }

    #[test]
    fn a_fragment_render_on_another_replica_cannot_resurrect_an_invalidated_value() {
        let a = Replica::default();
        let b = Replica {
            shared: Arc::clone(&a.shared),
            epoch_down: false,
        };
        let html = fragment::cache_fragment_in(Some(&b), "ns", "id", "v1", None, || {
            // A writes and invalidates while B is still rendering.
            assert!(a.invalidate_namespace("ns"));
            maud::html! { "old" }
        });
        assert_eq!(html.0, "old", "the fenced-out caller still gets its markup");
        let again = fragment::cache_fragment_in(Some(&b), "ns", "id", "v1", None, || {
            maud::html! { "new" }
        });
        assert_eq!(again.0, "new", "the stale markup must not be cached");
    }

    /// A replay serves cache effects from the tape. It must not read the live
    /// epoch, and the fenced write must land on the tape (#1634).
    #[cfg(feature = "reporting")]
    #[tokio::test]
    async fn a_replay_never_touches_the_live_epoch_and_still_writes_the_tape() {
        use crate::capsule::{
            CapsuleEffects,
            effects::{CachedValue, ReplayEffects},
        };
        let live = Replica {
            epoch_down: true,
            ..Replica::default()
        };
        let tape = std::sync::Arc::new(ReplayEffects::new(CapsuleEffects::default()));
        let seen = std::sync::Arc::clone(&tape);
        crate::capsule::effects::with_effect_tape(tape, async {
            let epoch = sample_fill_epoch(&live, "ns");
            assert_eq!(
                epoch,
                FillEpoch::Unsupported,
                "replay must skip the backend"
            );
            assert!(insert_cached_fenced(
                &live, "ns:k", 7_u32, None, "ns", epoch
            ));
        })
        .await;
        assert!(
            live.shared.lock().unwrap().data.is_empty(),
            "a replay must not write the live backend"
        );
        assert!(matches!(seen.cache_get("ns:k"), CachedValue::Hit(_)));
    }

    #[cfg(feature = "cache-moka")]
    #[test]
    fn a_backend_without_a_shared_epoch_inserts_as_before() {
        let moka = MokaCache::new(10, None);
        let epoch = moka.fill_epoch("ns");
        assert_eq!(epoch, FillEpoch::Unsupported);
        assert!(insert_cached_fenced(
            &moka,
            "ns:k",
            "v".to_string(),
            None,
            "ns",
            epoch
        ));
        assert_eq!(get_cached::<String>(&moka, "ns:k").as_deref(), Some("v"));
    }
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
