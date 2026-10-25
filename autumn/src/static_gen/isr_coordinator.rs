//! ISR coordination backends.
//!
//! Controls which replica may regenerate a given route within a revalidation
//! window. The [`LocalIsrCoordinator`] is a true no-op: all local
//! deduplication is already handled by the per-route `AtomicBool` in
//! `StaticFileLayer`, so a single process never needs an additional lock.
//! The [`PostgresIsrCoordinator`] uses `pg_try_advisory_lock` to ensure that
//! at most one replica regenerates a given route per revalidation window
//! across the entire fleet.
//!
//! ## Local vs multi-replica contract
//!
//! | Deployment | Recommended coordinator | Guarantee |
//! |------------|-------------------------|-----------|
//! | Single process / dev | `LocalIsrCoordinator` (default) | At most one in-flight task per route per process |
//! | Multi-replica (shared `dist/`) | `PostgresIsrCoordinator` | At most one regeneration per route per revalidation window across the fleet |
//! | Read-only `dist/` (build-time only) | Disable ISR (`revalidate` = None) | No runtime writes |
//!
//! ## Atomic writes
//!
//! Regardless of coordinator, regeneration always writes to a `.tmp` file
//! then renames atomically so a reader never observes a partially written
//! page.

use std::future::Future;
use std::pin::Pin;

use sha2::{Digest as _, Sha256};

/// Boxed async future returned by coordinator operations.
pub type IsrFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Coordination backend for ISR background regeneration.
///
/// Implementations decide whether *this* replica may regenerate a given route
/// for a given revalidation window. Returning `false` from
/// [`try_acquire`](Self::try_acquire) means another task or replica has
/// already claimed the work; the caller should skip regeneration.
///
/// Implementors must also call [`release`](Self::release) after the
/// regeneration attempt so that subsequent windows can be acquired.
pub trait IsrCoordinator: Send + Sync + 'static {
    /// Short backend identifier used in log messages.
    fn backend(&self) -> &'static str;

    /// Try to acquire the right to regenerate `url_path` for `window_key`.
    ///
    /// `window_key` is derived from [`isr_window_key`] and encodes both the
    /// route and the current revalidation bucket; two replicas that observe
    /// the same stale file within the same window will produce the same key.
    ///
    /// Returns `true` when this caller may proceed; `false` when another
    /// task or replica already holds the lock for this (route, window) pair.
    fn try_acquire<'a>(&'a self, url_path: &'a str, window_key: &'a str) -> IsrFuture<'a, bool>;

    /// Release the lock after regeneration completes (success or failure).
    ///
    /// Must be called exactly once for every successful [`try_acquire`](Self::try_acquire).
    fn release<'a>(&'a self, url_path: &'a str, window_key: &'a str) -> IsrFuture<'a, ()>;
}

/// In-process ISR coordinator — always grants the lock.
///
/// This is the **default** coordinator and is a true no-op. Local
/// deduplication is entirely handled by the per-route `AtomicBool` in
/// `StaticFileLayer`: only one background task can be spawned per route at
/// a time, so the coordinator will never see a concurrent call for the same
/// (route, window) pair within a single process.
///
/// For multi-replica deployments use [`PostgresIsrCoordinator`] (feature
/// `db`) which enforces fleet-wide deduplication via `pg_try_advisory_lock`.
#[derive(Debug, Default)]
pub struct LocalIsrCoordinator;

impl LocalIsrCoordinator {
    /// Create a new in-process coordinator.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl IsrCoordinator for LocalIsrCoordinator {
    fn backend(&self) -> &'static str {
        "local"
    }

    fn try_acquire<'a>(&'a self, _url_path: &'a str, _window_key: &'a str) -> IsrFuture<'a, bool> {
        Box::pin(async move { true })
    }

    fn release<'a>(&'a self, _url_path: &'a str, _window_key: &'a str) -> IsrFuture<'a, ()> {
        Box::pin(async move {})
    }
}

/// Postgres advisory-lock ISR coordinator.
///
/// Uses `pg_try_advisory_lock` / `pg_advisory_unlock` to prevent duplicate
/// regeneration across replicas. Requires the `db` feature.
///
/// The advisory lock is keyed on [`isr_advisory_lock_key`] which is derived
/// from (route, window) -- two replicas that see the same stale page in the
/// same revalidation window will attempt the same lock key, and only the
/// first will succeed.
///
/// ## Connection management
///
/// Postgres session-level advisory locks are bound to the database connection
/// that acquired them. This coordinator therefore **holds the connection** that
/// won the lock (stored in an internal map keyed by advisory lock key) and
/// retrieves the same connection in [`IsrCoordinator::release`] to call
/// `pg_advisory_unlock` before returning it to the pool.
///
/// Connections held during regeneration are not available to other tasks.
/// Regeneration tasks that fail to acquire the Postgres lock return their
/// pooled connection immediately.
#[cfg(feature = "postgres")]
pub struct PostgresIsrCoordinator {
    pool: diesel_async::pooled_connection::deadpool::Pool<diesel_async::AsyncPgConnection>,
    /// Connections holding active advisory locks, keyed by advisory lock key.
    /// The connection must be reused for the unlock call because session-level
    /// advisory locks are connection-scoped.
    held: std::sync::Mutex<
        std::collections::HashMap<
            i64,
            diesel_async::pooled_connection::deadpool::Object<diesel_async::AsyncPgConnection>,
        >,
    >,
}

#[cfg(feature = "postgres")]
impl PostgresIsrCoordinator {
    /// Create a Postgres ISR coordinator backed by the given connection pool.
    #[must_use]
    pub fn new(
        pool: diesel_async::pooled_connection::deadpool::Pool<diesel_async::AsyncPgConnection>,
    ) -> Self {
        Self {
            pool,
            held: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }
}

#[cfg(feature = "postgres")]
impl IsrCoordinator for PostgresIsrCoordinator {
    fn backend(&self) -> &'static str {
        "postgres"
    }

    fn try_acquire<'a>(&'a self, url_path: &'a str, window_key: &'a str) -> IsrFuture<'a, bool> {
        let lock_key = isr_advisory_lock_key(url_path, window_key);
        Box::pin(async move {
            let mut conn = match self.pool.get().await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error = %e, "ISR coordinator: could not get Postgres connection");
                    return false;
                }
            };
            match try_pg_advisory_lock(&mut conn, lock_key).await {
                Ok(true) => {
                    // Hold the connection so we can unlock on the same session.
                    self.held.lock().unwrap().insert(lock_key, conn);
                    true
                }
                Ok(false) => false,
                Err(e) => {
                    tracing::warn!(error = %e, "ISR coordinator: pg_try_advisory_lock failed");
                    false
                }
            }
        })
    }

    fn release<'a>(&'a self, url_path: &'a str, window_key: &'a str) -> IsrFuture<'a, ()> {
        let lock_key = isr_advisory_lock_key(url_path, window_key);
        Box::pin(async move {
            // Retrieve the connection that holds the lock.
            // Extract outside `let...else` so the mutex guard drops immediately.
            let maybe_conn = self.held.lock().unwrap().remove(&lock_key);
            let Some(mut conn) = maybe_conn else {
                tracing::warn!(
                    lock_key,
                    "ISR coordinator: release called without a held connection"
                );
                return;
            };
            match unlock_pg_advisory_lock(&mut conn, lock_key).await {
                Ok(false) => {
                    tracing::warn!(
                        lock_key,
                        "ISR coordinator: pg_advisory_unlock returned false (lock already released)"
                    );
                }
                Ok(true) => {}
                Err(e) => {
                    tracing::warn!(error = %e, "ISR coordinator: pg_advisory_unlock failed");
                }
            }
            // conn is dropped here, returning the connection to the pool.
        })
    }
}

// ---------------------------------------------------------------------------
// Key derivation utilities (public -- useful in application code and tests)
// ---------------------------------------------------------------------------

/// Compute the revalidation window key for a route and the current time.
///
/// All replicas that evaluate a given route as stale within the same
/// revalidation period will produce the same key, making it safe to use
/// as a distributed lock discriminator.
///
/// # Arguments
///
/// * `url_path` -- The URL path of the route (e.g. `"/about"`).
/// * `revalidate_secs` -- The ISR interval in seconds (0 is treated as 1).
/// * `now_unix_secs` -- Current Unix timestamp in seconds.
///
/// # Returns
///
/// A string of the form `"{url_path}:{bucket}"` where `bucket = now / interval`.
///
/// # Examples
///
/// ```rust
/// use autumn_web::static_gen::isr_window_key;
///
/// let url_path = "/about";
/// let revalidate_secs = 60;
/// let now_unix_secs = 1_700_000_000;
/// let key = isr_window_key(url_path, revalidate_secs, now_unix_secs);
/// assert_eq!(key, "/about:28333333");
/// ```
#[must_use]
pub fn isr_window_key(url_path: &str, revalidate_secs: u64, now_unix_secs: u64) -> String {
    let interval = revalidate_secs.max(1);
    let bucket = now_unix_secs / interval;
    format!("{url_path}:{bucket}")
}

/// Derive a stable signed 64-bit advisory lock key for a (route, window) pair.
///
/// Suitable for `pg_try_advisory_lock`. The result is a deterministic hash
/// of the inputs; different routes or different windows produce different keys
/// with overwhelming probability.
///
/// # Examples
///
/// ```rust
/// use autumn_web::static_gen::{isr_window_key, isr_advisory_lock_key};
///
/// let window_key = isr_window_key("/about", 60, 1_700_000_000);
/// let lock_key = isr_advisory_lock_key("/about", &window_key);
/// // The value is a stable 64-bit integer
/// assert_ne!(lock_key, 0);
/// ```
#[must_use]
pub fn isr_advisory_lock_key(url_path: &str, window_key: &str) -> i64 {
    let mut hasher = Sha256::new();
    hasher.update(b"isr\0");
    hasher.update(url_path.as_bytes());
    hasher.update(b"\0");
    hasher.update(window_key.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    i64::from_be_bytes(bytes)
}

// ---------------------------------------------------------------------------
// Postgres helpers (feature = "postgres")
// ---------------------------------------------------------------------------

#[cfg(feature = "postgres")]
#[derive(diesel::QueryableByName)]
struct PgAdvisoryLockRow {
    #[diesel(sql_type = diesel::sql_types::Bool)]
    acquired: bool,
}

#[cfg(feature = "postgres")]
async fn try_pg_advisory_lock(
    conn: &mut diesel_async::pooled_connection::deadpool::Object<diesel_async::AsyncPgConnection>,
    key: i64,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    use diesel_async::RunQueryDsl as _;

    let row = diesel::sql_query("SELECT pg_try_advisory_lock($1) AS acquired")
        .bind::<diesel::sql_types::BigInt, _>(key)
        .get_result::<PgAdvisoryLockRow>(&mut **conn)
        .await?;
    Ok(row.acquired)
}

#[cfg(feature = "postgres")]
#[derive(diesel::QueryableByName)]
struct PgAdvisoryUnlockRow {
    #[diesel(sql_type = diesel::sql_types::Bool)]
    released: bool,
}

#[cfg(feature = "postgres")]
async fn unlock_pg_advisory_lock(
    conn: &mut diesel_async::pooled_connection::deadpool::Object<diesel_async::AsyncPgConnection>,
    key: i64,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    use diesel_async::RunQueryDsl as _;

    let row = diesel::sql_query("SELECT pg_advisory_unlock($1) AS released")
        .bind::<diesel::sql_types::BigInt, _>(key)
        .get_result::<PgAdvisoryUnlockRow>(&mut **conn)
        .await?;
    Ok(row.released)
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- isr_window_key ---

    #[test]
    fn window_key_stable_within_interval() {
        // Bucket 28_333_333 covers [1_699_999_980, 1_700_000_039].
        let a = isr_window_key("/about", 60, 1_700_000_000);
        let b = isr_window_key("/about", 60, 1_700_000_039);
        assert_eq!(a, b);
    }

    #[test]
    fn window_key_changes_on_boundary() {
        // 1_700_000_039 is bucket 28_333_333; 1_700_000_040 is bucket 28_333_334.
        let a = isr_window_key("/about", 60, 1_700_000_039);
        let b = isr_window_key("/about", 60, 1_700_000_040);
        assert_ne!(a, b);
    }

    #[test]
    fn window_key_route_prefix() {
        let key = isr_window_key("/about", 60, 1_700_000_000);
        assert!(
            key.starts_with("/about:"),
            "key should start with route: {key}"
        );
    }

    #[test]
    fn window_key_zero_revalidate_no_panic() {
        let key = isr_window_key("/edge", 0, 42);
        assert!(!key.is_empty());
    }

    // --- isr_advisory_lock_key ---

    #[test]
    fn advisory_key_deterministic() {
        let a = isr_advisory_lock_key("/about", "/about:28333333");
        let b = isr_advisory_lock_key("/about", "/about:28333333");
        assert_eq!(a, b);
    }

    #[test]
    fn advisory_key_differs_by_route() {
        let a = isr_advisory_lock_key("/", "/about:28333333");
        let b = isr_advisory_lock_key("/about", "/about:28333333");
        assert_ne!(a, b);
    }

    #[test]
    fn advisory_key_differs_by_window() {
        let a = isr_advisory_lock_key("/about", "/about:1");
        let b = isr_advisory_lock_key("/about", "/about:2");
        assert_ne!(a, b);
    }

    // --- LocalIsrCoordinator ---

    #[tokio::test]
    async fn local_coordinator_always_grants() {
        // LocalIsrCoordinator is a no-op: local dedup is handled by the
        // AtomicBool in StaticFileLayer, not by this coordinator.
        let c = LocalIsrCoordinator::new();
        assert!(c.try_acquire("/a", "w1").await);
        // Even a second concurrent call returns true (no-op).
        assert!(c.try_acquire("/a", "w1").await);
    }

    #[tokio::test]
    async fn local_coordinator_release_is_noop() {
        let c = LocalIsrCoordinator::new();
        // release should not panic and the coordinator continues to grant.
        c.release("/a", "w1").await;
        assert!(c.try_acquire("/a", "w1").await);
    }
}
