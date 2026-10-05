//! Scheduled-task coordination backends.
//!
//! The in-process backend preserves the original single-process behavior.
//! The Postgres backend records each fleet-wide task tick in a table, so at
//! most one replica runs it (issue #3052).

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

    /// Like [`Self::try_acquire`], but keep the tick claimed for at least
    /// `period` (issue #3052).
    ///
    /// A fixed-delay task gives its delay. Each replica starts its timer at
    /// its own boot, so two replicas can reach one tick up to one period
    /// apart. The default ignores `period`.
    fn try_acquire_for_period<'a>(
        &'a self,
        task_name: &'a str,
        tick_key: &'a str,
        coordination: TaskCoordination,
        period: Duration,
    ) -> SchedulerFuture<'a, AutumnResult<Option<SchedulerLease>>> {
        let _ = period;
        self.try_acquire(task_name, tick_key, coordination)
    }
}

/// Acquired permission to run a scheduled task tick.
pub struct SchedulerLease {
    backend: String,
    leader_id: String,
    fencing_token: Option<i64>,
    #[cfg(test)]
    release_count: Option<Arc<std::sync::atomic::AtomicUsize>>,
    #[cfg(feature = "db")]
    postgres: Option<PostgresTickRow>,
    #[cfg(feature = "sqlite")]
    sqlite: Option<SqliteTableLease>,
}

impl SchedulerLease {
    pub(crate) fn local(backend: impl Into<String>, leader_id: impl Into<String>) -> Self {
        Self {
            backend: backend.into(),
            leader_id: leader_id.into(),
            fencing_token: None,
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
            release_count: Some(release_count),
            ..Self::local(backend, leader_id)
        }
    }

    /// Set the fencing token. Test doubles use it.
    #[cfg(test)]
    pub(crate) const fn with_fencing_token(mut self, token: i64) -> Self {
        self.fencing_token = Some(token);
        self
    }

    /// A lease granted by the Postgres tick-table coordinator.
    #[cfg(feature = "db")]
    fn postgres(leader_id: impl Into<String>, row: PostgresTickRow) -> Self {
        Self {
            fencing_token: Some(row.generation),
            postgres: Some(row),
            ..Self::local("postgres", leader_id)
        }
    }

    /// A lease granted by the `SQLite` lease-table coordinator.
    #[cfg(feature = "sqlite")]
    fn sqlite(leader_id: impl Into<String>, lease: SqliteTableLease) -> Self {
        Self {
            sqlite: Some(lease),
            ..Self::local("sqlite", leader_id)
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

    /// Fencing token of this tick: the tick row's `generation`.
    ///
    /// It increases with each claim. `None` on the `in_process` and `sqlite`
    /// backends, and for `per_replica` tasks.
    #[must_use]
    pub const fn fencing_token(&self) -> Option<i64> {
        self.fencing_token
    }

    /// Release backend resources associated with this lease.
    ///
    /// The Postgres and `SQLite` tick rows stay, so the tick stays claimed.
    ///
    /// # Errors
    ///
    /// Returns [`AutumnError`] when the backend cannot release its lock.
    #[allow(
        clippy::unused_async,
        reason = "public async API; only the sqlite lease awaits in its body"
    )]
    pub async fn release(self) -> AutumnResult<()> {
        #[cfg(test)]
        if let Some(release_count) = self.release_count {
            release_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }

        #[cfg(feature = "sqlite")]
        if let Some(lease) = self.sqlite {
            return lease.release().await;
        }

        Ok(())
    }

    /// Release the lease and free its key at once. The next `try_acquire` on
    /// the same key then wins.
    ///
    /// Use it only when a constant key works as a mutex, as ACME issuance
    /// does. A schedule tick uses [`Self::release`]: a freed tick can run
    /// again. Only this lease's own row is deleted, so a row that expired and
    /// went to another replica stays.
    ///
    /// # Errors
    ///
    /// Returns [`AutumnError`] when the backend cannot delete the row.
    pub async fn release_and_free(mut self) -> AutumnResult<()> {
        #[cfg(feature = "db")]
        if let Some(row) = self.postgres.take() {
            row.free().await?;
        }
        #[cfg(feature = "sqlite")]
        if let Some(lease) = self.sqlite.as_ref() {
            lease.free().await?;
        }
        self.release().await
    }
}

/// The tick that the running scheduled task runs for.
///
/// Read it with [`current_tick`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduledTick {
    tick_key: String,
    fencing_token: Option<i64>,
}

impl ScheduledTick {
    pub(crate) fn new(tick_key: impl Into<String>, lease: &SchedulerLease) -> Self {
        Self {
            tick_key: tick_key.into(),
            fencing_token: lease.fencing_token(),
        }
    }

    /// Global tick key, for example `nightly-invoice:1700000000`.
    #[must_use]
    pub fn tick_key(&self) -> &str {
        &self.tick_key
    }

    /// Fencing token of this tick. See [`SchedulerLease::fencing_token`].
    ///
    /// Write it with each side effect, and refuse a write that carries an
    /// older token.
    #[must_use]
    pub const fn fencing_token(&self) -> Option<i64> {
        self.fencing_token
    }
}

tokio::task_local! {
    static CURRENT_TICK: ScheduledTick;
}

/// The tick that the calling `#[scheduled]` task runs for.
///
/// `None` outside a scheduled task. A task from `tokio::spawn` does not get
/// this value.
#[must_use]
pub fn current_tick() -> Option<ScheduledTick> {
    CURRENT_TICK.try_with(Clone::clone).ok()
}

/// Run `future` with `tick` as its [`current_tick`].
pub(crate) async fn with_tick<F: Future>(tick: ScheduledTick, future: F) -> F::Output {
    CURRENT_TICK.scope(tick, future).await
}

/// Why an `in_process` scheduler can run each fleet tick on more than one
/// replica (issue #3052).
///
/// `None` when the backend is not `in_process`, or when no hint shows more
/// than one replica. The hints:
///
/// - `jobs_backend` is `postgres` or `redis`. A shared queue shows a fleet.
/// - `replicas` (the `AUTUMN_REPLICAS` value) is a number above 1.
/// - `kubernetes` (`KUBERNETES_SERVICE_HOST` is set).
#[must_use]
pub(crate) fn in_process_fleet_hint(
    backend: SchedulerBackend,
    jobs_backend: &str,
    replicas: Option<&str>,
    kubernetes: bool,
) -> Option<&'static str> {
    if backend != SchedulerBackend::InProcess {
        return None;
    }
    if matches!(jobs_backend, "postgres" | "redis") {
        return Some("jobs.backend is a shared queue");
    }
    if replicas
        .and_then(|value| value.trim().parse::<u64>().ok())
        .is_some_and(|count| count > 1)
    {
        return Some("AUTUMN_REPLICAS is more than 1");
    }
    if kubernetes {
        return Some("KUBERNETES_SERVICE_HOST is set");
    }
    None
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

/// Postgres tick-table coordinator (issue #3052).
///
/// Each fleet tick is one row in `autumn_scheduler_ticks`. The replica whose
/// `INSERT … ON CONFLICT DO NOTHING` adds the row runs the tick. The row stays
/// after the run, so a replica whose timer reaches the same tick later does
/// not run it again. A leader that crashes does not free its tick: the policy
/// is at-most-once per tick.
///
/// The claim keeps no session state and holds no connection while the tick
/// runs. Thus, it works behind a transaction-mode `PgBouncer`. The fencing
/// token is the row's `generation`. See [`SchedulerLease::fencing_token`].
#[cfg(feature = "db")]
#[derive(Clone)]
pub struct PostgresTickSchedulerCoordinator {
    pool: diesel_async::pooled_connection::deadpool::Pool<diesel_async::AsyncPgConnection>,
    replica_id: String,
    key_prefix: String,
    retention: Duration,
    ready: Arc<tokio::sync::OnceCell<()>>,
}

/// Old name of [`PostgresTickSchedulerCoordinator`].
#[cfg(feature = "db")]
#[deprecated(
    since = "0.9.0",
    note = "renamed to PostgresTickSchedulerCoordinator: it uses a tick table, not \
            advisory locks (#3052)"
)]
pub type PostgresAdvisorySchedulerCoordinator = PostgresTickSchedulerCoordinator;

/// How long a tick row stays when no retention is set. Matches the default
/// `scheduler.lease_ttl_secs`.
#[cfg(feature = "db")]
const DEFAULT_TICK_RETENTION: Duration = Duration::from_secs(300);

/// Floor on the tick-row retention.
#[cfg(feature = "db")]
const MIN_TICK_RETENTION: Duration = Duration::from_secs(1);

/// Cap on how long a tick row stays: 100 years. A longer interval is out of
/// range for Postgres, and every claim would fail.
#[cfg(feature = "db")]
const MAX_TICK_HOLD: Duration = Duration::from_secs(100 * 365 * 24 * 60 * 60);

/// How long a claimed tick stays claimed: the retention plus the period.
///
/// Add, not take the larger: a replica whose clock is behind can still reach
/// the tick after one full period. The retention is the margin for that skew.
#[cfg(feature = "db")]
fn tick_hold(retention: Duration, period: Duration) -> Duration {
    retention.saturating_add(period).min(MAX_TICK_HOLD)
}

/// A claimed Postgres tick row, kept so that
/// [`SchedulerLease::release_and_free`] can delete it.
#[cfg(feature = "db")]
struct PostgresTickRow {
    pool: diesel_async::pooled_connection::deadpool::Pool<diesel_async::AsyncPgConnection>,
    key_prefix: String,
    task_name: String,
    tick_key: String,
    generation: i64,
}

#[cfg(feature = "db")]
impl PostgresTickRow {
    /// Delete this row. The `generation` match keeps a row that another
    /// replica claimed after this one expired.
    async fn free(self) -> AutumnResult<()> {
        use diesel_async::RunQueryDsl as _;

        let mut conn = self.pool.get().await.map_err(|error| {
            AutumnError::service_unavailable_msg(format!(
                "scheduler postgres connection unavailable: {error}"
            ))
        })?;
        diesel::sql_query(
            "DELETE FROM autumn_scheduler_ticks \
             WHERE key_prefix = $1 AND task_name = $2 AND tick_key = $3 AND generation = $4",
        )
        .bind::<diesel::sql_types::Text, _>(&self.key_prefix)
        .bind::<diesel::sql_types::Text, _>(&self.task_name)
        .bind::<diesel::sql_types::Text, _>(&self.tick_key)
        .bind::<diesel::sql_types::BigInt, _>(self.generation)
        .execute(&mut *conn)
        .await
        .map_err(|error| {
            AutumnError::internal_server_error_msg(format!(
                "postgres scheduler tick free failed: {error}"
            ))
        })?;
        Ok(())
    }
}

/// DDL of the Postgres tick table.
///
/// If the app's database role cannot run `CREATE TABLE`, apply this before
/// the deploy. The runtime then finds the table and sends no DDL.
#[cfg(feature = "db")]
pub const PG_TICK_TABLE_DDL: &str = "\
CREATE TABLE IF NOT EXISTS autumn_scheduler_ticks (
    key_prefix TEXT        NOT NULL,
    task_name  TEXT        NOT NULL,
    tick_key   TEXT        NOT NULL,
    owner      TEXT        NOT NULL,
    generation BIGSERIAL   NOT NULL,
    claimed_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (key_prefix, task_name, tick_key)
);
CREATE INDEX IF NOT EXISTS idx_autumn_scheduler_ticks_expires_at
    ON autumn_scheduler_ticks (expires_at);";

/// Advisory key that serializes the runtime `CREATE TABLE` across replicas.
#[cfg(feature = "db")]
pub(crate) const PG_TICK_DDL_LOCK_KEY: i64 = 3_052_305_230_523_052;

#[cfg(feature = "db")]
impl PostgresTickSchedulerCoordinator {
    /// Create a Postgres tick-table coordinator.
    ///
    /// Tick rows stay for 300 seconds. Use [`Self::with_tick_retention`] to
    /// change that.
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
            retention: DEFAULT_TICK_RETENTION,
            ready: Arc::new(tokio::sync::OnceCell::new()),
        }
    }

    /// Set how long a tick row stays after its claim. The minimum is 1 second.
    ///
    /// A fixed-delay tick stays for at least its delay. A replica whose timer
    /// reaches a tick after its row is gone runs the tick again. Set this
    /// longer than the spread between replica timers.
    #[must_use]
    pub fn with_tick_retention(mut self, retention: Duration) -> Self {
        self.retention = retention.clamp(MIN_TICK_RETENTION, MAX_TICK_HOLD);
        self
    }

    /// Create the tick table on first use. A failed attempt leaves the cell
    /// empty, so the next `try_acquire` call tries again.
    async fn ensure_table(&self, conn: &mut diesel_async::AsyncPgConnection) -> AutumnResult<()> {
        self.ready
            .get_or_try_init(|| create_tick_table(conn))
            .await?;
        Ok(())
    }
}

#[cfg(feature = "db")]
#[derive(diesel::QueryableByName)]
struct TableExistsRow {
    #[diesel(sql_type = diesel::sql_types::Bool)]
    present: bool,
}

/// Create the tick table if it is missing.
///
/// The check comes first, so a role without the `CREATE` privilege can use a
/// table that an operator made. Postgres checks privileges before it reads
/// `IF NOT EXISTS`.
///
/// The DDL goes as one simple query, which Postgres runs as one implicit
/// transaction. The advisory lock holds until the last statement ends. Thus,
/// two replicas that start together do not both run `CREATE TABLE`.
#[cfg(feature = "db")]
async fn create_tick_table(conn: &mut diesel_async::AsyncPgConnection) -> AutumnResult<()> {
    use diesel_async::{RunQueryDsl as _, SimpleAsyncConnection as _};

    let setup_error = |error: diesel::result::Error| {
        AutumnError::internal_server_error_msg(format!(
            "postgres scheduler tick table setup failed: {error}"
        ))
    };
    let exists =
        diesel::sql_query("SELECT to_regclass('autumn_scheduler_ticks') IS NOT NULL AS present")
            .get_result::<TableExistsRow>(&mut *conn)
            .await
            .map_err(setup_error)?;
    if exists.present {
        return Ok(());
    }
    conn.batch_execute(&format!(
        "SELECT pg_advisory_xact_lock({PG_TICK_DDL_LOCK_KEY});\n{PG_TICK_TABLE_DDL}"
    ))
    .await
    .map_err(setup_error)
}

#[cfg(feature = "db")]
#[derive(diesel::QueryableByName)]
struct TickGenerationRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    generation: i64,
}

#[cfg(feature = "db")]
impl SchedulerCoordinator for PostgresTickSchedulerCoordinator {
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
        self.try_acquire_for_period(task_name, tick_key, coordination, Duration::ZERO)
    }

    fn try_acquire_for_period<'a>(
        &'a self,
        task_name: &'a str,
        tick_key: &'a str,
        coordination: TaskCoordination,
        period: Duration,
    ) -> SchedulerFuture<'a, AutumnResult<Option<SchedulerLease>>> {
        Box::pin(async move {
            use diesel_async::RunQueryDsl as _;

            if coordination == TaskCoordination::PerReplica {
                return Ok(Some(SchedulerLease::local(
                    "per_replica",
                    self.replica_id.clone(),
                )));
            }

            let mut conn = self.pool.get().await.map_err(|error| {
                AutumnError::service_unavailable_msg(format!(
                    "scheduler postgres connection unavailable: {error}"
                ))
            })?;
            self.ensure_table(&mut conn).await?;

            // Prune first. Each row carries its own expiry, set from the
            // database clock, so a shorter retention in another app sharing
            // the table cannot free our ticks.
            diesel::sql_query("DELETE FROM autumn_scheduler_ticks WHERE expires_at < now()")
                .execute(&mut *conn)
                .await
                .map_err(|error| {
                    AutumnError::internal_server_error_msg(format!(
                        "postgres scheduler tick prune failed: {error}"
                    ))
                })?;

            let claimed = diesel::sql_query(
                "INSERT INTO autumn_scheduler_ticks \
                 (key_prefix, task_name, tick_key, owner, expires_at) \
                 VALUES ($1, $2, $3, $4, now() + make_interval(secs => $5)) \
                 ON CONFLICT DO NOTHING \
                 RETURNING generation",
            )
            .bind::<diesel::sql_types::Text, _>(&self.key_prefix)
            .bind::<diesel::sql_types::Text, _>(task_name)
            .bind::<diesel::sql_types::Text, _>(tick_key)
            .bind::<diesel::sql_types::Text, _>(&self.replica_id)
            .bind::<diesel::sql_types::Double, _>(tick_hold(self.retention, period).as_secs_f64())
            .load::<TickGenerationRow>(&mut *conn)
            .await
            .map_err(|error| {
                AutumnError::internal_server_error_msg(format!(
                    "postgres scheduler tick claim failed: {error}"
                ))
            })?;

            Ok(claimed.into_iter().next().map(|row| {
                SchedulerLease::postgres(
                    self.replica_id.clone(),
                    PostgresTickRow {
                        pool: self.pool.clone(),
                        key_prefix: self.key_prefix.clone(),
                        task_name: task_name.to_owned(),
                        tick_key: tick_key.to_owned(),
                        generation: row.generation,
                    },
                )
            }))
        })
    }
}

/// Floor on the effective lease TTL of [`SqliteLeaseSchedulerCoordinator`].
///
/// A zero (or sub-millisecond) TTL writes `expires_at == now_ms`, which the
/// reap-first predicate in `try_acquire` (`expires_at <= now_ms`) treats as
/// already dead — so a second coordinator takes the identical tick while the
/// first task is still running. Clamp rather than reject: the constructor
/// takes a plain [`Duration`] a caller may reach through a computed value.
#[cfg(feature = "sqlite")]
const MIN_SCHEDULER_LEASE_TTL: Duration = Duration::from_secs(1);

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
    ///
    /// `lease_ttl` is clamped to at least [`MIN_SCHEDULER_LEASE_TTL`]: a
    /// shorter lease would expire the instant it is written, and the
    /// reap-first predicate in `try_acquire` would hand the identical tick to
    /// a second coordinator while the first task is still running.
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
            lease_ttl: lease_ttl.max(MIN_SCHEDULER_LEASE_TTL),
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
        self.try_acquire_for_period(task_name, tick_key, coordination, Duration::ZERO)
    }

    fn try_acquire_for_period<'a>(
        &'a self,
        task_name: &'a str,
        tick_key: &'a str,
        coordination: TaskCoordination,
        period: Duration,
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
            // A fixed-delay tick stays for its delay plus the TTL (issue #3052).
            let hold = tick_hold(self.lease_ttl, period);
            let ttl_ms = i64::try_from(hold.as_millis()).unwrap_or(i64::MAX);
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
                SqliteTableLease {
                    key,
                    owner,
                    pool: self.pool.clone(),
                },
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
/// The Postgres coordinator keeps its tick row in the same way (issue #3052).
#[cfg(feature = "sqlite")]
struct SqliteTableLease {
    key: i64,
    owner: String,
    pool: diesel_async::pooled_connection::deadpool::Pool<crate::db::RuntimeConnection>,
}

#[cfg(feature = "sqlite")]
impl SqliteTableLease {
    /// Delete this lease's row. The `owner` match keeps a row that another
    /// process claimed after this one expired.
    async fn free(&self) -> AutumnResult<()> {
        use diesel_async::RunQueryDsl as _;

        let mut conn = self.pool.get().await.map_err(|error| {
            AutumnError::service_unavailable_msg(format!(
                "scheduler sqlite lease connection unavailable: {error}"
            ))
        })?;
        diesel::sql_query("DELETE FROM autumn_scheduler_leases WHERE lock_key = ? AND owner = ?")
            .bind::<diesel::sql_types::BigInt, _>(self.key)
            .bind::<diesel::sql_types::Text, _>(&self.owner)
            .execute(&mut *conn)
            .await
            .map_err(|error| {
                AutumnError::internal_server_error_msg(format!(
                    "sqlite scheduler lease free failed: {error}"
                ))
            })?;
        Ok(())
    }

    #[allow(
        clippy::unused_async,
        reason = "`SchedulerLease::release` is async; this keeps one shape across backends"
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
            // needs a build that has one. On a Postgres build, the Postgres
            // tick table is the fleet-wide primitive.
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
                Ok(Arc::new(
                    PostgresTickSchedulerCoordinator::new(
                        pool,
                        replica_id,
                        config.key_prefix.clone(),
                    )
                    .with_tick_retention(Duration::from_secs(config.lease_ttl_secs)),
                ))
            }

            // The Postgres tick-table scheduler coordinator is Postgres-only
            // (its SQL is Postgres SQL). Under the `sqlite` feature the
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

/// Compute a stable signed 64-bit key for a task tick. The `SQLite` lease
/// table uses it as its primary key.
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

    // `PostgresTickSchedulerCoordinator` (feature `db`) needs a live pool
    // to construct — covered by the testcontainer-backed
    // `tests/integration/scheduled_coordination_pg.rs` suite instead. This
    // exercises the same default-method derivation (`backend() == "postgres"`)
    // via a minimal double, without a database.
    /// Issue #1907: `scheduler.backend = "sqlite"` is refused on a build with
    /// no `SQLite` backend, and the refusal names the Postgres alternative.
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

    // Issue #2585 item 2: `SqliteLeaseSchedulerCoordinator::new` takes the
    // `Duration` unchecked, and `try_acquire` computes `ttl_ms` from
    // `as_millis()` — so `Duration::ZERO` (or anything sub-millisecond)
    // wrote `expires_at == now_ms`, which the reap-first predicate
    // (`expires_at <= now_ms`) treats as already dead. A second coordinator
    // then took the identical tick while the first task was still running.
    // The constructor clamps to the same 1s floor `lock.rs` uses.
    #[cfg(feature = "sqlite")]
    fn sqlite_test_coordinator(ttl: Duration) -> SqliteLeaseSchedulerCoordinator {
        let manager = diesel_async::pooled_connection::AsyncDieselConnectionManager::<
            crate::db::RuntimeConnection,
        >::new(":memory:");
        let pool = diesel_async::pooled_connection::deadpool::Pool::builder(manager)
            .max_size(1)
            .runtime(deadpool::Runtime::Tokio1)
            .build()
            .expect("test pool builds without connecting");
        SqliteLeaseSchedulerCoordinator::new(
            pool,
            "replica-1",
            "test-prefix",
            ttl,
            std::sync::Arc::new(crate::time::SystemClock),
        )
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn scheduler_coordinator_clamps_a_sub_second_lease_ttl() {
        assert_eq!(
            sqlite_test_coordinator(Duration::ZERO).lease_ttl,
            Duration::from_secs(1),
            "a zero TTL would expire the instant it is written"
        );
        assert_eq!(
            sqlite_test_coordinator(Duration::from_micros(999)).lease_ttl,
            Duration::from_secs(1),
            "a sub-millisecond TTL leaves ttl_ms == 0"
        );
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn scheduler_coordinator_keeps_a_sane_lease_ttl() {
        assert_eq!(
            sqlite_test_coordinator(Duration::from_secs(30)).lease_ttl,
            Duration::from_secs(30)
        );
        assert_eq!(
            sqlite_test_coordinator(Duration::from_secs(1)).lease_ttl,
            Duration::from_secs(1),
            "exactly the floor stays the floor"
        );
    }

    // Issue #3052: hints that an `in_process` scheduler runs on a fleet.
    #[test]
    fn in_process_fleet_hint_flags_a_shared_jobs_backend() {
        for jobs in ["postgres", "redis"] {
            assert!(
                in_process_fleet_hint(SchedulerBackend::InProcess, jobs, None, false).is_some(),
                "{jobs}"
            );
        }
    }

    #[test]
    fn in_process_fleet_hint_flags_replica_and_kubernetes_hints() {
        let hint = |replicas, kubernetes| {
            in_process_fleet_hint(SchedulerBackend::InProcess, "local", replicas, kubernetes)
        };
        assert!(hint(Some("3"), false).is_some());
        assert!(hint(Some(" 2 "), false).is_some());
        assert!(hint(None, true).is_some());
        assert!(hint(Some("1"), false).is_none(), "one replica is safe");
        assert!(hint(Some("many"), false).is_none(), "not a number");
        assert!(hint(None, false).is_none(), "no hint");
    }

    #[test]
    fn in_process_fleet_hint_is_silent_on_a_coordinated_backend() {
        for backend in [SchedulerBackend::Postgres, SchedulerBackend::Sqlite] {
            assert!(in_process_fleet_hint(backend, "postgres", Some("9"), true).is_none());
        }
        assert!(
            in_process_fleet_hint(SchedulerBackend::InProcess, "sqlite", None, false).is_none(),
            "a sqlite queue is single-host"
        );
    }

    #[tokio::test]
    async fn current_tick_is_set_only_inside_the_tick_scope() {
        assert_eq!(current_tick(), None, "no tick outside a scheduled task");

        let lease = SchedulerLease::local("postgres", "replica-a").with_fencing_token(42);
        let tick = ScheduledTick::new("invoice:7", &lease);
        let seen = with_tick(tick.clone(), async {
            let inner = tokio::spawn(async { current_tick() }).await.expect("join");
            (current_tick(), inner)
        })
        .await;

        assert_eq!(seen.0, Some(tick));
        assert_eq!(
            seen.0.as_ref().map(ScheduledTick::tick_key),
            Some("invoice:7")
        );
        assert_eq!(seen.0.and_then(|tick| tick.fencing_token()), Some(42));
        assert_eq!(seen.1, None, "a spawned task does not inherit the tick");
        assert_eq!(current_tick(), None);
    }

    // Issue #3052: the hold adds the period to the retention, so a replica
    // whose clock is behind still finds the tick claimed. It is capped, so a
    // huge `lease_ttl_secs` cannot overflow the Postgres interval.
    #[cfg(feature = "db")]
    #[test]
    fn tick_hold_adds_the_period_and_is_capped() {
        let minute = Duration::from_secs(60);
        assert_eq!(tick_hold(minute, Duration::ZERO), minute);
        assert_eq!(tick_hold(minute, 10 * minute), 11 * minute);
        assert_eq!(tick_hold(Duration::MAX, minute), MAX_TICK_HOLD);
        assert_eq!(tick_hold(minute, Duration::MAX), MAX_TICK_HOLD);
    }

    #[test]
    fn local_lease_has_no_fencing_token() {
        assert_eq!(
            SchedulerLease::local("in_process", "replica-a").fencing_token(),
            None
        );
    }
}
