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
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::{Semaphore, watch};

use super::{Cache, FillLockStatus, get_cached, insert_cached};
use crate::time::{ClockSource, SystemClock, clock_unix_duration};

// ── Options ──────────────────────────────────────────────────────────

/// Configuration for [`get_or_compute_with`].
///
/// The default (`GetOrComputeOptions::new()`) is in-process single-flight
/// only: no TTL, no distributed lock, no stale-while-revalidate.
#[derive(Clone)]
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
    /// Clock used to evaluate stale-while-revalidate freshness. Defaults to
    /// [`SystemClock`]; overridden in tests via [`with_clock`] so freshness
    /// transitions are deterministic instead of depending on real
    /// `tokio::time::sleep` calls.
    ///
    /// [`with_clock`]: GetOrComputeOptions::with_clock
    clock: Arc<dyn ClockSource>,
}

impl GetOrComputeOptions {
    /// Create options with defaults: no TTL, in-process single-flight only.
    #[must_use]
    pub fn new() -> Self {
        Self {
            ttl: None,
            distributed_fill_lock: false,
            lock_ttl: Duration::from_secs(10),
            lock_poll_interval: Duration::from_millis(50),
            lock_wait_timeout: Duration::from_secs(5),
            stale_while_revalidate: None,
            clock: Arc::new(SystemClock),
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

    /// Override the clock used to evaluate stale-while-revalidate freshness.
    ///
    /// Defaults to [`SystemClock`]; tests can pass a
    /// [`crate::time::FixedClock`] / [`crate::time::TickingClock`] to make
    /// freshness transitions deterministic.
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn ClockSource>) -> Self {
        self.clock = clock;
        self
    }
}

impl Default for GetOrComputeOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for GetOrComputeOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GetOrComputeOptions")
            .field("ttl", &self.ttl)
            .field("distributed_fill_lock", &self.distributed_fill_lock)
            .field("lock_ttl", &self.lock_ttl)
            .field("lock_poll_interval", &self.lock_poll_interval)
            .field("lock_wait_timeout", &self.lock_wait_timeout)
            .field("stale_while_revalidate", &self.stale_while_revalidate)
            .finish_non_exhaustive()
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
        ReadThroughMetricsSnapshot {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            coalesced_waits: self.coalesced_waits.load(Ordering::Relaxed),
            fills: self.fills.load(Ordering::Relaxed),
            fill_failures: self.fill_failures.load(Ordering::Relaxed),
            stale_serves: self.stale_serves.load(Ordering::Relaxed),
            fill_lock_acquires: self.fill_lock_acquires.load(Ordering::Relaxed),
            fill_lock_contended: self.fill_lock_contended.load(Ordering::Relaxed),
        }
    }
}

/// The process-wide [`ReadThroughMetrics`] instance updated by
/// [`get_or_compute`] and [`get_or_compute_with`].
#[must_use]
pub fn read_through_metrics() -> &'static ReadThroughMetrics {
    static METRICS: LazyLock<ReadThroughMetrics> = LazyLock::new(ReadThroughMetrics::default);
    &METRICS
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
    let fraction = if fraction.is_finite() {
        fraction.clamp(0.0, 1.0)
    } else {
        0.0
    };
    if fraction == 0.0 {
        return base;
    }

    let mut buf = [0_u8; 4];
    if getrandom::getrandom(&mut buf).is_err() {
        // Fallback entropy source if the OS RNG is unavailable.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        buf = nanos.to_le_bytes();
    }
    let unit = f64::from(u32::from_le_bytes(buf)) / f64::from(u32::MAX);
    // factor is uniform in [1 - fraction, 1 + fraction].
    let factor = fraction.mul_add(2.0f64.mul_add(unit, -1.0), 1.0);
    base.mul_f64(factor)
}

// ── In-flight registry (single-flight) ──────────────────────────────

/// Outcome of an in-flight fill, published to waiters over a watch channel.
#[derive(Clone)]
enum FillState {
    Pending,
    Done(Arc<dyn Any + Send + Sync>),
    Failed(Arc<str>),
}

/// Identifies a single in-flight fill: which cache backend, and which key.
type InFlightKey = (usize, String);

/// In-flight fills, keyed by (cache identity, key). Two distinct
/// `Arc<dyn Cache>` allocations never coalesce with each other.
static IN_FLIGHT: LazyLock<Mutex<HashMap<InFlightKey, watch::Receiver<FillState>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// RAII removal of an in-flight entry: covers success, error, panic, and
/// cancellation (the leader's future being dropped mid-fill). Dropping the
/// paired `watch::Sender` also closes the channel, which is how waiters
/// notice a cancelled leader and re-contend for leadership.
struct InFlightGuard {
    key: InFlightKey,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        IN_FLIGHT
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.key);
    }
}

/// Which role a caller plays for a given (cache, key): the single leader that
/// runs the fill, or a waiter that awaits the leader's result.
enum Role {
    Leader(watch::Sender<FillState>, InFlightGuard),
    Waiter(watch::Receiver<FillState>),
}

/// Identity of a cache backend, stable for the lifetime of the `Arc`
/// allocation. Used so single-flight coalescing is scoped per cache handle:
/// two distinct `Arc<dyn Cache>`s (e.g. two Redis "replica" connections in a
/// test) never coalesce with each other, matching the fact that in-process
/// coalescing is inherently per-process.
fn cache_identity(cache: &Arc<dyn Cache>) -> usize {
    (Arc::as_ptr(cache).cast::<()>()) as usize
}

/// Register as the leader for `(cache, key)`, or discover an existing leader
/// and become a waiter. The `std::sync::Mutex` guard is held only for this
/// synchronous lookup/insert — never across an `.await` point.
fn claim_role(cache: &Arc<dyn Cache>, key: &str) -> Role {
    use std::collections::hash_map::Entry;

    let map_key: InFlightKey = (cache_identity(cache), key.to_owned());
    // A single lock acquisition covers the whole check-then-insert so two
    // threads can never both observe "no entry" and both become leader.
    let mut in_flight = IN_FLIGHT.lock().unwrap_or_else(PoisonError::into_inner);
    match in_flight.entry(map_key.clone()) {
        Entry::Occupied(entry) => Role::Waiter(entry.get().clone()),
        Entry::Vacant(entry) => {
            let (tx, rx) = watch::channel(FillState::Pending);
            entry.insert(rx);
            Role::Leader(tx, InFlightGuard { key: map_key })
        }
    }
}

/// Await the leader's outcome. Returns `None` when the caller should retry
/// (`claim_role` again) rather than trust the result: either the leader's
/// future was dropped/cancelled before publishing an outcome (channel
/// closed), or the leader published a value of a different type than `V`
/// (the same key was used with two different value types).
async fn await_result<V, E>(
    mut rx: watch::Receiver<FillState>,
) -> Option<Result<V, CacheFillError<E>>>
where
    V: Clone + Send + Sync + 'static,
{
    let state = {
        let waited = rx.wait_for(|s| !matches!(s, FillState::Pending)).await;
        match waited {
            Ok(state_ref) => state_ref.clone(),
            Err(_closed) => return None,
        }
    };
    match state {
        FillState::Pending => unreachable!("wait_for guarantees a non-pending state"),
        FillState::Done(value) => value.downcast_ref::<V>().cloned().map(Ok),
        FillState::Failed(message) => Some(Err(CacheFillError::FillFailed(message))),
    }
}

// ── Stale-while-revalidate envelope ─────────────────────────────────

/// Wraps a cached value with its freshness deadline when
/// stale-while-revalidate is enabled. Stored in place of the bare value, so a
/// key must be used consistently with or without SWR.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct SwrEnvelope<V> {
    value: V,
    fresh_until_unix_ms: u64,
}

fn now_unix_ms(clock: &dyn ClockSource) -> u64 {
    u64::try_from(clock_unix_duration(clock).as_millis()).unwrap_or(u64::MAX)
}

/// Read the current value, used for the lock-poll loop and the leader's
/// pre-fill double-check.
///
/// In SWR mode this must still check freshness: entry into the poll loop, by
/// construction, only happens when a *stale* envelope already exists (that's
/// why a refresh was triggered in the first place), so treating "an envelope
/// exists" as "the lock winner published a fresh value" would let a losing
/// replica mistake the old stale envelope for a completed refresh.
fn fast_path_value<V>(cache: &Arc<dyn Cache>, key: &str, options: &GetOrComputeOptions) -> Option<V>
where
    V: Clone + serde::de::DeserializeOwned + Send + Sync + 'static,
{
    if options.stale_while_revalidate.is_some() {
        get_cached::<SwrEnvelope<V>>(cache.as_ref(), key)
            .filter(|envelope| now_unix_ms(options.clock.as_ref()) < envelope.fresh_until_unix_ms)
            .map(|envelope| envelope.value)
    } else {
        get_cached::<V>(cache.as_ref(), key)
    }
}

/// Ceiling on the lock-poll loop's exponential backoff (see [`run_leader_fill`]).
const MAX_LOCK_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Run `fill`, release the distributed lock, and publish/write the result.
/// Shared by the immediate-`Acquired` path and the poll loop's re-acquire
/// path in [`run_leader_fill`], which otherwise duplicate this exact
/// increment-fill-release-finish sequence.
///
/// The lock is released only *after* `finish_fill` has written and published
/// the value, not right after `fill()` returns: releasing it earlier opens a
/// window where a contending replica's poll tick sees both "no value yet" and
/// "lock free," acquires the now-unlocked lock, and runs its own redundant
/// fill — exactly the duplicate-recompute the distributed lock exists to
/// prevent.
async fn run_fill_and_release<V, E, F, Fut>(
    cache: &Arc<dyn Cache>,
    key: &str,
    options: &GetOrComputeOptions,
    fill: F,
    tx: &watch::Sender<FillState>,
    token: &str,
) -> Result<V, CacheFillError<E>>
where
    V: Clone + serde::Serialize + serde::de::DeserializeOwned + Send + Sync + 'static,
    E: std::fmt::Display + Send + 'static,
    F: FnOnce() -> Fut + Send,
    Fut: Future<Output = Result<V, E>> + Send,
{
    read_through_metrics()
        .fill_lock_acquires
        .fetch_add(1, Ordering::Relaxed);
    let result = fill().await;
    let outcome = finish_fill(cache, key, options, result, tx);
    cache.release_fill_lock(key, token);
    outcome
}

/// Run the fill closure (honoring the distributed fill lock if enabled),
/// write the result, and publish the outcome to `tx`. Shared by the cold-miss
/// leader path and the stale-while-revalidate background refresh.
async fn run_leader_fill<V, E, F, Fut>(
    cache: &Arc<dyn Cache>,
    key: &str,
    options: &GetOrComputeOptions,
    fill: F,
    tx: watch::Sender<FillState>,
) -> Result<V, CacheFillError<E>>
where
    V: Clone + serde::Serialize + serde::de::DeserializeOwned + Send + Sync + 'static,
    E: std::fmt::Display + Send + 'static,
    F: FnOnce() -> Fut + Send,
    Fut: Future<Output = Result<V, E>> + Send,
{
    if !options.distributed_fill_lock {
        let result = fill().await;
        return finish_fill(cache, key, options, result, &tx);
    }

    let token = uuid::Uuid::new_v4().to_string();
    match cache.try_acquire_fill_lock(key, &token, options.lock_ttl) {
        FillLockStatus::Unsupported => {
            let result = fill().await;
            finish_fill(cache, key, options, result, &tx)
        }
        FillLockStatus::Acquired => {
            run_fill_and_release(cache, key, options, fill, &tx, &token).await
        }
        FillLockStatus::Held => {
            read_through_metrics()
                .fill_lock_contended
                .fetch_add(1, Ordering::Relaxed);
            let start = Instant::now();
            // Exponential backoff, capped at MAX_LOCK_POLL_INTERVAL: a flat
            // poll cadence means every waiting replica hammers Redis (a cache
            // read plus a lock-acquire attempt, each a block_in_place round
            // trip) at a constant rate for the whole wait, even once it's
            // clear the lock is held and unlikely to free up immediately.
            let mut poll_interval = options.lock_poll_interval;
            loop {
                tokio::time::sleep(poll_interval).await;
                poll_interval = poll_interval.saturating_mul(2).min(MAX_LOCK_POLL_INTERVAL);

                if let Some(value) = fast_path_value::<V>(cache, key, options) {
                    let _ = tx.send(FillState::Done(Arc::new(value.clone())));
                    return Ok(value);
                }

                if start.elapsed() >= options.lock_wait_timeout {
                    // Bounded damage: give up waiting and fill locally rather
                    // than block the caller indefinitely.
                    let result = fill().await;
                    return finish_fill(cache, key, options, result, &tx);
                }

                if cache.try_acquire_fill_lock(key, &token, options.lock_ttl)
                    == FillLockStatus::Acquired
                {
                    return run_fill_and_release(cache, key, options, fill, &tx, &token).await;
                }
            }
        }
    }
}

/// Write the fill outcome to the cache (as a bare value, or a
/// [`SwrEnvelope`] when stale-while-revalidate is enabled) and publish it to
/// waiters. A failure writes nothing, so the key is never poisoned.
fn finish_fill<V, E>(
    cache: &Arc<dyn Cache>,
    key: &str,
    options: &GetOrComputeOptions,
    result: Result<V, E>,
    tx: &watch::Sender<FillState>,
) -> Result<V, CacheFillError<E>>
where
    V: Clone + serde::Serialize + serde::de::DeserializeOwned + Send + Sync + 'static,
    E: std::fmt::Display,
{
    match result {
        Ok(value) => {
            if let Some(grace) = options.stale_while_revalidate {
                // `ttl: None` means "no expiry" (per its own doc comment): the
                // envelope must stay fresh forever, not go stale immediately.
                // Treating a missing TTL as `ttl_ms = 0` would stamp
                // `fresh_until` as "now", making the very next read (and every
                // read after) take the stale branch and re-trigger a
                // background refresh in a tight loop.
                let fresh_until_unix_ms = options.ttl.map_or(u64::MAX, |ttl| {
                    let ttl_ms = u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX);
                    now_unix_ms(options.clock.as_ref()).saturating_add(ttl_ms)
                });
                let envelope = SwrEnvelope {
                    value: value.clone(),
                    fresh_until_unix_ms,
                };
                let physical_ttl = options.ttl.map(|ttl| ttl + grace);
                insert_cached(cache.as_ref(), key, envelope, physical_ttl);
            } else {
                insert_cached(cache.as_ref(), key, value.clone(), options.ttl);
            }
            read_through_metrics().fills.fetch_add(1, Ordering::Relaxed);
            let _ = tx.send(FillState::Done(Arc::new(value.clone())));
            Ok(value)
        }
        Err(error) => {
            let message: Arc<str> = error.to_string().into();
            read_through_metrics()
                .fill_failures
                .fetch_add(1, Ordering::Relaxed);
            let _ = tx.send(FillState::Failed(message));
            Err(CacheFillError::Fill(error))
        }
    }
}

/// Process-wide ceiling on concurrently running stale-while-revalidate
/// background refreshes, across *all* keys. The per-key single-flight
/// registry only dedupes refreshes for the *same* key; without this, many
/// keys going stale around the same time (e.g. after a bulk write with
/// unjittered TTLs) could spawn an unbounded number of detached tasks, each
/// holding open whatever resource the `fill` closure captured (a pooled DB
/// connection, say), risking pool exhaustion under load.
const MAX_CONCURRENT_BACKGROUND_REFRESHES: usize = 64;

static BACKGROUND_REFRESH_LIMIT: LazyLock<Semaphore> =
    LazyLock::new(|| Semaphore::new(MAX_CONCURRENT_BACKGROUND_REFRESHES));

/// Try to become the leader for a stale-while-revalidate background refresh.
/// If another fill (a cold-miss leader, or an earlier refresh) is already
/// in-flight for this key, do nothing and drop `fill` unrun — only one
/// refresh should run at a time per key. If the process-wide concurrent
/// refresh limit is already saturated, also drop `fill` unrun rather than
/// spawn an unbounded task: the stale value was already returned to this
/// caller, and the next stale read will retry the refresh.
fn spawn_background_refresh<V, E, F, Fut>(
    cache: Arc<dyn Cache>,
    key: String,
    options: GetOrComputeOptions,
    fill: F,
) where
    V: Clone + serde::Serialize + serde::de::DeserializeOwned + Send + Sync + 'static,
    E: std::fmt::Display + Send + 'static,
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = Result<V, E>> + Send + 'static,
{
    match claim_role(&cache, &key) {
        Role::Leader(tx, guard) => {
            let Ok(permit) = BACKGROUND_REFRESH_LIMIT.try_acquire() else {
                drop(guard);
                return;
            };
            tokio::spawn(async move {
                let _permit = permit;
                let _result = run_leader_fill(&cache, &key, &options, fill, tx).await;
                drop(guard);
            });
        }
        Role::Waiter(_rx) => {
            // A refresh (or an unrelated fill) is already in flight for this
            // key; the stale value has already been returned to this caller.
        }
    }
}

/// In-process single-flight read-through: no stale-while-revalidate, so
/// `fill` need not be `'static` (it is always awaited inline, never spawned).
async fn simple_read_through<V, E, F, Fut>(
    cache: &Arc<dyn Cache>,
    key: &str,
    options: &GetOrComputeOptions,
    fill: F,
) -> Result<V, CacheFillError<E>>
where
    V: Clone + serde::Serialize + serde::de::DeserializeOwned + Send + Sync + 'static,
    E: std::fmt::Display + Send + 'static,
    F: FnOnce() -> Fut + Send,
    Fut: Future<Output = Result<V, E>> + Send,
{
    loop {
        if let Some(value) = get_cached::<V>(cache.as_ref(), key) {
            read_through_metrics().hits.fetch_add(1, Ordering::Relaxed);
            return Ok(value);
        }
        read_through_metrics()
            .misses
            .fetch_add(1, Ordering::Relaxed);

        match claim_role(cache, key) {
            Role::Leader(tx, guard) => {
                // Double-check: another leader may have filled the key
                // between the fast-path read above and winning leadership.
                if let Some(value) = get_cached::<V>(cache.as_ref(), key) {
                    let _ = tx.send(FillState::Done(Arc::new(value.clone())));
                    drop(guard);
                    return Ok(value);
                }
                let result = run_leader_fill(cache, key, options, fill, tx).await;
                drop(guard);
                return result;
            }
            Role::Waiter(rx) => {
                read_through_metrics()
                    .coalesced_waits
                    .fetch_add(1, Ordering::Relaxed);
                if let Some(result) = await_result::<V, E>(rx).await {
                    return result;
                }
                // Channel closed or type mismatch: loop back and re-contend.
            }
        }
    }
}

/// Whether an SWR envelope is still usable (fresh or stale-but-in-grace) at
/// `now`, i.e. `now < fresh_until_unix_ms + grace`. Once this is false the
/// entry is past `ttl + grace` and must be treated as a cold miss rather than
/// served indefinitely: relying on physical eviction to make that happen
/// doesn't hold for the in-process Moka backend, which (per its own
/// per-cache TTL, commonly `None`) may never evict the entry on its own.
fn swr_within_grace(now_unix_ms: u64, fresh_until_unix_ms: u64, grace: Option<Duration>) -> bool {
    let grace_ms = grace.map_or(0, |g| u64::try_from(g.as_millis()).unwrap_or(u64::MAX));
    now_unix_ms < fresh_until_unix_ms.saturating_add(grace_ms)
}

/// Stale-while-revalidate read-through: `fill` must be `'static` because a
/// stale read may hand it to a background task instead of awaiting it.
async fn swr_read_through<V, E, F, Fut>(
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
    loop {
        let envelope = get_cached::<SwrEnvelope<V>>(cache.as_ref(), key);
        let now = now_unix_ms(options.clock.as_ref());

        if let Some(envelope) = &envelope {
            if now < envelope.fresh_until_unix_ms {
                read_through_metrics().hits.fetch_add(1, Ordering::Relaxed);
                return Ok(envelope.value.clone());
            }
            if swr_within_grace(
                now,
                envelope.fresh_until_unix_ms,
                options.stale_while_revalidate,
            ) {
                read_through_metrics()
                    .stale_serves
                    .fetch_add(1, Ordering::Relaxed);
                spawn_background_refresh(cache.clone(), key.to_owned(), options.clone(), fill);
                return Ok(envelope.value.clone());
            }
            // Past `ttl + grace`: fall through to the cold-miss path below
            // instead of serving this indefinitely-stale value forever.
        }

        read_through_metrics()
            .misses
            .fetch_add(1, Ordering::Relaxed);
        match claim_role(cache, key) {
            Role::Leader(tx, guard) => {
                // Double-check: another leader may have refreshed the key
                // between the read above and winning leadership. Only trust
                // that write if it's actually usable (fresh or in grace) —
                // otherwise this would just re-observe the same expired
                // envelope and skip filling entirely.
                if let Some(envelope) = get_cached::<SwrEnvelope<V>>(cache.as_ref(), key)
                    && swr_within_grace(
                        now_unix_ms(options.clock.as_ref()),
                        envelope.fresh_until_unix_ms,
                        options.stale_while_revalidate,
                    )
                {
                    let _ = tx.send(FillState::Done(Arc::new(envelope.value.clone())));
                    drop(guard);
                    return Ok(envelope.value);
                }
                let result = run_leader_fill(cache, key, &options, fill, tx).await;
                drop(guard);
                return result;
            }
            Role::Waiter(rx) => {
                read_through_metrics()
                    .coalesced_waits
                    .fetch_add(1, Ordering::Relaxed);
                if let Some(result) = await_result::<V, E>(rx).await {
                    return result;
                }
                // Channel closed or type mismatch: loop back and re-contend.
            }
        }
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
    let options = GetOrComputeOptions {
        ttl,
        ..GetOrComputeOptions::new()
    };
    simple_read_through(cache, key, &options, fill).await
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
    if options.stale_while_revalidate.is_some() {
        swr_read_through(cache, key, options, fill).await
    } else {
        simple_read_through(cache, key, &options, fill).await
    }
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

    #[cfg(feature = "cache-moka")]
    #[test]
    fn fast_path_value_ignores_stale_swr_envelope() {
        use super::super::MokaCache;

        let cache: Arc<dyn Cache> = Arc::new(MokaCache::new(10, None));
        let key = "fast-path-swr-key";
        let options = GetOrComputeOptions::new().stale_while_revalidate(Duration::from_secs(10));

        // A stale envelope (fresh_until in the past) must NOT be reported as
        // "the fill landed": the lock-poll loop in `run_leader_fill` relies on
        // `fast_path_value` to distinguish a fresh write by the lock winner
        // from the pre-existing stale value that triggered the refresh in the
        // first place. Reporting the stale value here would make a losing
        // replica stop waiting/polling prematurely and publish stale data as
        // if it were the completed refresh.
        let stale = SwrEnvelope {
            value: "stale-value".to_string(),
            fresh_until_unix_ms: 0,
        };
        insert_cached(cache.as_ref(), key, stale, None);
        assert_eq!(
            fast_path_value::<String>(&cache, key, &options),
            None,
            "a stale SWR envelope must not be reported as a completed fill"
        );

        // A fresh envelope (fresh_until far in the future) is reported.
        let fresh = SwrEnvelope {
            value: "fresh-value".to_string(),
            fresh_until_unix_ms: u64::MAX,
        };
        insert_cached(cache.as_ref(), key, fresh, None);
        assert_eq!(
            fast_path_value::<String>(&cache, key, &options),
            Some("fresh-value".to_string())
        );
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
