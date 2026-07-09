//! Named, cluster-wide distributed locks backed by `PostgreSQL` advisory locks.
//!
//! Autumn already gates its own migrations, `#[scheduled]` leader election, and
//! ISR revalidation on `PostgreSQL` advisory locks, but those helpers are private
//! to their modules. [`Lock`] promotes that capability into a small, safe,
//! app-facing API for "run this exactly once across the cluster right now" work:
//! nightly cleanup sweeps, cache warming, one-shot backfills, or "send the daily
//! digest once".
//!
//! ```no_run
//! # #[cfg(feature = "db")]
//! # async fn demo(pool: diesel_async::pooled_connection::deadpool::Pool<diesel_async::AsyncPgConnection>) -> Result<(), autumn_web::lock::LockError> {
//! use autumn_web::lock::Lock;
//!
//! let lock = Lock::new(pool, "nightly-cleanup");
//! // Runs the closure on exactly one replica; the others observe `None` and
//! // skip. The lock auto-releases when the section ends (normal return, early
//! // `?`, or panic).
//! let ran = lock
//!     .try_with(|| async {
//!         // ... critical section that must not run twice ...
//!     })
//!     .await?;
//! if ran.is_none() {
//!     // Another replica holds the lock right now — nothing to do here.
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # Semantics
//!
//! The lock is a `PostgreSQL` **session-scoped** advisory lock. The connection
//! that acquires it is detached from the pool for the lifetime of the guard and
//! is never returned to the shared pool while the lock is held — releasing the
//! lock (or, on panic, simply closing that session) is handled internally, so
//! the "recycled a lock-bearing connection and leaked the lock" footgun cannot
//! occur in application code.
//!
//! # Non-goals
//!
//! This is a *coordination* lock, not a durable mutual-exclusion queue:
//!
//! - **Not fair.** `PostgreSQL` advisory locks are not FIFO; waiters are not
//!   served in arrival order.
//! - **Not a lease.** There is no heartbeat/renewal; if the holder's connection
//!   drops, the lock releases. For long-lived leader election use the scheduler.
//! - **Not row-level.** Use `with_lock` (pessimistic) or optimistic locking for
//!   per-row contention; this is a *named*, row-independent lock.
//! - **`PostgreSQL` only.** Advisory-lock semantics assume `PostgreSQL`.

use std::time::Duration;

use sha2::{Digest as _, Sha256};

/// Domain-separation prefix for application distributed-lock keys.
///
/// String lock names are hashed together with this prefix so the app lock
/// keyspace cannot collide with the keyspaces the scheduler
/// (`autumn:scheduler`), migrations ([`crate::migrate::MIGRATION_ADVISORY_LOCK_KEY`]),
/// ISR revalidation (`isr`), or repository upserts (`repository_upsert`)
/// already use.
pub const DISTRIBUTED_LOCK_DOMAIN: &str = "autumn:lock:v1";

/// Derive a stable, collision-namespaced signed 64-bit advisory lock key for a
/// named application lock.
///
/// The result is a deterministic hash of [`DISTRIBUTED_LOCK_DOMAIN`] and `name`;
/// the same name always yields the same key, different names yield different
/// keys with overwhelming probability, and the domain prefix keeps these keys
/// out of the keyspaces used internally by the scheduler, migrations, and ISR.
///
/// # Examples
///
/// ```
/// use autumn_web::lock::distributed_lock_key;
///
/// let a = distributed_lock_key("nightly-cleanup");
/// let b = distributed_lock_key("nightly-cleanup");
/// assert_eq!(a, b);
/// assert_ne!(a, distributed_lock_key("daily-digest"));
/// ```
#[must_use]
pub fn distributed_lock_key(name: &str) -> i64 {
    let mut hasher = Sha256::new();
    hasher.update(DISTRIBUTED_LOCK_DOMAIN.as_bytes());
    hasher.update(b"\0");
    hasher.update(name.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    i64::from_be_bytes(bytes)
}

/// Errors returned when acquiring or releasing a distributed [`Lock`].
#[derive(Debug)]
#[non_exhaustive]
pub enum LockError {
    /// No database pool was available (the state has no primary pool
    /// configured, or the pool could not hand out a connection).
    PoolUnavailable(String),
    /// A database error occurred while acquiring or releasing the lock.
    Database(String),
    /// A blocking acquire with a timeout did not obtain the lock in time.
    Timeout {
        /// The lock name that timed out.
        name: String,
        /// How long the acquire waited before giving up.
        waited: Duration,
    },
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PoolUnavailable(msg) => {
                write!(f, "distributed lock pool unavailable: {msg}")
            }
            Self::Database(msg) => write!(f, "distributed lock database error: {msg}"),
            Self::Timeout { name, waited } => write!(
                f,
                "timed out after {:.3}s acquiring distributed lock {name:?}",
                waited.as_secs_f64()
            ),
        }
    }
}

impl std::error::Error for LockError {}

// `LockError` converts into `AutumnError` through the blanket
// `impl<E: std::error::Error> From<E>` in `crate::error`, which special-cases
// `LockError::PoolUnavailable` / `LockError::Timeout` to a `503 Service
// Unavailable` status (see `crate::error`).

/// Default poll interval used by the blocking, timed acquire
/// ([`Lock::lock_timeout`]).
pub const DEFAULT_LOCK_POLL_INTERVAL: Duration = Duration::from_millis(250);

#[cfg(feature = "db")]
pub use db_impl::{Lock, LockGuard};

#[cfg(feature = "db")]
mod db_impl {
    use std::time::{Duration, Instant};

    use diesel_async::AsyncPgConnection;
    use diesel_async::RunQueryDsl as _;
    use diesel_async::pooled_connection::deadpool::{Object, Pool};

    use super::{DEFAULT_LOCK_POLL_INTERVAL, LockError, distributed_lock_key};

    /// Lower bound on the effective [`Lock::lock_timeout`] poll interval.
    ///
    /// Clamps a zero (or sub-millisecond) poll interval up to 1ms so the poll
    /// loop yields to the runtime instead of busy-spinning.
    const MIN_LOCK_POLL_INTERVAL: Duration = Duration::from_millis(1);

    #[derive(diesel::QueryableByName)]
    struct AcquiredRow {
        #[diesel(sql_type = diesel::sql_types::Bool)]
        acquired: bool,
    }

    #[derive(diesel::QueryableByName)]
    struct ReleasedRow {
        #[diesel(sql_type = diesel::sql_types::Bool)]
        released: bool,
    }

    /// A handle to a named, cluster-wide distributed lock.
    ///
    /// Cheap to clone-construct (holds a pool handle, a name, and the derived
    /// key). Acquiring a lock checks out a dedicated connection that is held —
    /// and never recycled to the shared pool — until the lock is released. See
    /// the [module docs](crate::lock) for semantics and non-goals.
    #[derive(Clone)]
    pub struct Lock {
        pool: Pool<AsyncPgConnection>,
        name: String,
        key: i64,
        poll_interval: Duration,
    }

    impl Lock {
        /// Build a lock bound to `pool` and identified by `name`.
        ///
        /// `name` is hashed to a stable, namespaced 64-bit key (see
        /// [`distributed_lock_key`]). The lock is acquired on a connection from
        /// `pool`; pass a **primary** pool — advisory locks must be taken on the
        /// primary so every replica contends on the same server.
        #[must_use]
        pub fn new(pool: Pool<AsyncPgConnection>, name: impl Into<String>) -> Self {
            let name = name.into();
            let key = distributed_lock_key(&name);
            Self {
                pool,
                name,
                key,
                poll_interval: DEFAULT_LOCK_POLL_INTERVAL,
            }
        }

        /// Build a lock from any [`DbState`](crate::db::DbState), using its
        /// primary pool.
        ///
        /// # Errors
        ///
        /// Returns [`LockError::PoolUnavailable`] if the state has no primary
        /// database pool configured.
        pub fn from_state<S: crate::db::DbState + ?Sized>(
            state: &S,
            name: impl Into<String>,
        ) -> Result<Self, LockError> {
            let pool = state.pool().ok_or_else(|| {
                LockError::PoolUnavailable("no primary database pool configured".to_string())
            })?;
            Ok(Self::new(pool.clone(), name))
        }

        /// The lock's name.
        #[must_use]
        pub fn name(&self) -> &str {
            &self.name
        }

        /// The derived 64-bit advisory lock key.
        #[must_use]
        pub const fn key(&self) -> i64 {
            self.key
        }

        /// Override the poll interval used by [`Lock::lock_timeout`] (default
        /// [`DEFAULT_LOCK_POLL_INTERVAL`]).
        #[must_use]
        pub const fn with_poll_interval(mut self, interval: Duration) -> Self {
            self.poll_interval = interval;
            self
        }

        /// Check out a pooled connection to attempt an acquire on.
        ///
        /// The connection is returned as a pooled [`Object`], **not** detached:
        /// if the acquire fails the caller simply drops it and it recycles back
        /// to the pool. Only on a *successful* acquire does the caller
        /// [`Object::take`] it, permanently detaching the lock-bearing session
        /// so it can never be recycled while the lock is held.
        async fn checkout(&self) -> Result<Object<AsyncPgConnection>, LockError> {
            self.pool
                .get()
                .await
                .map_err(|e| LockError::PoolUnavailable(e.to_string()))
        }

        /// Try to acquire the lock without blocking.
        ///
        /// Returns `Ok(Some(guard))` if the lock was acquired, or `Ok(None)`
        /// immediately if another node currently holds it.
        ///
        /// # Errors
        ///
        /// Returns [`LockError`] if a connection could not be obtained or the
        /// database query failed.
        pub async fn try_lock(&self) -> Result<Option<LockGuard>, LockError> {
            let mut conn = self.checkout().await?;
            let acquired = diesel::sql_query("SELECT pg_try_advisory_lock($1) AS acquired")
                .bind::<diesel::sql_types::BigInt, _>(self.key)
                .get_result::<AcquiredRow>(&mut *conn)
                .await
                .map_err(|e| LockError::Database(e.to_string()))?
                .acquired;
            if acquired {
                // Acquired: detach the session from the pool so the
                // lock-bearing connection can never be recycled while held.
                let conn = Object::take(conn);
                Ok(Some(LockGuard::new(conn, self.key, self.name.clone())))
            } else {
                // Not acquired: drop the pooled `Object` so it recycles back to
                // the pool (do not close it — we lost the race, not the
                // connection).
                drop(conn);
                Ok(None)
            }
        }

        /// Acquire the lock, blocking until it becomes available.
        ///
        /// Uses server-side `pg_advisory_lock`, so the waiting is done by
        /// `PostgreSQL` (no client-side polling). For a bounded wait, use
        /// [`Lock::lock_timeout`].
        ///
        /// # Errors
        ///
        /// Returns [`LockError`] if a connection could not be obtained or the
        /// database query failed.
        pub async fn lock(&self) -> Result<LockGuard, LockError> {
            let mut conn = self.checkout().await?;
            diesel::sql_query("SELECT pg_advisory_lock($1)")
                .bind::<diesel::sql_types::BigInt, _>(self.key)
                .execute(&mut *conn)
                .await
                .map_err(|e| LockError::Database(e.to_string()))?;
            // Acquired: detach the session from the pool so the lock-bearing
            // connection can never be recycled while held.
            let conn = Object::take(conn);
            Ok(LockGuard::new(conn, self.key, self.name.clone()))
        }

        /// Acquire the lock, blocking up to `timeout`.
        ///
        /// Checks out a single connection and polls `pg_try_advisory_lock` on
        /// that same held connection at the configured poll interval until the
        /// lock is acquired or `timeout` elapses. Holding one connection for the
        /// whole wait avoids per-poll connection churn; on timeout the
        /// connection recycles back to the pool.
        ///
        /// # Errors
        ///
        /// Returns [`LockError::Timeout`] if the lock is not acquired within
        /// `timeout`, or another [`LockError`] on connection/query failure.
        pub async fn lock_timeout(&self, timeout: Duration) -> Result<LockGuard, LockError> {
            let start = Instant::now();
            // Check out one connection and reuse it for every poll iteration.
            let mut conn = self.checkout().await?;
            loop {
                let acquired = diesel::sql_query("SELECT pg_try_advisory_lock($1) AS acquired")
                    .bind::<diesel::sql_types::BigInt, _>(self.key)
                    .get_result::<AcquiredRow>(&mut *conn)
                    .await
                    .map_err(|e| LockError::Database(e.to_string()))?
                    .acquired;
                if acquired {
                    // Acquired: detach the session from the pool so the
                    // lock-bearing connection can never be recycled while held.
                    let conn = Object::take(conn);
                    return Ok(LockGuard::new(conn, self.key, self.name.clone()));
                }
                let elapsed = start.elapsed();
                if elapsed >= timeout {
                    // Timed out: drop the pooled `Object` so it recycles back to
                    // the pool (the lock was never acquired on it).
                    drop(conn);
                    return Err(LockError::Timeout {
                        name: self.name.clone(),
                        waited: elapsed,
                    });
                }
                let remaining = timeout.saturating_sub(elapsed);
                // Clamp the effective poll interval to a small minimum so a
                // zero (or sub-millisecond) interval cannot busy-spin, but never
                // sleep past the remaining budget.
                let poll = self
                    .poll_interval
                    .max(MIN_LOCK_POLL_INTERVAL)
                    .min(remaining);
                tokio::time::sleep(poll).await;
            }
        }

        /// Run `f` while holding the lock, blocking to acquire it first.
        ///
        /// The lock auto-releases when `f` finishes — on normal return, on an
        /// early `?` inside `f`, or on panic. Returns whatever `f` returns.
        ///
        /// # Errors
        ///
        /// Returns [`LockError`] if the lock could not be acquired.
        pub async fn with<F, Fut, T>(&self, f: F) -> Result<T, LockError>
        where
            F: FnOnce() -> Fut,
            Fut: std::future::Future<Output = T>,
        {
            let guard = self.lock().await?;
            Ok(run_guarded(guard, f).await)
        }

        /// Run `f` while holding the lock, blocking up to `timeout` to acquire.
        ///
        /// # Errors
        ///
        /// Returns [`LockError::Timeout`] if the lock is not acquired within
        /// `timeout`, or another [`LockError`] on connection/query failure.
        pub async fn with_timeout<F, Fut, T>(&self, timeout: Duration, f: F) -> Result<T, LockError>
        where
            F: FnOnce() -> Fut,
            Fut: std::future::Future<Output = T>,
        {
            let guard = self.lock_timeout(timeout).await?;
            Ok(run_guarded(guard, f).await)
        }

        /// Run `f` only if the lock can be acquired without blocking.
        ///
        /// Returns `Ok(Some(f_result))` if the lock was acquired and `f` ran, or
        /// `Ok(None)` if another node currently holds the lock. The lock
        /// auto-releases when `f` finishes (including on panic).
        ///
        /// # Errors
        ///
        /// Returns [`LockError`] if a connection could not be obtained or the
        /// database query failed.
        pub async fn try_with<F, Fut, T>(&self, f: F) -> Result<Option<T>, LockError>
        where
            F: FnOnce() -> Fut,
            Fut: std::future::Future<Output = T>,
        {
            let Some(guard) = self.try_lock().await? else {
                return Ok(None);
            };
            Ok(Some(run_guarded(guard, f).await))
        }
    }

    /// Run `f` to completion, then explicitly release `guard`.
    ///
    /// If `f` panics, `guard`'s `Drop` releases the lock by closing its session
    /// as the stack unwinds. On normal completion we release explicitly so the
    /// unlock is issued promptly (and any release error is logged rather than
    /// masking `f`'s result).
    async fn run_guarded<F, Fut, T>(guard: LockGuard, f: F) -> T
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        let result = f().await;
        if let Err(e) = guard.release().await {
            tracing::warn!(error = %e, "distributed lock release failed after guarded section");
        }
        result
    }

    /// An acquired distributed lock. Releases automatically on drop.
    ///
    /// Prefer [`LockGuard::release`] to release explicitly (it issues
    /// `pg_advisory_unlock` and returns a typed error on failure). If the guard
    /// is simply dropped — including while unwinding from a panic — the lock is
    /// released by closing the underlying session, which `PostgreSQL` treats as
    /// releasing every session-scoped advisory lock it held.
    pub struct LockGuard {
        conn: Option<AsyncPgConnection>,
        key: i64,
        name: String,
    }

    impl LockGuard {
        const fn new(conn: AsyncPgConnection, key: i64, name: String) -> Self {
            Self {
                conn: Some(conn),
                key,
                name,
            }
        }

        /// The lock's name.
        #[must_use]
        pub fn name(&self) -> &str {
            &self.name
        }

        /// The derived 64-bit advisory lock key.
        #[must_use]
        pub const fn key(&self) -> i64 {
            self.key
        }

        /// Explicitly release the lock, issuing `pg_advisory_unlock`.
        ///
        /// The lock-bearing session is closed afterwards (the connection is
        /// never returned to the pool).
        ///
        /// # Errors
        ///
        /// Returns [`LockError::Database`] if the unlock query failed. The lock
        /// is still released regardless, because the session is closed.
        pub async fn release(mut self) -> Result<(), LockError> {
            let Some(mut conn) = self.conn.take() else {
                return Ok(());
            };
            let result = diesel::sql_query("SELECT pg_advisory_unlock($1) AS released")
                .bind::<diesel::sql_types::BigInt, _>(self.key)
                .get_result::<ReleasedRow>(&mut conn)
                .await;
            // Closing the session releases the lock server-side no matter what.
            drop(conn);
            match result {
                Ok(row) if row.released => Ok(()),
                Ok(_) => {
                    tracing::warn!(
                        lock_name = %self.name,
                        lock_key = self.key,
                        "distributed lock unlock returned false (lock was not held)"
                    );
                    Ok(())
                }
                Err(e) => Err(LockError::Database(e.to_string())),
            }
        }
    }

    impl std::fmt::Debug for LockGuard {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("LockGuard")
                .field("name", &self.name)
                .field("key", &self.key)
                .field("held", &self.conn.is_some())
                .finish()
        }
    }

    impl Drop for LockGuard {
        fn drop(&mut self) {
            if let Some(conn) = self.conn.take() {
                // Released implicitly (early return without `release`, or panic
                // unwind): we cannot run the async `pg_advisory_unlock` here, so
                // drop the detached connection. Closing the session releases
                // every session-scoped advisory lock PostgreSQL held for it.
                drop(conn);
                tracing::debug!(
                    lock_name = %self.name,
                    lock_key = self.key,
                    "distributed lock released by closing its session on drop"
                );
            }
        }
    }
}

// Gated on `db`: several assertions reference `db`-only namespacing helpers
// (`crate::migrate::MIGRATION_ADVISORY_LOCK_KEY`,
// `crate::repository::repository_upsert_advisory_lock_key`), so this module must
// not compile with the `db` feature off.
#[cfg(all(test, feature = "db"))]
mod tests {
    use super::*;

    #[test]
    fn key_is_deterministic() {
        assert_eq!(
            distributed_lock_key("nightly-cleanup"),
            distributed_lock_key("nightly-cleanup")
        );
    }

    #[test]
    fn key_differs_by_name() {
        assert_ne!(
            distributed_lock_key("nightly-cleanup"),
            distributed_lock_key("daily-digest")
        );
    }

    #[test]
    fn key_is_namespaced_away_from_other_keyspaces() {
        // The domain prefix keeps app lock keys out of the keyspaces the
        // scheduler, migrations, ISR, and repository upserts already use, even
        // for identical input strings.
        let name = "cleanup";
        let app = distributed_lock_key(name);

        // Scheduler keys are SHA256 of (prefix, task, tick) with different
        // framing; a same-named app lock must not land on a scheduler key.
        assert_ne!(
            app,
            crate::scheduler::advisory_lock_key("autumn:scheduler", name, name)
        );
        // ISR keys are SHA256 of ("isr", route, window).
        assert_ne!(app, crate::static_gen::isr_advisory_lock_key(name, name));
        // Repository-upsert keys are SHA256 of ("repository_upsert", table, id).
        assert_ne!(
            app,
            crate::repository::repository_upsert_advisory_lock_key(name, 0)
        );
        // The fixed migration lock key is a compile-time constant.
        assert_ne!(app, crate::migrate::MIGRATION_ADVISORY_LOCK_KEY);
    }

    #[test]
    fn empty_and_whitespace_names_are_distinct() {
        assert_ne!(distributed_lock_key(""), distributed_lock_key(" "));
        assert_ne!(distributed_lock_key("a"), distributed_lock_key("a "));
    }

    #[test]
    fn timeout_error_displays_name_and_duration() {
        let err = LockError::Timeout {
            name: "nightly-cleanup".to_string(),
            waited: std::time::Duration::from_millis(1500),
        };
        let msg = err.to_string();
        assert!(msg.contains("nightly-cleanup"), "message: {msg}");
        assert!(
            msg.contains("1.5"),
            "message should include the wait: {msg}"
        );
    }

    #[test]
    fn pool_unavailable_error_maps_to_service_unavailable() {
        let err: crate::error::AutumnError =
            LockError::PoolUnavailable("no pool".to_string()).into();
        assert_eq!(
            err.status(),
            crate::reexports::http::StatusCode::SERVICE_UNAVAILABLE
        );
    }
}
