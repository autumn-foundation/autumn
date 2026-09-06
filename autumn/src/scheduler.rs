//! Scheduled-task coordination backends.
//!
//! The in-process backend preserves the original single-process behavior.
//! The Postgres backend uses advisory locks so each fleet-wide task tick is
//! claimed by at most one replica under normal operation.

// autumn-determinism-gate: production code in this module must read time and
// mint identifiers through the framework's injected seams (ClockSource /
// Entropy), never `Instant::now()` / `Utc::now()` / `SystemTime::now()` /
// `Uuid::new_v4()` directly. See CONTRIBUTING.md "Determinism seam gate"
// (issue #1797). Justify exceptions with
// #[allow(clippy::disallowed_methods, reason = "…")] at the narrowest scope.
#![cfg_attr(not(test), deny(clippy::disallowed_methods))]
// autumn-panic-gate: request-path module — production code path must be panic-free.
// See CONTRIBUTING.md "Request-path panic gate". Justify exceptions with
// #[allow(clippy::<lint>, reason = "…")] at the narrowest scope.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
        clippy::string_slice,
        clippy::arithmetic_side_effects,
    )
)]

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest as _, Sha256};

use crate::config::{SchedulerBackend, SchedulerConfig};
use crate::state::AppState;
use crate::task::TaskCoordination;
use crate::{AutumnError, AutumnResult};

/// Boxed future returned by scheduler coordination operations.
pub type SchedulerFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A configured scheduler backend that decides whether this replica may run a tick.
pub trait SchedulerCoordinator: Send + Sync {
    /// Backend identifier surfaced in logs and actuator metadata.
    fn backend(&self) -> &'static str;

    /// Stable replica identifier surfaced in actuator metadata.
    fn replica_id(&self) -> &str;

    /// Whether this coordinator coordinates across a fleet of replicas,
    /// rather than a single process (issue #1864).
    ///
    /// Lets a call site ask the coordinator directly instead of matching the
    /// [`SchedulerBackend`] config enum. The default derives it from
    /// [`Self::backend`], so existing implementors — including test doubles —
    /// need no changes; override it only if a future backend's `backend()`
    /// string diverges from its fleet-distribution semantics.
    fn is_fleet_distributed(&self) -> bool {
        self.backend() == "postgres"
    }

    /// Try to acquire permission to run `task_name` for `tick_key`.
    fn try_acquire<'a>(
        &'a self,
        task_name: &'a str,
        tick_key: &'a str,
        coordination: TaskCoordination,
    ) -> SchedulerFuture<'a, AutumnResult<Option<SchedulerLease>>>;
}

/// Acquired permission to run a scheduled task tick.
pub struct SchedulerLease {
    backend: String,
    leader_id: String,
    #[cfg(test)]
    release_count: Option<Arc<std::sync::atomic::AtomicUsize>>,
    #[cfg(feature = "db")]
    postgres: Option<PostgresAdvisoryLease>,
    #[cfg(feature = "sqlite")]
    sqlite: Option<SqliteTableLease>,
}

impl SchedulerLease {
    pub(crate) fn local(backend: impl Into<String>, leader_id: impl Into<String>) -> Self {
        Self {
            backend: backend.into(),
            leader_id: leader_id.into(),
            #[cfg(test)]
            release_count: None,
            #[cfg(feature = "db")]
            postgres: None,
            #[cfg(feature = "sqlite")]
            sqlite: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn tracked(
        backend: impl Into<String>,
        leader_id: impl Into<String>,
        release_count: Arc<std::sync::atomic::AtomicUsize>,
    ) -> Self {
        Self {
            backend: backend.into(),
            leader_id: leader_id.into(),
            release_count: Some(release_count),
            #[cfg(feature = "db")]
            postgres: None,
            #[cfg(feature = "sqlite")]
            sqlite: None,
        }
    }

    #[cfg(feature = "db")]
    fn postgres(leader_id: impl Into<String>, lease: PostgresAdvisoryLease) -> Self {
        Self {
            backend: "postgres".to_owned(),
            leader_id: leader_id.into(),
            #[cfg(test)]
            release_count: None,
            postgres: Some(lease),
            #[cfg(feature = "sqlite")]
            sqlite: None,
        }
    }

    /// A lease granted by the `SQLite` lease-table coordinator.
    #[cfg(feature = "sqlite")]
    fn sqlite(leader_id: impl Into<String>, lease: SqliteTableLease) -> Self {
        Self {
            backend: "sqlite".to_owned(),
            leader_id: leader_id.into(),
            #[cfg(test)]
            release_count: None,
            postgres: None,
            sqlite: Some(lease),
        }
    }

    /// Backend that granted this lease.
    #[must_use]
    pub fn backend(&self) -> &str {
        &self.backend
    }

    /// Replica currently considered leader for this tick.
    #[must_use]
    pub fn leader_id(&self) -> &str {
        &self.leader_id
    }

    /// Release backend resources associated with this lease.
    ///
    /// # Errors
    ///
    /// Returns [`AutumnError`] when the backend cannot release its lock.
    pub async fn release(self) -> AutumnResult<()> {
        #[cfg(test)]
        if let Some(release_count) = self.release_count {
            release_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }

        #[cfg(feature = "db")]
        if let Some(lease) = self.postgres {
            return lease.release().await;
        }

        #[cfg(feature = "sqlite")]
        if let Some(lease) = self.sqlite {
            return lease.release().await;
        }

        Ok(())
    }
}

/// Local coordinator that always lets this process run.
#[derive(Debug, Clone)]
pub struct InProcessSchedulerCoordinator {
    replica_id: String,
}

impl InProcessSchedulerCoordinator {
    /// Create an in-process coordinator for a replica id.
    #[must_use]
    pub fn new(replica_id: impl Into<String>) -> Self {
        Self {
            replica_id: replica_id.into(),
        }
    }
}

impl SchedulerCoordinator for InProcessSchedulerCoordinator {
    fn backend(&self) -> &'static str {
        "in_process"
    }

    fn replica_id(&self) -> &str {
        &self.replica_id
    }

    fn try_acquire<'a>(
        &'a self,
        _task_name: &'a str,
        _tick_key: &'a str,
        coordination: TaskCoordination,
    ) -> SchedulerFuture<'a, AutumnResult<Option<SchedulerLease>>> {
        Box::pin(async move {
            let backend = match coordination {
                TaskCoordination::Fleet => "in_process",
                TaskCoordination::PerReplica => "per_replica",
            };
            Ok(Some(SchedulerLease::local(
                backend,
                self.replica_id.clone(),
            )))
        })
    }
}

/// Postgres advisory-lock coordinator.
#[cfg(feature = "db")]
#[derive(Clone)]
pub struct PostgresAdvisorySchedulerCoordinator {
    pool: diesel_async::pooled_connection::deadpool::Pool<diesel_async::AsyncPgConnection>,
    replica_id: String,
    key_prefix: String,
}

#[cfg(feature = "db")]
impl PostgresAdvisorySchedulerCoordinator {
    /// Create a Postgres advisory-lock coordinator.
    #[must_use]
    pub fn new(
        pool: diesel_async::pooled_connection::deadpool::Pool<diesel_async::AsyncPgConnection>,
        replica_id: impl Into<String>,
        key_prefix: impl Into<String>,
    ) -> Self {
        Self {
            pool,
            replica_id: replica_id.into(),
            key_prefix: key_prefix.into(),
        }
    }
}

#[cfg(feature = "db")]
impl SchedulerCoordinator for PostgresAdvisorySchedulerCoordinator {
    fn backend(&self) -> &'static str {
        "postgres"
    }

    fn replica_id(&self) -> &str {
        &self.replica_id
    }

    fn try_acquire<'a>(
        &'a self,
        task_name: &'a str,
        tick_key: &'a str,
        coordination: TaskCoordination,
    ) -> SchedulerFuture<'a, AutumnResult<Option<SchedulerLease>>> {
        Box::pin(async move {
            if coordination == TaskCoordination::PerReplica {
                return Ok(Some(SchedulerLease::local(
                    "per_replica",
                    self.replica_id.clone(),
                )));
            }

            let key = advisory_lock_key(&self.key_prefix, task_name, tick_key);
            let mut conn = self.pool.get().await.map_err(|error| {
                AutumnError::service_unavailable_msg(format!(
                    "scheduler postgres lock connection unavailable: {error}"
                ))
            })?;
            let acquired = try_pg_advisory_lock(&mut conn, key).await?;
            if acquired {
                Ok(Some(SchedulerLease::postgres(
                    self.replica_id.clone(),
                    PostgresAdvisoryLease {
                        conn: Some(conn),
                        key,
                    },
                )))
            } else {
                Ok(None)
            }
        })
    }
}

#[cfg(feature = "db")]
struct PostgresAdvisoryLease {
    conn:
        Option<diesel_async::pooled_connection::deadpool::Object<diesel_async::AsyncPgConnection>>,
    key: i64,
}

#[cfg(feature = "db")]
impl PostgresAdvisoryLease {
    async fn release(mut self) -> AutumnResult<()> {
        let Some(mut conn) = self.conn.take() else {
            return Ok(());
        };
        let released = unlock_pg_advisory_lock(&mut conn, self.key).await?;
        if !released {
            tracing::warn!(
                lock_key = self.key,
                "Postgres advisory scheduler lock was already released"
            );
        }
        Ok(())
    }
}

/// Single-host lease coordinator for the `SQLite` backend (issue #1907).
///
/// Leases each `(task, tick)` in a table in the app's own database file, so
/// several processes on one host elect exactly one leader per tick.
///
/// A lease carries an expiry, not a session, so a leader that dies frees the
/// tick after `scheduler.lease_ttl_secs` instead of wedging the task. Set the
/// TTL above the longest a tick body can take. See
/// `docs/guide/scheduled-multi-replica.md`.
#[cfg(feature = "sqlite")]
#[derive(Clone)]
pub struct SqliteLeaseSchedulerCoordinator {
    pool: diesel_async::pooled_connection::deadpool::Pool<crate::db::RuntimeConnection>,
    replica_id: String,
    key_prefix: String,
    lease_ttl: Duration,
    clock: Arc<dyn crate::time::ClockSource>,
    ready: Arc<tokio::sync::OnceCell<()>>,
}

#[cfg(feature = "sqlite")]
impl SqliteLeaseSchedulerCoordinator {
    /// Create a `SQLite` lease coordinator over the app's primary pool.
    #[must_use]
    pub fn new(
        pool: diesel_async::pooled_connection::deadpool::Pool<crate::db::RuntimeConnection>,
        replica_id: impl Into<String>,
        key_prefix: impl Into<String>,
        lease_ttl: Duration,
        clock: Arc<dyn crate::time::ClockSource>,
    ) -> Self {
        Self {
            pool,
            replica_id: replica_id.into(),
            key_prefix: key_prefix.into(),
            lease_ttl,
            clock,
            ready: Arc::new(tokio::sync::OnceCell::new()),
        }
    }

    /// Create the lease table on first use.
    ///
    /// Framework migrations are Postgres SQL and do not run on `SQLite`, so the
    /// runtime owns this schema. A failed attempt leaves the cell empty, so the
    /// next acquire retries.
    async fn ensure_table(&self, conn: &mut crate::db::RuntimeConnection) -> AutumnResult<()> {
        let ready = Arc::clone(&self.ready);
        ready.get_or_try_init(|| Self::create_table(conn)).await?;
        Ok(())
    }

    /// The DDL itself. Idempotent, and safe to run from several processes at
    /// once: `SQLite` serializes writers and every statement is `IF NOT EXISTS`.
    async fn create_table(conn: &mut crate::db::RuntimeConnection) -> AutumnResult<()> {
        use diesel_async::RunQueryDsl as _;

        for statement in [
            "CREATE TABLE IF NOT EXISTS autumn_scheduler_leases ( \
               lock_key    BIGINT PRIMARY KEY NOT NULL, \
               task_name   TEXT   NOT NULL, \
               tick_key    TEXT   NOT NULL, \
               owner       TEXT   NOT NULL, \
               acquired_at BIGINT NOT NULL, \
               expires_at  BIGINT NOT NULL)",
            "CREATE INDEX IF NOT EXISTS idx_autumn_scheduler_leases_expiry \
             ON autumn_scheduler_leases (expires_at)",
        ] {
            diesel::sql_query(statement)
                .execute(&mut *conn)
                .await
                .map_err(|error| {
                    AutumnError::internal_server_error_msg(format!(
                        "sqlite scheduler lease table setup failed: {error}"
                    ))
                })?;
        }
        Ok(())
    }

    /// Current wall time in milliseconds, read from the injected clock.
    fn now_ms(&self) -> i64 {
        self.clock.now().timestamp_millis()
    }
}

/// Owner token minted per acquire, recorded on the row so an operator reading
/// the table can tell which process claimed a tick.
#[cfg(feature = "sqlite")]
fn next_lease_owner(replica_id: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{replica_id}#{}#{seq}", std::process::id())
}

#[cfg(feature = "sqlite")]
impl SchedulerCoordinator for SqliteLeaseSchedulerCoordinator {
    fn backend(&self) -> &'static str {
        "sqlite"
    }

    fn replica_id(&self) -> &str {
        &self.replica_id
    }

    /// The lease table is shared by every process on the host, so a fleet task
    /// runs on exactly one of them.
    fn is_fleet_distributed(&self) -> bool {
        true
    }

    fn try_acquire<'a>(
        &'a self,
        task_name: &'a str,
        tick_key: &'a str,
        coordination: TaskCoordination,
    ) -> SchedulerFuture<'a, AutumnResult<Option<SchedulerLease>>> {
        Box::pin(async move {
            use diesel_async::RunQueryDsl as _;

            if coordination == TaskCoordination::PerReplica {
                return Ok(Some(SchedulerLease::local(
                    "per_replica",
                    self.replica_id.clone(),
                )));
            }

            let key = advisory_lock_key(&self.key_prefix, task_name, tick_key);
            let mut conn = self.pool.get().await.map_err(|error| {
                AutumnError::service_unavailable_msg(format!(
                    "scheduler sqlite lease connection unavailable: {error}"
                ))
            })?;
            self.ensure_table(&mut conn).await?;

            let now_ms = self.now_ms();
            let ttl_ms = i64::try_from(self.lease_ttl.as_millis()).unwrap_or(i64::MAX);
            let expires_at = now_ms.saturating_add(ttl_ms);

            // Reap expired leases first, so the insert below is the whole
            // acquire: a live lease keeps its row and blocks the insert, while a
            // lease whose holder died is already gone.
            diesel::sql_query("DELETE FROM autumn_scheduler_leases WHERE expires_at <= ?")
                .bind::<diesel::sql_types::BigInt, _>(now_ms)
                .execute(&mut *conn)
                .await
                .map_err(|error| {
                    AutumnError::internal_server_error_msg(format!(
                        "sqlite scheduler lease reap failed: {error}"
                    ))
                })?;

            let owner = next_lease_owner(&self.replica_id);
            let inserted = diesel::sql_query(
                "INSERT INTO autumn_scheduler_leases \
                 (lock_key, task_name, tick_key, owner, acquired_at, expires_at) \
                 VALUES (?, ?, ?, ?, ?, ?) \
                 ON CONFLICT(lock_key) DO NOTHING",
            )
            .bind::<diesel::sql_types::BigInt, _>(key)
            .bind::<diesel::sql_types::Text, _>(task_name)
            .bind::<diesel::sql_types::Text, _>(tick_key)
            .bind::<diesel::sql_types::Text, _>(&owner)
            .bind::<diesel::sql_types::BigInt, _>(now_ms)
            .bind::<diesel::sql_types::BigInt, _>(expires_at)
            .execute(&mut *conn)
            .await
            .map_err(|error| {
                AutumnError::internal_server_error_msg(format!(
                    "sqlite scheduler lease acquire failed: {error}"
                ))
            })?;

            if inserted == 0 {
                return Ok(None);
            }
            Ok(Some(SchedulerLease::sqlite(
                self.replica_id.clone(),
                SqliteTableLease { key },
            )))
        })
    }
}

/// A held `SQLite` scheduler lease.
///
/// Release does **not** delete the row, and that is the point: the row is what
/// makes the tick claimed. Deleting it would let a second process whose timer
/// reaches the same tick a moment later insert the same key and run the tick a
/// second time — duplicating whatever the task does. So a released lease simply
/// stops being renewed, the tick stays reserved for the rest of
/// `scheduler.lease_ttl_secs`, and the next acquire reaps the row once it
/// expires. Set the TTL longer than the spread between the processes' timers.
///
/// This is stricter than the Postgres coordinator, whose `pg_advisory_unlock`
/// frees the key the moment the leader finishes.
#[cfg(feature = "sqlite")]
struct SqliteTableLease {
    key: i64,
}

#[cfg(feature = "sqlite")]
impl SqliteTableLease {
    #[allow(
        clippy::unused_async,
        reason = "matches the fallible async release the Postgres lease has, so \
                  `SchedulerLease::release` keeps one shape across backends"
    )]
    async fn release(self) -> AutumnResult<()> {
        tracing::debug!(
            lock_key = self.key,
            "sqlite scheduler tick released; the row keeps the tick reserved until it expires"
        );
        Ok(())
    }
}

/// Build the scheduler coordinator for the current application state.
///
/// # Errors
///
/// Returns [`AutumnError`] when a distributed backend is selected without the
/// required runtime dependency.
pub fn coordinator_from_config(
    config: &SchedulerConfig,
    state: &AppState,
) -> AutumnResult<Arc<dyn SchedulerCoordinator>> {
    let replica_id = config.resolved_replica_id();
    match config.backend {
        SchedulerBackend::InProcess => Ok(Arc::new(InProcessSchedulerCoordinator::new(replica_id))),
        SchedulerBackend::Sqlite => {
            #[cfg(feature = "sqlite")]
            {
                let pool = state.pool().cloned().ok_or_else(|| {
                    AutumnError::service_unavailable_msg(
                        "scheduler.backend = \"sqlite\" requires a configured database pool",
                    )
                })?;
                // The lease table coordinates processes only because they open
                // the same FILE. On an in-memory target each has its own table,
                // so every replica would win the same tick and run it — while
                // `is_fleet_distributed()` reports the opposite. Refuse rather
                // than promise coordination that cannot happen (issue #1907).
                if state
                    .extension::<crate::config::AutumnConfig>()
                    .and_then(|config| {
                        config
                            .database
                            .effective_primary_url()
                            .map(crate::config::is_in_memory_sqlite_target)
                    })
                    .unwrap_or(false)
                {
                    return Err(AutumnError::service_unavailable_msg(
                        "scheduler.backend = \"sqlite\" requires a FILE-backed database: an \
                         in-memory SQLite target is private to each process, so every replica \
                         would claim the same tick and run it. Point database.url at a \
                         sqlite:// file, or use scheduler.backend = \"in_process\"",
                    ));
                }
                Ok(Arc::new(SqliteLeaseSchedulerCoordinator::new(
                    pool,
                    replica_id,
                    config.key_prefix.clone(),
                    Duration::from_secs(config.lease_ttl_secs),
                    state.clock_arc(),
                )))
            }

            // The lease table lives in the app's SQLite file, so this backend
            // needs a build that has one. On a Postgres build, advisory locks
            // are the fleet-wide primitive.
            #[cfg(not(feature = "sqlite"))]
            {
                let _ = (state, replica_id);
                Err(AutumnError::service_unavailable_msg(
                    "scheduler.backend = \"sqlite\" requires a build of autumn-web compiled \
                     with --features sqlite; on Postgres use scheduler.backend = \"postgres\"",
                ))
            }
        }
        SchedulerBackend::Postgres => {
            #[cfg(all(feature = "db", not(feature = "sqlite")))]
            {
                let pool = state.pool().cloned().ok_or_else(|| {
                    AutumnError::service_unavailable_msg(
                        "scheduler.backend = \"postgres\" requires a configured database pool",
                    )
                })?;
                Ok(Arc::new(PostgresAdvisorySchedulerCoordinator::new(
                    pool,
                    replica_id,
                    config.key_prefix.clone(),
                )))
            }

            // The Postgres advisory-lock scheduler coordinator is Postgres-only
            // (it leases via `pg_advisory_lock`). Under the `sqlite` feature the
            // runtime pool is a SQLite pool with no such primitive, so refuse
            // rather than mis-type. SQLite runs one of the two single-host
            // coordinators instead: `in_process` (the default, one process) or
            // `sqlite` (a lease table shared by the processes on the host).
            #[cfg(all(feature = "db", feature = "sqlite"))]
            {
                let _ = (state, replica_id);
                Err(AutumnError::service_unavailable_msg(
                    "scheduler.backend = \"postgres\" requires the Postgres backend and is \
                     unsupported under the sqlite feature; use scheduler.backend = \"in_process\" \
                     (the default) or scheduler.backend = \"sqlite\" to coordinate several \
                     processes on the host",
                ))
            }

            #[cfg(not(feature = "db"))]
            {
                let _ = state;
                Err(AutumnError::service_unavailable_msg(
                    "scheduler.backend = \"postgres\" requires the autumn-web db feature",
                ))
            }
        }
    }
}

/// Derive the global tick key for a fixed-delay task and Unix elapsed time.
#[must_use]
pub fn fixed_delay_tick_key(task_name: &str, delay: Duration, unix_elapsed: Duration) -> String {
    let interval = delay.as_nanos().max(1);
    // `interval` is `.max(1)`, so the division is always defined; going through
    // `checked_div` states that rather than relying on the reader to spot it.
    let bucket = unix_elapsed.as_nanos().checked_div(interval).unwrap_or(0);
    format!("{task_name}:{bucket}")
}

/// Derive the global tick key for a cron task and a Unix timestamp.
#[must_use]
pub fn cron_tick_key(task_name: &str, unix_secs: u64) -> String {
    format!("{task_name}:{unix_secs}")
}

/// Compute a stable signed 64-bit advisory lock key for a task tick.
#[must_use]
pub fn advisory_lock_key(key_prefix: &str, task_name: &str, tick_key: &str) -> i64 {
    let mut hasher = Sha256::new();
    hasher.update(key_prefix.as_bytes());
    hasher.update(b"\0");
    hasher.update(task_name.as_bytes());
    hasher.update(b"\0");
    hasher.update(tick_key.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 8];
    #[allow(
        clippy::indexing_slicing,
        reason = "infallible: SHA-256 digest is always 32 bytes, so [..8] is in bounds"
    )]
    let head = &digest[..8];
    bytes.copy_from_slice(head);
    i64::from_be_bytes(bytes)
}

/// Current Unix timestamp in seconds, read from the real system clock.
///
/// # Deprecated
///
/// This reads wall time off the injected-clock seam, so a tick key derived from
/// it is not reproducible under a [`#[sim_test]`](crate::sim_test). Read the
/// app's clock instead:
///
/// ```rust,ignore
/// let secs = autumn_web::time::clock_unix_secs(state.clock());
/// ```
///
/// The framework's own scheduler already does exactly that; this function has
/// no remaining production caller inside autumn.
#[must_use]
#[deprecated(
    since = "0.7.0",
    note = "reads real wall time off the injected-clock seam; use \
            autumn_web::time::clock_unix_secs(state.clock()) instead (see #1797)"
)]
pub fn now_unix_secs() -> u64 {
    #[allow(deprecated, reason = "the deprecated shim delegates to its own pair")]
    now_unix_duration().as_secs()
}

/// Current elapsed time since the Unix epoch, read from the real system clock.
///
/// # Deprecated
///
/// See [`now_unix_secs`]. Use
/// [`crate::time::clock_unix_duration(state.clock())`](crate::time::clock_unix_duration).
#[must_use]
#[deprecated(
    since = "0.7.0",
    note = "reads real wall time off the injected-clock seam; use \
            autumn_web::time::clock_unix_duration(state.clock()) instead (see #1797)"
)]
pub fn now_unix_duration() -> Duration {
    #[allow(
        clippy::disallowed_methods,
        reason = "the body of the deprecated real-time shim itself; it exists only \
                  so an existing downstream caller keeps compiling while the \
                  deprecation steers it onto time::clock_unix_duration"
    )]
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
}

#[cfg(feature = "db")]
#[derive(diesel::QueryableByName)]
struct AdvisoryLockRow {
    #[diesel(sql_type = diesel::sql_types::Bool)]
    acquired: bool,
}

#[cfg(feature = "db")]
async fn try_pg_advisory_lock(
    conn: &mut diesel_async::pooled_connection::deadpool::Object<diesel_async::AsyncPgConnection>,
    key: i64,
) -> AutumnResult<bool> {
    use diesel_async::RunQueryDsl as _;

    let row = diesel::sql_query("SELECT pg_try_advisory_lock($1) AS acquired")
        .bind::<diesel::sql_types::BigInt, _>(key)
        .get_result::<AdvisoryLockRow>(&mut **conn)
        .await
        .map_err(|error| AutumnError::internal_server_error_msg(error.to_string()))?;
    Ok(row.acquired)
}

#[cfg(feature = "db")]
#[derive(diesel::QueryableByName)]
struct AdvisoryUnlockRow {
    #[diesel(sql_type = diesel::sql_types::Bool)]
    released: bool,
}

#[cfg(feature = "db")]
async fn unlock_pg_advisory_lock(
    conn: &mut diesel_async::pooled_connection::deadpool::Object<diesel_async::AsyncPgConnection>,
    key: i64,
) -> AutumnResult<bool> {
    use diesel_async::RunQueryDsl as _;

    let row = diesel::sql_query("SELECT pg_advisory_unlock($1) AS released")
        .bind::<diesel::sql_types::BigInt, _>(key)
        .get_result::<AdvisoryUnlockRow>(&mut **conn)
        .await
        .map_err(|error| AutumnError::internal_server_error_msg(error.to_string()))?;
    Ok(row.released)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cron_tick_key_uses_task_name_and_second() {
        assert_eq!(cron_tick_key("digest", 1_700_000_000), "digest:1700000000");
    }

    // Issue #1864: `is_fleet_distributed` is a default trait method derived
    // from `backend()`, so a plain in-process coordinator needs no override
    // to report `false`.
    #[test]
    fn in_process_coordinator_is_not_fleet_distributed() {
        let coordinator = InProcessSchedulerCoordinator::new("replica-1");
        assert_eq!(coordinator.backend(), "in_process");
        assert!(!coordinator.is_fleet_distributed());
    }

    // `PostgresAdvisorySchedulerCoordinator` (feature `db`) needs a live pool
    // to construct — covered by the testcontainer-backed
    // `tests/integration/scheduled_coordination.rs` suite instead. This
    // exercises the same default-method derivation (`backend() == "postgres"`)
    // via a minimal double, without a database.
    /// Issue #1907: `scheduler.backend = "sqlite"` is refused on a build with
    /// no SQLite backend, and the refusal names the Postgres alternative.
    #[cfg(not(feature = "sqlite"))]
    #[test]
    fn sqlite_scheduler_backend_is_refused_without_the_sqlite_feature() {
        let config = SchedulerConfig {
            backend: SchedulerBackend::Sqlite,
            ..SchedulerConfig::default()
        };
        let state = AppState::for_test();
        let message = match coordinator_from_config(&config, &state) {
            Ok(_) => panic!("the sqlite coordinator needs a build with the sqlite feature"),
            Err(error) => error.to_string(),
        };
        assert!(
            message.contains("--features sqlite"),
            "the refusal names the missing feature; got: {message}"
        );
        assert!(
            message.contains("scheduler.backend = \"postgres\""),
            "the refusal names the Postgres alternative; got: {message}"
        );
    }

    #[test]
    fn a_coordinator_reporting_the_postgres_backend_string_is_fleet_distributed() {
        struct FakePostgresCoordinator;
        impl SchedulerCoordinator for FakePostgresCoordinator {
            fn backend(&self) -> &'static str {
                "postgres"
            }
            fn replica_id(&self) -> &'static str {
                "replica-1"
            }
            fn try_acquire<'a>(
                &'a self,
                _task_name: &'a str,
                _tick_key: &'a str,
                _coordination: TaskCoordination,
            ) -> SchedulerFuture<'a, AutumnResult<Option<SchedulerLease>>> {
                Box::pin(async { Ok(None) })
            }
        }
        assert!(FakePostgresCoordinator.is_fleet_distributed());
    }
}
