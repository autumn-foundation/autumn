//! Scheduled-task coordination backends.
//!
//! The in-process backend preserves the original single-process behavior.
//! The Postgres backend uses advisory locks so each fleet-wide task tick is
//! claimed by at most one replica under normal operation.

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
            // rather than mis-type. Single-node SQLite runs the in-process
            // coordinator (`scheduler.backend = "in_process"`, the default).
            #[cfg(all(feature = "db", feature = "sqlite"))]
            {
                let _ = (state, replica_id);
                Err(AutumnError::service_unavailable_msg(
                    "scheduler.backend = \"postgres\" requires the Postgres backend and is \
                     unsupported under the sqlite feature; use scheduler.backend = \"in_process\"",
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

/// Current Unix timestamp in seconds.
#[must_use]
pub fn now_unix_secs() -> u64 {
    now_unix_duration().as_secs()
}

/// Current elapsed time since the Unix epoch.
#[must_use]
pub fn now_unix_duration() -> Duration {
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
}
