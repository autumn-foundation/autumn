//! Read-through cache fills with stampede protection.
//!
//! When a hot key expires, every concurrent request misses and recomputes the
//! same value at once — the classic thundering herd. This module adds a
//! read-through API over any [`Cache`] backend:
//!
//! - [`get_or_compute`] — get the cached value, or run `fill` **once per
//!   process** and share the result with every concurrent caller
//!   (single-flight coalescing).
//! - [`get_or_compute_with`] — the same, plus opt-in cross-replica protection
//!   via a distributed fill lock and/or stale-while-revalidate, configured
//!   with [`GetOrComputeOptions`].
//! - [`jittered_ttl`] — de-synchronize mass expiry of keys written together.
//! - [`read_through_metrics`] — process-wide counters (hits, misses,
//!   coalesced waiters, fills, fill failures, …) surfaced through the
//!   actuator's metrics endpoints.
//!
//! # Single-flight protocol
//!
//! Concurrent misses for the same key elect one *leader* (the first caller to
//! register in a process-global in-flight table); the leader runs the fill and
//! publishes the outcome over a watch channel. Every other caller becomes a
//! *waiter* and awaits that outcome instead of recomputing. A failing fill
//! never poisons the key: the leader's error propagates (typed) to the leader
//! and (rendered) to the waiters, the in-flight entry is removed, and the next
//! caller retries the fill.

use std::any::Any;
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::watch;

use super::Cache;

// ── Options ──────────────────────────────────────────────────────────

/// Configuration for [`get_or_compute_with`].
///
/// The default (`GetOrComputeOptions::new()`) is in-process single-flight
/// only: no TTL, no distributed lock, no stale-while-revalidate.
#[derive(Clone, Debug)]
pub struct GetOrComputeOptions {
    /// Freshness TTL for the value written on fill. `None` = no expiry.
    pub ttl: Option<Duration>,
    /// Opt in to a cross-replica fill lock (supported by backends that
    /// implement [`Cache::try_acquire_fill_lock`], e.g. Redis). When another
    /// replica holds the lock this replica polls for the value instead of
    /// filling.
    pub distributed_fill_lock: bool,
    /// Expiry of the distributed fill lock. Bounds the damage from a filler
    /// that crashes while holding the lock: after this the lock self-clears
    /// and another replica takes over.
    pub lock_ttl: Duration,
    /// How often a replica that lost the distributed lock polls the cache for
    /// the winner's value (and re-attempts the lock).
    pub lock_poll_interval: Duration,
    /// How long a replica waits on another replica's fill before giving up
    /// and computing the value itself (bounded damage, never unavailability).
    pub lock_wait_timeout: Duration,
    /// `Some(grace)` enables stale-while-revalidate: after `ttl` the value is
    /// considered stale but is still served for up to `grace` while a single
    /// background task refreshes it.
    pub stale_while_revalidate: Option<Duration>,
}

impl GetOrComputeOptions {
    /// Create options with defaults: no TTL, in-process single-flight only.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            ttl: None,
            distributed_fill_lock: false,
            lock_ttl: Duration::from_secs(10),
            lock_poll_interval: Duration::from_millis(50),
            lock_wait_timeout: Duration::from_secs(5),
            stale_while_revalidate: None,
        }
    }

    /// Set the freshness TTL.
    #[must_use]
    pub const fn ttl(mut self, ttl: Duration) -> Self {
        self.ttl = Some(ttl);
        self
    }

    /// Opt in to the distributed fill lock (Redis backend).
    #[must_use]
    pub const fn distributed_fill_lock(mut self, enabled: bool) -> Self {
        self.distributed_fill_lock = enabled;
        self
    }

    /// Set the distributed lock's expiry.
    #[must_use]
    pub const fn lock_ttl(mut self, ttl: Duration) -> Self {
        self.lock_ttl = ttl;
        self
    }

    /// Set the poll cadence used while another replica holds the fill lock.
    #[must_use]
    pub const fn lock_poll_interval(mut self, interval: Duration) -> Self {
        self.lock_poll_interval = interval;
        self
    }

    /// Set how long to wait on another replica's fill before self-filling.
    #[must_use]
    pub const fn lock_wait_timeout(mut self, timeout: Duration) -> Self {
        self.lock_wait_timeout = timeout;
        self
    }

    /// Enable stale-while-revalidate with the given grace period.
    #[must_use]
    pub const fn stale_while_revalidate(mut self, grace: Duration) -> Self {
        self.stale_while_revalidate = Some(grace);
        self
    }
}

impl Default for GetOrComputeOptions {
    fn default() -> Self {
        Self::new()
    }
}

// ── Error type ───────────────────────────────────────────────────────

/// Error returned by [`get_or_compute`] / [`get_or_compute_with`].
///
/// A failing fill never poisons the key: nothing is written to the cache, the
/// in-flight entry is removed before waiters are woken, and the next caller
/// retries the fill.
#[derive(Debug, thiserror::Error)]
pub enum CacheFillError<E> {
    /// This caller ran the fill closure itself and it failed.
    #[error("cache fill failed: {0}")]
    Fill(E),
    /// This caller coalesced onto a concurrent fill (same key, same process)
    /// whose fill failed. Carries the leader's error rendered via `Display`
    /// (`E` is not required to be `Clone`).
    #[error("cache fill failed in concurrent caller: {0}")]
    FillFailed(Arc<str>),
}

impl<E> CacheFillError<E> {
    /// Return the typed fill error if this caller ran the fill itself.
    pub fn into_fill(self) -> Option<E> {
        match self {
            Self::Fill(e) => Some(e),
            Self::FillFailed(_) => None,
        }
    }
}

// ── Metrics ──────────────────────────────────────────────────────────

/// Process-wide read-through cache counters.
///
/// Exposed as `autumn_cache_*` counters on `/actuator/prometheus` and under
/// `cache` in the `/actuator/metrics` JSON snapshot. Obtain the live instance
/// with [`read_through_metrics`].
#[derive(Debug, Default)]
pub struct ReadThroughMetrics {
    hits: AtomicU64,
    misses: AtomicU64,
    coalesced_waits: AtomicU64,
    fills: AtomicU64,
    fill_failures: AtomicU64,
    stale_serves: AtomicU64,
    fill_lock_acquires: AtomicU64,
    fill_lock_contended: AtomicU64,
}

/// Point-in-time copy of [`ReadThroughMetrics`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct ReadThroughMetricsSnapshot {
    /// Fresh fast-path reads served from the cache.
    pub hits: u64,
    /// Fast-path reads that found no (fresh) value.
    pub misses: u64,
    /// Callers that parked behind an in-process leader instead of filling.
    pub coalesced_waits: u64,
    /// Fill closures that completed successfully in this process.
    pub fills: u64,
    /// Fill closures that returned an error.
    pub fill_failures: u64,
    /// Stale values served while a background refresh ran (SWR mode).
    pub stale_serves: u64,
    /// Distributed fill locks acquired by this process.
    pub fill_lock_acquires: u64,
    /// Distributed fill lock attempts that found the lock held elsewhere.
    pub fill_lock_contended: u64,
}

impl ReadThroughMetrics {
    /// Take a consistent-enough snapshot of all counters (each counter is
    /// read atomically; the set is not read under a global lock).
    pub fn snapshot(&self) -> ReadThroughMetricsSnapshot {
        todo!("green phase")
    }
}

/// The process-wide [`ReadThroughMetrics`] instance updated by
/// [`get_or_compute`] and [`get_or_compute_with`].
pub fn read_through_metrics() -> &'static ReadThroughMetrics {
    todo!("green phase")
}

// ── TTL jitter ───────────────────────────────────────────────────────

/// Multiply `base` by a random factor drawn uniformly from
/// `[1 - fraction, 1 + fraction]`.
///
/// Use this when writing many keys together (bulk warmups, batch imports) so
/// they don't all expire in the same instant and stampede together.
/// `fraction` is clamped to `[0.0, 1.0]`; non-finite values are treated as
/// `0.0` (no jitter).
#[must_use]
pub fn jittered_ttl(base: Duration, fraction: f64) -> Duration {
    let _ = (base, fraction);
    todo!("green phase")
}

// ── In-flight registry (single-flight) ──────────────────────────────

/// Outcome of an in-flight fill, published to waiters over a watch channel.
#[derive(Clone)]
enum FillState {
    Pending,
    Done(Arc<dyn Any + Send + Sync>),
    Failed(Arc<str>),
}

/// In-flight fills, keyed by (cache identity, key). Two distinct
/// `Arc<dyn Cache>` allocations never coalesce with each other.
#[allow(dead_code)] // used from the green phase onwards
static IN_FLIGHT: LazyLock<Mutex<HashMap<(usize, String), watch::Receiver<FillState>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// RAII removal of an in-flight entry: covers success, error, panic, and
/// cancellation (the leader's future being dropped mid-fill).
#[allow(dead_code)] // used from the green phase onwards
struct InFlightGuard {
    key: (usize, String),
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        IN_FLIGHT
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.key);
    }
}

// ── Public API ───────────────────────────────────────────────────────

/// Read `key` from the cache, or compute it with `fill` — at most once per
/// process for concurrent callers (single-flight).
///
/// On a hit the cached value is returned immediately. On a miss, the first
/// caller runs `fill`; every concurrent caller for the same key awaits that
/// one fill and shares its result. The computed value is written back with
/// `ttl` (honored natively by backends like Redis; in-process Moka caches use
/// their per-instance TTL).
///
/// For cross-replica protection or stale-while-revalidate, use
/// [`get_or_compute_with`].
///
/// # Errors
///
/// - [`CacheFillError::Fill`] if this caller ran `fill` and it failed.
/// - [`CacheFillError::FillFailed`] if a concurrent caller ran `fill` and it
///   failed; the message is the leader's error rendered via `Display`.
///
/// A failed fill writes nothing: the next caller retries.
pub async fn get_or_compute<V, E, F, Fut>(
    cache: &Arc<dyn Cache>,
    key: &str,
    ttl: Option<Duration>,
    fill: F,
) -> Result<V, CacheFillError<E>>
where
    V: Clone + serde::Serialize + serde::de::DeserializeOwned + Send + Sync + 'static,
    E: std::fmt::Display + Send + 'static,
    F: FnOnce() -> Fut + Send,
    Fut: Future<Output = Result<V, E>> + Send,
{
    let _ = (cache, key, ttl, fill);
    todo!("green phase")
}

/// [`get_or_compute`] with cross-replica options: a distributed fill lock
/// and/or stale-while-revalidate. See [`GetOrComputeOptions`].
///
/// The `'static` bounds on `fill` exist because stale-while-revalidate may
/// run the fill on a background task; capture owned handles (`Arc`-clone your
/// pool into the closure).
///
/// # Errors
///
/// Same as [`get_or_compute`].
pub async fn get_or_compute_with<V, E, F, Fut>(
    cache: &Arc<dyn Cache>,
    key: &str,
    options: GetOrComputeOptions,
    fill: F,
) -> Result<V, CacheFillError<E>>
where
    V: Clone + serde::Serialize + serde::de::DeserializeOwned + Send + Sync + 'static,
    E: std::fmt::Display + Send + 'static,
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = Result<V, E>> + Send + 'static,
{
    let _ = (cache, key, options, fill);
    todo!("green phase")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jittered_ttl_within_bounds() {
        let base = Duration::from_secs(100);
        let mut distinct = std::collections::HashSet::new();
        for _ in 0..100 {
            let jittered = jittered_ttl(base, 0.2);
            assert!(
                jittered >= Duration::from_secs(80) && jittered <= Duration::from_secs(120),
                "jittered TTL {jittered:?} outside [80s, 120s]"
            );
            distinct.insert(jittered.as_nanos());
        }
        assert!(
            distinct.len() >= 2,
            "expected at least 2 distinct jittered values, got {}",
            distinct.len()
        );
        assert_eq!(jittered_ttl(base, 0.0), base, "fraction 0.0 must be exact");
    }

    #[test]
    fn jittered_ttl_clamps_fraction() {
        let base = Duration::from_secs(10);
        for _ in 0..50 {
            let jittered = jittered_ttl(base, 5.0);
            assert!(
                jittered <= Duration::from_secs(20),
                "fraction must clamp to 1.0; got {jittered:?}"
            );
        }
        // Non-finite fractions degrade to no jitter rather than panicking.
        assert_eq!(jittered_ttl(base, f64::NAN), base);
        assert_eq!(jittered_ttl(base, f64::INFINITY), base);
    }

    #[cfg(feature = "cache-moka")]
    #[test]
    fn fill_lock_default_unsupported() {
        use super::super::{FillLockStatus, MokaCache};

        let cache = MokaCache::new(10, None);
        assert_eq!(
            Cache::try_acquire_fill_lock(&cache, "k", "token", Duration::from_secs(1)),
            FillLockStatus::Unsupported
        );
        // Default release is a no-op and must not panic.
        Cache::release_fill_lock(&cache, "k", "token");
    }

    #[test]
    fn metrics_snapshot_starts_at_zero_and_counts() {
        let metrics = ReadThroughMetrics::default();
        let snap = metrics.snapshot();
        assert_eq!(snap.hits, 0);
        assert_eq!(snap.misses, 0);
        assert_eq!(snap.coalesced_waits, 0);
        assert_eq!(snap.fills, 0);
        assert_eq!(snap.fill_failures, 0);
        assert_eq!(snap.stale_serves, 0);
        assert_eq!(snap.fill_lock_acquires, 0);
        assert_eq!(snap.fill_lock_contended, 0);

        metrics.hits.fetch_add(2, Ordering::Relaxed);
        metrics.misses.fetch_add(1, Ordering::Relaxed);
        metrics.coalesced_waits.fetch_add(3, Ordering::Relaxed);
        metrics.fills.fetch_add(4, Ordering::Relaxed);
        metrics.fill_failures.fetch_add(5, Ordering::Relaxed);
        metrics.stale_serves.fetch_add(6, Ordering::Relaxed);
        metrics.fill_lock_acquires.fetch_add(7, Ordering::Relaxed);
        metrics.fill_lock_contended.fetch_add(8, Ordering::Relaxed);

        let snap = metrics.snapshot();
        assert_eq!(snap.hits, 2);
        assert_eq!(snap.misses, 1);
        assert_eq!(snap.coalesced_waits, 3);
        assert_eq!(snap.fills, 4);
        assert_eq!(snap.fill_failures, 5);
        assert_eq!(snap.stale_serves, 6);
        assert_eq!(snap.fill_lock_acquires, 7);
        assert_eq!(snap.fill_lock_contended, 8);
    }

    #[test]
    fn read_through_metrics_is_a_stable_singleton() {
        let a: *const ReadThroughMetrics = read_through_metrics();
        let b: *const ReadThroughMetrics = read_through_metrics();
        assert_eq!(a, b);
    }
}
