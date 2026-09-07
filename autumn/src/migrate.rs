//! Database migration support.
//!
//! Provides helpers for running Diesel migrations at application startup.
//! In **dev** mode, pending migrations run automatically; in **prod** mode,
//! they must be applied explicitly via `autumn migrate`.
//!
//! # Usage
//!
//! Application code typically does not use this module directly. Instead,
//! pass embedded migrations to [`AppBuilder::migrations`](crate::app::AppBuilder::migrations)
//! and the framework handles the rest:
//!
//! ```rust,ignore
//! use diesel_migrations::{EmbeddedMigrations, embed_migrations};
//!
//! const MIGRATIONS: EmbeddedMigrations = embed_migrations!();
//!
//! #[autumn_web::main]
//! async fn main() {
//!     autumn_web::app()
//!         .routes(routes![...])
//!         .migrations(MIGRATIONS)
//!         .run()
//!         .await;
//! }
//! ```

use diesel::RunQueryDsl;
use diesel::migration::{Migration, MigrationSource};
use diesel::pg::Pg;
use diesel_migrations::{FileBasedMigrations, HarnessWithOutput, MigrationHarness};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::Path;

/// Re-export `EmbeddedMigrations` so users can reference it without adding
/// `diesel_migrations` as a direct dependency.
pub use diesel_migrations::EmbeddedMigrations;

/// Re-export the `embed_migrations!` macro.
pub use diesel_migrations::embed_migrations;

/// Embedded Autumn framework migrations.
///
/// These are applied by `autumn migrate` and are also registered
/// automatically at startup when a framework feature requires its own table.
pub const FRAMEWORK_MIGRATIONS: EmbeddedMigrations = embed_migrations!("migrations");

/// Result of running pending migrations.
#[derive(Debug)]
pub struct MigrationResult {
    /// Names of the migrations that were applied.
    pub applied: Vec<String>,
}

/// Error type for migration operations.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MigrationError {
    /// Failed to connect to the database.
    #[error("failed to connect to database: {0}")]
    Connection(String),

    /// A migration failed to apply.
    #[error("migration failed: {0}")]
    Migration(String),

    /// The migration advisory lock could not be acquired within the timeout.
    ///
    /// Another process is likely running migrations. Increase `wait_timeout`
    /// or investigate the blocking session in `pg_locks`.
    #[error(
        "migration advisory lock not acquired within {timeout_secs}s; \
         another process may still be running migrations"
    )]
    LockTimeout {
        /// Configured wait timeout in seconds.
        timeout_secs: u64,
    },
}

/// Run `$body` with a `&mut` connection to `$url` that honors the
/// connection string's `sslmode`
/// (see [`crate::db::establish_migration_connection`]): TLS-off strings keep
/// the historical native `PgConnection`, TLS-requiring ones connect through
/// the pool's rustls connector — the bundled libpq has no SSL support, so
/// the native path cannot reach TLS-only servers at all (issue #1585
/// review). The body is expanded once per concrete connection type, so it
/// may only use the sync diesel APIs both provide (queries, transactions,
/// `MigrationHarness`).
///
/// The connect AND `$body` both run on a freshly spawned
/// [`std::thread::scope`] thread, never the calling one. This is load-bearing,
/// not an optimization: the rustls arm's connection bridges every sync
/// diesel call through its own internal `block_on`
/// (see [`crate::db::establish_migration_connection`]'s doc comment), which
/// panics ("Cannot start a runtime from within a runtime") if run from a
/// thread that is itself already inside some ambient runtime's context —
/// exactly what happens when an app calls a migration function directly from
/// its own async `.on_startup(|state| async move { ... })` hook rather than
/// from `spawn_blocking`. A freshly spawned thread has never entered any
/// runtime, so it is always safe regardless of the caller's own context.
/// `scope` (rather than a plain `'static` `std::thread::spawn`) lets `$body`
/// borrow from the enclosing function (e.g. an `Option<&HashMap<..>>`
/// parameter) without needing to be `'static`.
macro_rules! with_migration_connection {
    ($url:expr, |$conn:ident| $body:expr) => {
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    match crate::db::establish_migration_connection($url)
                        .map_err(|e| MigrationError::Connection(e.to_string()))?
                    {
                        crate::db::MigrationConnection::Native(mut native) => {
                            let $conn = &mut native;
                            $body
                        }
                        crate::db::MigrationConnection::Rustls { mut conn, runtime } => {
                            let result = {
                                let $conn = &mut conn;
                                $body
                            };
                            // The runtime drives the connection's tokio
                            // driver task: it must outlive every use of
                            // `conn`.
                            drop(conn);
                            drop(runtime);
                            result
                        }
                    }
                })
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
        })
    };
}

/// `PostgreSQL` advisory lock key used to serialize concurrent migration runs.
///
/// Derived from the big-endian encoding of the ASCII bytes `autn_mig` (`i64`).
/// The value is stable across framework versions so operators can monitor
/// contention without consulting source code.
///
/// Monitor contention with:
///
/// ```sql
/// SELECT pid, granted, mode
/// FROM pg_locks
/// WHERE locktype = 'advisory'
///   AND classid = 1635087470
///   AND objid   = 1601005927
///   AND objsubid = 1;
/// ```
pub const MIGRATION_ADVISORY_LOCK_KEY: i64 = 0x6175_746E_5F6D_6967_u64.cast_signed();

/// Default time to wait for the migration advisory lock before failing.
///
/// Override per call via the `wait_timeout` parameter of [`run_pending_locked`]
/// or [`hold_migration_lock`].
pub const DEFAULT_LOCK_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

#[derive(diesel::QueryableByName)]
struct AdvisoryLockRow {
    #[diesel(sql_type = diesel::sql_types::Bool)]
    acquired: bool,
}

#[derive(diesel::QueryableByName)]
struct AdvisoryUnlockRow {
    #[diesel(sql_type = diesel::sql_types::Bool)]
    released: bool,
}

#[derive(diesel::QueryableByName)]
struct AppliedMigrationVersion {
    #[diesel(sql_type = diesel::sql_types::Text)]
    version: String,
}

/// Runtime readiness state for a configured read replica's schema version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReplicaMigrationReadiness {
    /// Primary and replica report the same applied migration versions.
    Ready,
    /// The replica is reachable but has not applied the same migrations.
    Stale {
        primary_latest: Option<String>,
        replica_latest: Option<String>,
    },
    /// The framework could not determine replica migration state.
    Unknown(String),
}

impl ReplicaMigrationReadiness {
    /// Returns whether the replica can safely receive read traffic.
    #[must_use]
    pub(crate) const fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }

    /// Human-readable reason used in runtime readiness state.
    #[must_use]
    pub(crate) fn detail(&self) -> Option<String> {
        match self {
            Self::Ready => None,
            Self::Stale {
                primary_latest,
                replica_latest,
            } => Some(format!(
                "replica migrations lag primary (primary_latest={}, replica_latest={})",
                primary_latest.as_deref().unwrap_or("<none>"),
                replica_latest.as_deref().unwrap_or("<none>")
            )),
            Self::Unknown(error) => Some(format!("replica migration readiness unknown: {error}")),
        }
    }
}

/// Borrow an [`EmbeddedMigrations`] as a [`diesel::migration::MigrationSource`].
///
/// `EmbeddedMigrations` is neither `Copy` nor `Clone`, but multi-target
/// (control + shards) startup migration needs to apply the same embedded
/// set against several databases.
pub(crate) struct EmbeddedMigrationsRef<'a>(pub &'a EmbeddedMigrations);

impl<DB: diesel::backend::Backend> diesel::migration::MigrationSource<DB>
    for EmbeddedMigrationsRef<'_>
{
    fn migrations(
        &self,
    ) -> diesel::migration::Result<Vec<Box<dyn diesel::migration::Migration<DB>>>> {
        diesel::migration::MigrationSource::<DB>::migrations(self.0)
    }
}

/// Run all pending migrations against the given database URL.
///
/// Uses a **synchronous** connection (not the async pool) because Diesel
/// migrations require `MigrationHarness`, which is sync-only. The
/// connection honors the URL's `sslmode`: TLS-requiring strings connect
/// through the pool's rustls connector (the bundled libpq cannot), so
/// migrations work against TLS-only servers too.
///
/// Returns the list of migration versions that were applied, or an error
/// if a migration fails (including the failing SQL in the message).
///
/// # Errors
///
/// Returns [`MigrationError::Connection`] if the database is unreachable,
/// or [`MigrationError::Migration`] if a migration fails.
pub fn run_pending(
    database_url: &str,
    migrations: impl diesel::migration::MigrationSource<diesel::pg::Pg> + Send,
) -> Result<MigrationResult, MigrationError> {
    with_migration_connection!(database_url, |conn| {
        let mut harness = HarnessWithOutput::write_to_stdout(conn);

        let applied = harness
            .run_pending_migrations(migrations)
            .map_err(|e| MigrationError::Migration(e.to_string()))?;

        Ok(MigrationResult {
            applied: applied.iter().map(|m| format!("{m}")).collect(),
        })
    })
}

/// Return names of pending (not yet applied) migrations.
///
/// # Errors
///
/// Returns [`MigrationError::Connection`] if the database is unreachable,
/// or [`MigrationError::Migration`] if status cannot be determined.
pub fn pending_migrations(
    database_url: &str,
    migrations: impl diesel::migration::MigrationSource<diesel::pg::Pg> + Send,
) -> Result<Vec<String>, MigrationError> {
    with_migration_connection!(database_url, |conn| {
        let pending = conn
            .pending_migrations(migrations)
            .map_err(|e| MigrationError::Migration(e.to_string()))?;

        Ok(pending
            .iter()
            .map(|m| m.name().version().to_string())
            .collect())
    })
}

/// Actionable error surfaced when **any** in-memory `SQLite` target is
/// configured together with registered startup migrations (issue #1614
/// follow-up).
///
/// No in-memory database — private (`sqlite::memory:` / `:memory:` /
/// `file::memory:`) OR shared-cache (`file::memory:?cache=shared`) — can retain
/// a registered migration for the runtime pool. The migration runs on a
/// transient synchronous connection; a *private* in-memory database gives every
/// connection its own empty database, and a *shared* in-memory database is
/// destroyed the moment its last connection closes — and because the runtime
/// deadpool is created lazily (it may not have checked out a connection yet), the
/// pool's first checkout opens a fresh, empty database. Either way the migrated
/// schema is gone before the pool anchors it. The only remedy is a **file-backed**
/// database, which persists on disk across the migration connection closing.
#[cfg(feature = "sqlite")]
pub(crate) const IN_MEMORY_MIGRATION_MSG: &str = "In-memory SQLite (`:memory:` / `sqlite::memory:` / `file::memory:`, including \
     `cache=shared`) cannot be used with registered startup migrations \u{2014} the \
     schema is applied on a transient connection and is lost before the runtime \
     pool anchors it. Use a file-backed SQLite database.";

/// Return the [`IN_MEMORY_MIGRATION_MSG`] reject error when `database_url` is
/// **any** in-memory `SQLite` target (private OR shared-cache) AND `migrations`
/// carries at least one registered migration to apply; otherwise `None`.
///
/// This is the single decision point the `SQLite` migration-application paths
/// share (the boot [`auto_migrate_sqlite`], the pub [`run_pending_sqlite`], and
/// the `AUTUMN_MIGRATE=1` `apply_pending_sqlite_or_exit`), so they cannot drift.
/// It performs no I/O — the target is classified from the URL string
/// ([`crate::db::sqlite_target_is_any_in_memory`]) and the registered set is
/// enumerated in-memory — so it runs **before** any transient migration
/// connection is opened.
///
/// An empty migration set returns `None`: an in-memory target with no registered
/// migrations is a legitimate configuration (it is the default test harness), so
/// it is never rejected. A file-backed target likewise returns `None` — only it
/// retains the migrated schema for the pool.
#[cfg(feature = "sqlite")]
pub(crate) fn reject_in_memory_migrations<S>(
    database_url: &str,
    migrations: &S,
) -> Option<MigrationError>
where
    S: diesel::migration::MigrationSource<diesel::sqlite::Sqlite>,
{
    if !crate::db::sqlite_target_is_any_in_memory(database_url) {
        return None;
    }
    // Only reject when there is actually a migration to apply. If the set can't
    // be enumerated, treat it as non-empty (the apply would fail anyway) so the
    // doomed configuration is still surfaced rather than silently proceeding.
    let has_registered = migrations.migrations().map_or(true, |m| !m.is_empty());
    if !has_registered {
        return None;
    }
    Some(MigrationError::Migration(
        IN_MEMORY_MIGRATION_MSG.to_owned(),
    ))
}

/// Run all pending migrations against a `SQLite` database URL (issue #1614, PR3).
///
/// The `SQLite` counterpart to [`run_pending`]. Establishes a synchronous
/// `SqliteConnection` (via [`crate::db::establish_sqlite_migration_connection`])
/// and applies pending migrations through diesel's `MigrationHarness`, wrapping
/// the whole list→apply sequence in the shared [`with_sqlite_migration_lock`]
/// write lock (issue #2065, deferred from PR #2062) so two concurrent `autumn
/// migrate` / `autumn schema migrate` processes against the same file cannot
/// interleave their read-then-write windows and re-run an already-applied
/// migration. There is still **no Postgres advisory lock** on this path —
/// `SQLite` has no `pg_advisory_lock` primitive; the on-disk write lock held
/// across the sequence is the entire cross-process serialization mechanism.
///
/// The migration set is taken as a [`MigrationSource<Sqlite>`](diesel::migration::MigrationSource);
/// the framework's [`EmbeddedMigrations`] (and [`EmbeddedMigrationsRef`]) satisfy
/// this for the `SQLite` backend exactly as they do for Postgres, so a set
/// registered via [`AppBuilder::migrations`](crate::app::AppBuilder::migrations)
/// runs here unchanged.
///
/// # Errors
///
/// Returns [`MigrationError::Connection`] if the database cannot be opened, or
/// [`MigrationError::Migration`] if a migration fails.
#[cfg(feature = "sqlite")]
pub fn run_pending_sqlite(
    database_url: &str,
    migrations: impl diesel::migration::MigrationSource<diesel::sqlite::Sqlite>,
) -> Result<MigrationResult, MigrationError> {
    // Reject ANY in-memory target (private OR shared-cache) with registered
    // migrations before opening the (transient) migration connection: the
    // migrated schema is lost before the runtime pool anchors it — a private
    // in-memory connection is its own empty database, and a shared in-memory
    // database is destroyed when its last connection closes (issue #1614
    // follow-up). Only a file-backed target is unaffected.
    if let Some(err) = reject_in_memory_migrations(database_url, &migrations) {
        return Err(err);
    }
    let mut conn = crate::db::establish_sqlite_migration_connection(database_url)
        .map_err(|e| MigrationError::Connection(e.to_string()))?;
    let applied = with_sqlite_migration_lock(&mut conn, |conn| {
        let mut harness = HarnessWithOutput::write_to_stdout(conn);
        harness
            .run_pending_migrations(migrations)
            .map(|applied| applied.iter().map(|m| format!("{m}")).collect::<Vec<_>>())
            .map_err(|e| MigrationError::Migration(e.to_string()))
    })?;
    Ok(MigrationResult { applied })
}

/// Serialize a `SQLite` migration sequence under the database's write lock.
///
/// The single intra-run serialization primitive shared by BOTH `SQLite`
/// migration directions — [`run_pending_sqlite`] (up) and
/// [`revert_user_migrations_sqlite`] (down) — and therefore by both the `autumn
/// migrate` and `autumn schema migrate` verbs, which each route through those two
/// functions. It takes the `SQLite` write lock up front with `BEGIN IMMEDIATE`
/// and holds it across the whole list→plan→apply/revert sequence in `f`, then
/// commits. Two concurrent migrators against the same file therefore cannot
/// interleave their read-then-write windows: the second `BEGIN IMMEDIATE` queues
/// on the connection's `busy_timeout` (set by
/// [`crate::db::establish_sqlite_migration_connection`]) until the first commits,
/// then re-reads an already-drained pending/applied set and cleanly no-ops
/// instead of re-running an already-applied `up.sql` (or already-reverted
/// `down.sql`) and reporting a false failure (issue #2065, deferred from
/// PR #2062).
///
/// There is deliberately **no Postgres advisory lock** on the `SQLite` path
/// (issue #1999 / #2036 precedent) — `SQLite` has no `pg_advisory_lock`
/// primitive; this on-disk write lock is the whole serialization mechanism.
///
/// # Cooperation with diesel's per-migration transactions
///
/// The `BEGIN IMMEDIATE` is issued **through** diesel's `AnsiTransactionManager`
/// (`begin_transaction_sql`), so the manager's depth counter advances 0 → 1 (the
/// same technique as [`crate::db::scoped_immediate_transaction`]). Each migration
/// diesel then applies/reverts inside its own `self.transaction(...)` becomes a
/// nested `SAVEPOINT` (depth 1 → 2) rather than a raw nested `BEGIN`, which would
/// fail with "cannot start a transaction within a transaction".
///
/// On any error the outer transaction is rolled back, so a mid-sequence failure
/// leaves the applied set unchanged (the sequence is atomic) rather than
/// partially advanced. On the success path — the only path the concurrency fix
/// exercises, since a loser drains an already-empty set — the end state is
/// identical to the pre-lock harness.
///
/// # Errors
///
/// Returns [`MigrationError::Migration`] if the write lock cannot be taken or the
/// transaction cannot be committed, or the error returned by `f`.
#[cfg(feature = "sqlite")]
fn with_sqlite_migration_lock<T>(
    conn: &mut diesel::SqliteConnection,
    f: impl FnOnce(&mut diesel::SqliteConnection) -> Result<T, MigrationError>,
) -> Result<T, MigrationError> {
    use diesel::connection::{AnsiTransactionManager, TransactionManager};

    // Take the write lock up front THROUGH the transaction manager so its depth
    // counter is synced (0 → 1); diesel's per-migration `self.transaction(...)`
    // then nests as a SAVEPOINT instead of colliding with a raw nested BEGIN.
    AnsiTransactionManager::begin_transaction_sql(conn, "BEGIN IMMEDIATE")
        .map_err(|e| MigrationError::Migration(e.to_string()))?;

    match f(conn) {
        Ok(value) => {
            <AnsiTransactionManager as TransactionManager<diesel::SqliteConnection>>::commit_transaction(conn)
                .map_err(|e| MigrationError::Migration(e.to_string()))?;
            Ok(value)
        }
        Err(err) => {
            if let Err(rollback_err) = <AnsiTransactionManager as TransactionManager<
                diesel::SqliteConnection,
            >>::rollback_transaction(conn)
            {
                tracing::warn!(
                    "failed to roll back SQLite migration write-lock transaction after error: {rollback_err}"
                );
            }
            Err(err)
        }
    }
}

/// Return names of pending (not yet applied) migrations on a `SQLite` target.
///
/// The `SQLite` status counterpart to [`pending_migrations`], used by
/// [`auto_migrate_sqlite`] to report pending work without applying it.
///
/// # Errors
///
/// Returns [`MigrationError::Connection`] if the database cannot be opened, or
/// [`MigrationError::Migration`] if status cannot be determined.
#[cfg(feature = "sqlite")]
fn pending_migrations_sqlite(
    database_url: &str,
    migrations: impl diesel::migration::MigrationSource<diesel::sqlite::Sqlite>,
) -> Result<Vec<String>, MigrationError> {
    let mut conn = crate::db::establish_sqlite_migration_connection(database_url)
        .map_err(|e| MigrationError::Connection(e.to_string()))?;
    let pending = conn
        .pending_migrations(migrations)
        .map_err(|e| MigrationError::Migration(e.to_string()))?;
    Ok(pending
        .iter()
        .map(|m| m.name().version().to_string())
        .collect())
}

pub(crate) fn compare_replica_migration_versions(
    primary: &[String],
    replica: &[String],
) -> ReplicaMigrationReadiness {
    let primary_versions: std::collections::BTreeSet<_> = primary.iter().collect();
    let replica_versions: std::collections::BTreeSet<_> = replica.iter().collect();

    if primary_versions == replica_versions {
        ReplicaMigrationReadiness::Ready
    } else {
        ReplicaMigrationReadiness::Stale {
            primary_latest: primary_versions
                .iter()
                .next_back()
                .map(|version| (*version).clone()),
            replica_latest: replica_versions
                .iter()
                .next_back()
                .map(|version| (*version).clone()),
        }
    }
}

fn applied_migration_versions(database_url: &str) -> Result<Vec<String>, MigrationError> {
    with_migration_connection!(database_url, |conn| {
        let rows =
            diesel::sql_query("SELECT version FROM __diesel_schema_migrations ORDER BY version")
                .load::<AppliedMigrationVersion>(conn)
                .map_err(|e| MigrationError::Migration(e.to_string()))?;

        Ok(rows.into_iter().map(|row| row.version).collect())
    })
}

pub(crate) fn check_replica_migration_readiness(
    primary_url: &str,
    replica_url: &str,
) -> ReplicaMigrationReadiness {
    let primary = match applied_migration_versions(primary_url) {
        Ok(versions) => versions,
        Err(error) => return ReplicaMigrationReadiness::Unknown(error.to_string()),
    };
    let replica = match applied_migration_versions(replica_url) {
        Ok(versions) => versions,
        Err(error) => return ReplicaMigrationReadiness::Unknown(error.to_string()),
    };

    compare_replica_migration_versions(&primary, &replica)
}

pub(crate) async fn check_replica_migration_readiness_blocking(
    primary_url: String,
    replica_url: String,
) -> ReplicaMigrationReadiness {
    tokio::task::spawn_blocking(move || {
        check_replica_migration_readiness(&primary_url, &replica_url)
    })
    .await
    .unwrap_or_else(|error| {
        ReplicaMigrationReadiness::Unknown(format!(
            "replica migration readiness task failed: {error}"
        ))
    })
}

/// Acquire the `PostgreSQL` session-level advisory lock that serializes migration runs.
///
/// Polls `pg_try_advisory_lock` at 500 ms intervals until the lock is
/// acquired or `timeout` elapses. Logs at `INFO` on acquisition and `DEBUG`
/// while waiting.
///
/// **Non-`PostgreSQL` note:** advisory locks are a `PostgreSQL`-specific primitive.
/// `SQLite` and in-memory test harnesses do not support them. Those backends are
/// single-process by nature; `run_pending` (the unlocked variant) is the right
/// choice there.
///
/// # Errors
///
/// Returns [`MigrationError::Migration`] if the database query fails, or
/// [`MigrationError::LockTimeout`] if the lock is not acquired within `timeout`.
pub fn acquire_migration_lock(
    conn: &mut diesel::PgConnection,
    timeout: std::time::Duration,
) -> Result<(), MigrationError> {
    acquire_migration_lock_on(conn, timeout)
}

/// Generic body of [`acquire_migration_lock`], usable with both the native
/// `PgConnection` and the rustls migration wrapper (see
/// [`crate::db::MigrationConnection`]).
fn acquire_migration_lock_on<C>(
    conn: &mut C,
    timeout: std::time::Duration,
) -> Result<(), MigrationError>
where
    C: diesel::connection::LoadConnection<Backend = Pg>,
{
    let start = std::time::Instant::now();
    let poll = std::time::Duration::from_millis(500);

    tracing::info!(
        lock_key = MIGRATION_ADVISORY_LOCK_KEY,
        timeout_secs = timeout.as_secs(),
        "Acquiring migration advisory lock",
    );

    loop {
        let acquired = diesel::sql_query("SELECT pg_try_advisory_lock($1) AS acquired")
            .bind::<diesel::sql_types::BigInt, _>(MIGRATION_ADVISORY_LOCK_KEY)
            .get_result::<AdvisoryLockRow>(conn)
            .map_err(|e| MigrationError::Migration(e.to_string()))?
            .acquired;

        if acquired {
            tracing::info!("Migration advisory lock acquired");
            return Ok(());
        }

        let elapsed = start.elapsed();
        if elapsed >= timeout {
            return Err(MigrationError::LockTimeout {
                timeout_secs: timeout.as_secs(),
            });
        }

        tracing::debug!(
            elapsed_secs = elapsed.as_secs(),
            timeout_secs = timeout.as_secs(),
            "Waiting for migration advisory lock; another process may be running migrations",
        );

        std::thread::sleep(poll.min(timeout.saturating_sub(elapsed)));
    }
}

/// Release the `PostgreSQL` session-level advisory lock acquired by
/// [`acquire_migration_lock`].
///
/// Called automatically by [`MigrationLockGuard`] on drop. Logs at `INFO` on
/// success and `WARN` if the lock was not held or the query fails. `PostgreSQL`
/// also releases session-level advisory locks automatically when the connection
/// closes, so a missed explicit release is safe.
pub fn release_migration_lock(conn: &mut diesel::PgConnection) {
    release_migration_lock_on(conn);
}

/// Generic body of [`release_migration_lock`], usable with both the native
/// `PgConnection` and the rustls migration wrapper.
fn release_migration_lock_on<C>(conn: &mut C)
where
    C: diesel::connection::LoadConnection<Backend = Pg>,
{
    match diesel::sql_query("SELECT pg_advisory_unlock($1) AS released")
        .bind::<diesel::sql_types::BigInt, _>(MIGRATION_ADVISORY_LOCK_KEY)
        .get_result::<AdvisoryUnlockRow>(conn)
    {
        Ok(row) if row.released => {
            tracing::info!("Migration advisory lock released");
        }
        Ok(_) => {
            tracing::warn!("Migration advisory unlock returned false: lock was not held");
        }
        Err(e) => {
            tracing::warn!(error = %e, "Failed to release migration advisory lock");
        }
    }
}

/// RAII guard that holds a `PostgreSQL` advisory lock for the duration of a
/// migration run.
///
/// Created by [`hold_migration_lock`]. The lock is released when this guard
/// drops, or automatically when the underlying connection closes on process
/// exit (so `std::process::exit` is safe).
///
/// # Non-`PostgreSQL` backends
///
/// `SQLite` and in-memory test harnesses do not support advisory locks and do
/// not need cross-process serialization (they are single-process by nature).
/// Skip this guard when running against those backends.
pub struct MigrationLockGuard {
    // TLS-aware (issue #1585 review): `autumn migrate` acquires this lock
    // BEFORE spawning the external diesel CLI, so the lock connection itself
    // must honor the URL's sslmode — the bundled libpq cannot reach
    // TLS-only servers at all.
    conn: crate::db::MigrationConnection,
}

impl std::fmt::Debug for MigrationLockGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MigrationLockGuard").finish_non_exhaustive()
    }
}

impl Drop for MigrationLockGuard {
    fn drop(&mut self) {
        match &mut self.conn {
            crate::db::MigrationConnection::Native(conn) => release_migration_lock_on(conn),
            crate::db::MigrationConnection::Rustls { conn, .. } => release_migration_lock_on(conn),
        }
    }
}

/// A single user migration that was successfully reverted.
///
/// Emitted by the `on_reverted` callback in [`revert_user_migrations_locked`] after
/// each successful revert so callers can stream per-migration UX output.
#[derive(Debug)]
pub struct RevertedMigration {
    /// Version string (e.g. `"20260101000000"`).
    pub version: String,
    /// Full migration name including version prefix (e.g. `"20260101000000_create_posts"`).
    pub name: String,
    /// Wall-clock time taken by the revert.
    pub duration: std::time::Duration,
}

/// An applied **user** migration, resolved against the local `migrations/`
/// directory using Diesel's own version normalisation.
///
/// `dir` is `None` when the migration is recorded as applied in the database
/// but is no longer present locally (e.g. deploying from a branch that lacks
/// it). Such migrations are surfaced — not silently dropped — so a rollback can
/// refuse rather than revert an older migration out of order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedUserMigration {
    /// Normalised version string (Diesel's `version()`), e.g. `"20260101000000"`.
    pub version: String,
    /// Local migration directory name (e.g. `"20260101000000_create_posts"`),
    /// or the bare version if the migration is not present locally.
    pub name: String,
    /// Path to the local migration directory, or `None` if it is missing.
    pub dir: Option<std::path::PathBuf>,
}

/// Versions of all embedded framework migrations: the control-plane
/// [`FRAMEWORK_MIGRATIONS`] plus the shard-required version-history,
/// commit-hook queue and derivation-state migrations.
///
/// Used to exclude framework-owned migrations from user rollback planning so
/// the forward-only contract is preserved regardless of which migrations are
/// applied locally. The shard-required sets must be included too: on a shard
/// target they are recorded in `__diesel_schema_migrations` but have no user
/// `down.sql`, so without this exclusion `autumn migrate down --shard` would
/// plan one of them as a user migration and fail.
fn framework_migration_versions() -> Result<std::collections::BTreeSet<String>, MigrationError> {
    framework_migration_versions_for::<Pg>()
}

/// Backend-generic core of [`framework_migration_versions`]. The version strings
/// are identical across backends (they come from the same embedded directory
/// names), so the `SQLite` rollback path (issue #2058) can enumerate the same
/// framework-owned set through the `Sqlite` `MigrationSource` impl without
/// duplicating the list.
fn framework_migration_versions_for<DB>()
-> Result<std::collections::BTreeSet<String>, MigrationError>
where
    DB: diesel::backend::Backend,
    EmbeddedMigrations: diesel::migration::MigrationSource<DB>,
{
    let mut versions = std::collections::BTreeSet::new();
    for migrations in [
        MigrationSource::<DB>::migrations(&FRAMEWORK_MIGRATIONS),
        MigrationSource::<DB>::migrations(&crate::version_history::VERSION_HISTORY_MIGRATIONS),
        MigrationSource::<DB>::migrations(
            &crate::repository_commit_hooks::REPOSITORY_COMMIT_HOOK_MIGRATIONS,
        ),
        MigrationSource::<DB>::migrations(&crate::derivation::DERIVATION_MIGRATIONS),
    ] {
        let migrations = migrations.map_err(|e| MigrationError::Migration(e.to_string()))?;
        versions.extend(migrations.iter().map(|m| m.name().version().to_string()));
    }
    Ok(versions)
}

/// Enumerate `(version, full_name)` for every migration in an embedded set —
/// `version` is what `__diesel_schema_migrations` actually keys on (e.g.
/// `"20260101000000"`); `full_name` also carries the description (e.g.
/// `"20260101000000_create_posts"`), which is what distinguishes "the exact
/// same migration, registered twice" from "two unrelated migrations that
/// happen to share a version".
///
/// Backend-generic so it enumerates a `SQLite`-oriented embedded set too —
/// the version/name metadata comes from the migration directory naming, not
/// from parsing the backend-specific SQL, so which `DB` is chosen here never
/// changes the result.
///
/// # Errors
///
/// Returns [`MigrationError::Migration`] if the embedded set's metadata
/// cannot be enumerated (not expected in practice for a `const` produced by
/// `embed_migrations!`, but the underlying Diesel API is fallible).
pub(crate) fn migration_versions_and_names<DB>(
    source: &EmbeddedMigrations,
) -> Result<Vec<(String, String)>, MigrationError>
where
    DB: diesel::backend::Backend,
    EmbeddedMigrations: diesel::migration::MigrationSource<DB>,
{
    let migrations = MigrationSource::<DB>::migrations(source)
        .map_err(|e| MigrationError::Migration(e.to_string()))?;
    Ok(migrations
        .iter()
        .map(|m| (m.name().version().to_string(), m.name().to_string()))
        .collect())
}

/// Compute a `full migration name -> substitute version` map that resolves
/// every migration-version collision across `named_sets` automatically,
/// rather than rejecting registration — see
/// [`AppBuilder::plugin_migrations`](crate::app::AppBuilder::plugin_migrations)
/// for the full rationale. Pure and DB-free (parses each embedded set's own
/// version/name metadata; no connection involved), so it can run right
/// before the apply loop, on the FINAL set of registered sources — including
/// ones the framework itself folds in after app-wiring time (the
/// shard-required version-history / commit-hook-queue sets, and the two
/// standalone shard-directory / shard-map control migrations, which are
/// otherwise applied straight from their own `const`s rather than through
/// the app's registered `migrations`), closing the gap a purely
/// registration-time check would miss. Callers pass borrowed `EmbeddedMigrations`
/// (not owned) so a call site can cheaply include such standalone `const`
/// sets alongside an already-built `Vec` of owned ones without needing
/// `EmbeddedMigrations` to implement `Clone` (it deliberately doesn't).
///
/// Diesel's `__diesel_schema_migrations` table is keyed by **version
/// alone** — it has no notion of which registered source (the framework, a
/// plugin, the app's own `migrations/`) recorded a version. Two
/// independently authored migrations that happen to reuse the same version
/// — nothing coordinates timestamps across a plugin, the framework, and an
/// app — would otherwise collide silently: whichever set's `run_pending`
/// call runs first "wins" the version, and every other set's same-versioned
/// migration is skipped forever as "already applied", even though its
/// `up.sql` never actually ran.
///
/// For each version claimed by more than one DISTINCT full migration name,
/// the lexicographically-first full name keeps the plain version; every
/// other one is mapped to a bounded, deterministic substitute from
/// [`bounded_substitute_version`], logged at `INFO` so the resolution is
/// visible. This ordering is a pure function of the colliding migrations'
/// own names — **not** of `named_sets`' order — so reordering
/// `.migrations()`/`.plugin_migrations()` calls, or adding a new plugin,
/// never flips which one already-settled collisions resolve to. The
/// substitute itself is also salted with the LOSING migration's own full
/// name (never a source name), which stays fixed for the migration's
/// lifetime even as the *set* of sources registering a duplicate grows or
/// shrinks release over release (the same bundle later folded into an
/// additional plugin, say) — hashing on that changeable set instead would
/// silently reassign an already-applied migration's tracked substitute the
/// moment its registration footprint changed, even though the migration
/// itself never did. Every generated substitute is also checked against
/// every RAW version already in use (not just other substitutes), so it can
/// never coincide with an unrelated migration's own plain version. A version
/// reused under the exact SAME full name (e.g. a shard-required set folded
/// verbatim into the full framework bundle too) is the intentional,
/// harmless case and is left untouched — only genuinely different
/// migrations get remapped, under the assumption that two migrations
/// sharing both a version AND a full name are the same migration; two
/// unrelated authors coincidentally picking both is not detected (Diesel's
/// embedded-migration API does not expose raw `up.sql` bytes at runtime to
/// check further).
///
/// This makes fresh installs and apps that have always registered the same
/// sources together fully safe. It does NOT recover history for a source
/// introduced after a database already has one side of the collision
/// applied under its plain version — see
/// [`AppBuilder::plugin_migrations`](crate::app::AppBuilder::plugin_migrations)'s
/// doc comment for why that case is fundamentally unrecoverable from the
/// table alone, and what to do instead.
///
/// Returns an empty map when nothing collides — the overwhelmingly common
/// case — so callers can skip wrapping entirely when it's empty.
pub(crate) fn compute_migration_disambiguation(
    named_sets: &[(&str, &EmbeddedMigrations)],
) -> HashMap<String, String> {
    // version -> distinct full names claiming it. Only the DISTINCT full
    // names matter for collision detection: the same migration folded into
    // two bundles under different source names (the intentional, harmless
    // case) contributes one entry, not two, regardless of how many sources
    // register it or in what order.
    let mut by_version: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for (_, set) in named_sets {
        let Ok(pairs) = migration_versions_and_names::<Pg>(set) else {
            continue;
        };
        for (version, full_name) in pairs {
            let entries = by_version.entry(version).or_default();
            if !entries.contains(&full_name) {
                entries.push(full_name);
            }
        }
    }

    let mut disambiguated = HashMap::new();
    // Seed with every RAW version already claimed by some registered
    // migration, not just substitutes handed out so far — otherwise a
    // generated substitute could coincide with an unrelated migration's own
    // plain version and the two would still share one Diesel tracking key.
    let mut substitutes_in_use: std::collections::HashSet<String> =
        by_version.keys().cloned().collect();
    for (version, mut entries) in by_version {
        if entries.len() <= 1 {
            continue; // 0 or 1 distinct migration claims this version -- no collision.
        }
        // Deterministic, CONTENT-based order -- a pure function of the
        // colliding migrations' own full names, independent of which
        // `.migrations()`/`.plugin_migrations()` call happened to run
        // first. This matters because registration order is NOT stable
        // across builds: reordering those calls in source, or adding a new
        // plugin, must not change which migration keeps the plain version.
        // The lexicographically-first full name wins it; every other
        // colliding migration is mapped to a substitute below.
        entries.sort();
        let kept_name = entries[0].clone();
        for full_name in entries.into_iter().skip(1) {
            // The substitute hash derives from the losing migration's own full
            // name, not from which sources currently register it. A migration's
            // full name is fixed by its own directory naming and never changes
            // across releases, whereas the set of sources registering a duplicate
            // can grow or shrink — the same bundle folded into an additional
            // plugin, say. Hashing on that set would silently reassign an
            // already-applied migration's tracked substitute the moment its
            // registration footprint changed, though the migration itself did not.
            let mut tie_breaker = 1u32;
            let mut substitute = bounded_substitute_version(&version, &full_name, tie_breaker);
            while substitutes_in_use.contains(&substitute) {
                tie_breaker += 1;
                substitute = bounded_substitute_version(&version, &full_name, tie_breaker);
            }
            tracing::info!(
                version = %version,
                migration = %full_name,
                collides_with = %kept_name,
                substitute_version = %substitute,
                "Migration version collision resolved automatically — both migrations will still apply"
            );
            substitutes_in_use.insert(substitute.clone());
            disambiguated.insert(full_name, substitute);
        }
    }
    disambiguated
}

/// Build a substitute version for a colliding migration that (a) never
/// collides with the original version or any other substitute already
/// handed out (`tie_breaker` bumps on a collision), and (b) always fits
/// Diesel's `__diesel_schema_migrations.version` column (`VARCHAR(50)`)
/// regardless of how long `version` or `salt` are.
///
/// `salt` should be a value that is stable for the lifetime of the migration
/// it disambiguates — [`compute_migration_disambiguation`] passes the
/// migration's own full name (fixed by its directory naming, never by which
/// or how many sources currently register it). Uses a short, stable hash of
/// `salt` (not the value itself, which could alone exceed the column width)
/// plus a numeric tie-breaker suffix. Deterministic: the same `(version,
/// salt, tie_breaker)` always produces the same substitute.
fn bounded_substitute_version(version: &str, salt: &str, tie_breaker: u32) -> String {
    let mut hasher = Sha256::new();
    hasher.update(salt.as_bytes());
    let hash = hex::encode(hasher.finalize());
    let short_hash = &hash[..8];
    let suffix = if tie_breaker <= 1 {
        format!("+{short_hash}")
    } else {
        format!("+{short_hash}-{tie_breaker}")
    };
    // Reserve room for the suffix; truncate an unusually long version rather
    // than risk overflowing the VARCHAR(50) column.
    let max_version_len = 50usize.saturating_sub(suffix.len());
    let truncated_version: String = version.chars().take(max_version_len).collect();
    format!("{truncated_version}{suffix}")
}

/// A migration whose TRACKED identity is a substitute version, while its
/// actual behavior (`run`/`revert`/`metadata`) delegates entirely to the
/// original. Built by [`DisambiguatedMigrations`] for an entry named in a
/// [`compute_migration_disambiguation`] result.
struct RenamedMigration<DB: diesel::backend::Backend + 'static> {
    inner: Box<dyn Migration<DB>>,
    name: RenamedMigrationName,
}

/// The [`diesel::migration::MigrationName`] a [`RenamedMigration`] reports:
/// the same display text as the original (so logs and `autumn migrate
/// status` still show the real migration name), but a substitute version.
struct RenamedMigrationName {
    display: String,
    version: String,
}

impl std::fmt::Display for RenamedMigrationName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.display)
    }
}

impl diesel::migration::MigrationName for RenamedMigrationName {
    fn version(&self) -> diesel::migration::MigrationVersion<'_> {
        diesel::migration::MigrationVersion::from(self.version.as_str())
    }
}

impl<DB: diesel::backend::Backend + 'static> Migration<DB> for RenamedMigration<DB> {
    fn run(
        &self,
        conn: &mut dyn diesel::connection::BoxableConnection<DB>,
    ) -> diesel::migration::Result<()> {
        self.inner.run(conn)
    }

    fn revert(
        &self,
        conn: &mut dyn diesel::connection::BoxableConnection<DB>,
    ) -> diesel::migration::Result<()> {
        self.inner.revert(conn)
    }

    fn metadata(&self) -> &dyn diesel::migration::MigrationMetadata {
        self.inner.metadata()
    }

    fn name(&self) -> &dyn diesel::migration::MigrationName {
        &self.name
    }
}

/// Wraps an [`EmbeddedMigrations`] set, transparently substituting the
/// tracked version of any migration named in `disambiguated` (full name ->
/// substitute version — see [`compute_migration_disambiguation`]). Every
/// other migration in the set passes through completely unchanged — this is
/// a no-op wrapper when `disambiguated` is empty, the overwhelmingly common
/// case (no collision was ever detected).
pub(crate) struct DisambiguatedMigrations<'a> {
    inner: &'a EmbeddedMigrations,
    disambiguated: &'a HashMap<String, String>,
}

impl<'a> DisambiguatedMigrations<'a> {
    pub(crate) const fn new(
        inner: &'a EmbeddedMigrations,
        disambiguated: &'a HashMap<String, String>,
    ) -> Self {
        Self {
            inner,
            disambiguated,
        }
    }
}

impl<DB> MigrationSource<DB> for DisambiguatedMigrations<'_>
where
    DB: diesel::backend::Backend + 'static,
    EmbeddedMigrations: MigrationSource<DB>,
{
    fn migrations(&self) -> diesel::migration::Result<Vec<Box<dyn Migration<DB>>>> {
        let migrations = MigrationSource::<DB>::migrations(self.inner)?;
        Ok(migrations
            .into_iter()
            .map(|m| {
                let full_name = m.name().to_string();
                match self.disambiguated.get(&full_name) {
                    Some(substitute) => Box::new(RenamedMigration {
                        name: RenamedMigrationName {
                            display: full_name,
                            version: substitute.clone(),
                        },
                        inner: m,
                    }) as Box<dyn Migration<DB>>,
                    None => m,
                }
            })
            .collect())
    }
}

/// SHA-256 content hash of a migration's `up.sql`, used to detect the case
/// where a migration was edited **after** it was applied. Deterministic across
/// platforms:
///
///   1. Line-ending normalisation: `\r\n` → `\n`, then any remaining `\r`
///      → `\n`, so a Windows checkout matches a Linux one.
///   2. `trim_end()` on the resulting string — trailing whitespace and the
///      customary final newline are removed so tools that strip / re-append
///      them cannot spuriously trip the mismatch guard.
///   3. Lower-case hex of the SHA-256 of the normalised bytes.
///
/// The same normalisation is applied at record-time (`record_checksum` /
/// `record_checksums`) and at validate-time (`validate_checksums`) so the
/// hash a migration recorded when it was applied still compares equal to the
/// hash of the same on-disk content later.
#[must_use]
pub fn migration_checksum(up_sql: &str) -> String {
    let normalised = normalise_up_sql(up_sql);
    let mut hasher = Sha256::new();
    hasher.update(normalised.as_bytes());
    hex::encode(hasher.finalize())
}

/// Bytes variant of [`migration_checksum`]: normalises the input via a
/// lossy-UTF-8 decode and then hashes it exactly like [`migration_checksum`].
///
/// Non-UTF-8 bytes are decoded lossily so this call never fails, and it hashes
/// identically to [`migration_checksum`] for valid UTF-8 input. It exists for
/// callers that already hold raw `up.sql` bytes and is exercised by the
/// checksum path/tests.
///
/// Note: it is **not** wired to embedded startup bytes. Startup auto-migrate
/// validation re-hashes the on-disk `up.sql` (see
/// [`validate_recorded_checksums_against_dir`]); Diesel's embedded `Migration`
/// API does not expose each migration's raw SQL, so there is no embedded-bytes
/// path to feed this function at startup.
#[must_use]
pub fn migration_checksum_bytes(up_sql: &[u8]) -> String {
    // Lossy decode: mirror the CLI's on-disk read (`fs::read_to_string`),
    // which itself rejects non-UTF-8 — the lossy path is only exercised
    // when the embedded macro somehow ships non-UTF-8, which Diesel does
    // not. Keeping the API infallible avoids a fallible checksum in the
    // apply loop.
    let s = String::from_utf8_lossy(up_sql);
    migration_checksum(&s)
}

fn normalise_up_sql(up_sql: &str) -> String {
    let mut normalised = up_sql.replace("\r\n", "\n");
    if normalised.contains('\r') {
        normalised = normalised.replace('\r', "\n");
    }
    let trimmed = normalised.trim_end();
    trimmed.to_owned()
}

/// The recorded-vs-actual state of one applied migration's `up.sql`.
///
/// Produced by [`classify`] and consumed by [`validate_checksums`] and the
/// CLI's `status` printer. `Ok` is the normal happy path; `Unrecorded`
/// covers legacy migrations applied before the framework tracked
/// checksums (baseline them with `autumn migrate baseline`); `Changed`
/// and `Missing` are the failure modes. `Changed` is an applied migration
/// whose on-disk `up.sql` no longer matches the checksum recorded when it
/// was applied; `Missing` is an applied migration that *had* a recorded
/// checksum (so it was once part of this source tree) but whose `up.sql`
/// is now gone — deleted or renamed after being applied. Both mean the
/// schema in production silently differs from what a fresh build would
/// produce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChecksumState {
    /// The recorded checksum matches the current on-disk content — safe.
    Ok,
    /// A recorded checksum exists but disagrees with the current content:
    /// the migration was edited after being applied.
    Changed {
        /// Hex-encoded SHA-256 of the `up.sql` at the time it was applied.
        recorded: String,
        /// Hex-encoded SHA-256 of the current on-disk `up.sql`.
        actual: String,
    },
    /// A recorded checksum exists but the on-disk `up.sql` is gone — the
    /// migration was deleted or renamed after being applied. Because a
    /// checksum is only ever recorded for a migration whose `up.sql` was
    /// present in *this* migrations dir at record time (see
    /// [`record_checksums`]), a recorded-but-now-absent file is genuine
    /// drift: a fresh DB built from the current source tree would no longer
    /// run this migration. Embedded/framework migrations never receive a
    /// recorded checksum against the user dir, so they can never land here.
    Missing {
        /// Hex-encoded SHA-256 of the `up.sql` at the time it was applied.
        recorded: String,
    },
    /// The migration has no recorded checksum — either applied before
    /// this feature existed, or its `up.sql` could not be resolved to
    /// hash and it was never recorded. Never itself an error;
    /// `autumn migrate baseline` records pending on-disk hashes so future
    /// edits are caught.
    Unrecorded,
}

/// Classify each applied migration's `up.sql` against its recorded checksum.
///
/// * `applied` — applied migration versions from `__diesel_schema_migrations`.
/// * `up_sql_by_version` — the current on-disk `up.sql` text for each version
///   the caller could resolve (may be missing entries for versions that were
///   removed locally; a version whose `up.sql` is absent is reported as
///   [`ChecksumState::Missing`] when it has a recorded checksum — genuine
///   drift — and [`ChecksumState::Unrecorded`] when it has none).
/// * `recorded` — the (version → hex checksum) map from
///   `autumn_migration_checksums`.
///
/// Result order matches `applied`, so callers can iterate the classification
/// alongside the applied list for status printing.
#[must_use]
pub fn classify<S1, S2>(
    applied: &[String],
    up_sql_by_version: &HashMap<String, String, S1>,
    recorded: &HashMap<String, String, S2>,
) -> Vec<(String, ChecksumState)>
where
    S1: std::hash::BuildHasher,
    S2: std::hash::BuildHasher,
{
    applied
        .iter()
        .map(|version| {
            let state = match (up_sql_by_version.get(version), recorded.get(version)) {
                (Some(up_sql), Some(recorded_hash)) => {
                    let actual = migration_checksum(up_sql);
                    if &actual == recorded_hash {
                        ChecksumState::Ok
                    } else {
                        ChecksumState::Changed {
                            recorded: recorded_hash.clone(),
                            actual,
                        }
                    }
                }
                // Recorded as applied but the on-disk up.sql is gone: the
                // migration was deleted or renamed after being applied. The
                // recorded checksum proves it once belonged to THIS dir (a
                // checksum is only recorded for a file present at record
                // time), so this is genuine drift — not the legacy case.
                // Framework/embedded migrations never get a recorded checksum
                // against the user dir, so they can never reach this arm.
                (None, Some(recorded_hash)) => ChecksumState::Missing {
                    recorded: recorded_hash.clone(),
                },
                // No recorded checksum (with or without on-disk up.sql):
                // legacy migration applied before checksum tracking, or an
                // unresolvable up.sql that was never recorded. Never an error.
                (Some(_) | None, None) => ChecksumState::Unrecorded,
            };
            (version.clone(), state)
        })
        .collect()
}

/// Fail fast on the first drifted migration checksum.
///
/// Two failure modes are caught, both of which silently fork the schema
/// between environments:
///
///   * [`ChecksumState::Changed`] — a migration was edited after being applied.
///   * [`ChecksumState::Missing`] — a migration was deleted or renamed after
///     being applied (its `up.sql` is gone but it still has a recorded
///     checksum, so a fresh DB from the current source tree would no longer
///     run it).
///
/// `Unrecorded` entries never fail — they are the legacy state before this
/// feature existed, and `autumn migrate baseline` records their current hash
/// so future edits are caught. Because `Missing` requires a recorded checksum,
/// and a checksum is only recorded for a file that was present in this dir at
/// record time, embedded/framework migrations (never recorded against the user
/// dir) cannot trigger a false-positive `Missing`.
///
/// # Errors
///
/// Returns [`MigrationError::Migration`] with a message that names the version.
/// For `Changed` it includes the recorded hex, the actual hex, and the remedy
/// (never edit an applied migration — add a new one; the re-baseline command
/// is the deliberate escape hatch). For `Missing` it explains that a migration
/// must never be deleted or renamed after being applied.
pub fn validate_checksums<S1, S2>(
    applied: &[String],
    up_sql_by_version: &HashMap<String, String, S1>,
    recorded: &HashMap<String, String, S2>,
) -> Result<(), MigrationError>
where
    S1: std::hash::BuildHasher,
    S2: std::hash::BuildHasher,
{
    for (version, state) in classify(applied, up_sql_by_version, recorded) {
        match state {
            ChecksumState::Changed { recorded, actual } => {
                return Err(MigrationError::Migration(format!(
                    "migration {version} checksum mismatch: recorded {recorded} but on-disk \
                     content hashes to {actual}. Migrations must never be edited after being \
                     applied \u{2014} add a new migration instead, or run the documented \
                     re-baseline command if this change was deliberate."
                )));
            }
            ChecksumState::Missing { recorded } => {
                return Err(MigrationError::Migration(format!(
                    "migration {version} is recorded as applied (checksum {recorded}) but its \
                     up.sql is missing from the source tree \u{2014} a migration must never be \
                     deleted or renamed after being applied; add a new migration instead."
                )));
            }
            ChecksumState::Ok | ChecksumState::Unrecorded => {}
        }
    }
    Ok(())
}

// ── DB helpers: autumn_migration_checksums ───────────────────────────────

#[derive(diesel::QueryableByName)]
struct RecordedChecksumRow {
    #[diesel(sql_type = diesel::sql_types::Text)]
    version: String,
    #[diesel(sql_type = diesel::sql_types::Text)]
    checksum: String,
}

#[derive(diesel::QueryableByName)]
struct TableExistsRow {
    #[diesel(sql_type = diesel::sql_types::Bool)]
    present: bool,
}

/// Ensure the framework-owned `autumn_migration_checksums` table exists on
/// this connection, creating it idempotently if absent.
///
/// The same DDL ships as a framework migration
/// (`20260709000000_create_migration_checksums`) so fresh databases get the
/// managed path and a `down.sql` — but the checksum record/validate paths must
/// **not** depend on that migration having run. The startup auto-migrate path
/// (and shard targets) only apply the app-registered migration sets, which by
/// default do not include [`FRAMEWORK_MIGRATIONS`]; without this helper the
/// table would never be created there and every dev recording would warn while
/// startup validation stayed vacuous (issue #1203 review, B1/S2). Creating the
/// table here works identically on control and shard targets.
///
/// `CREATE TABLE IF NOT EXISTS` is safe and idempotent under autocommit (the
/// [`with_migration_connection!`] connection has no surrounding transaction),
/// and this only ever runs on the primary migration/write connection — never on
/// a read replica (the replica parity path never touches the checksum table).
///
/// # Errors
///
/// Returns [`MigrationError::Migration`] if the `CREATE TABLE` cannot run (e.g.
/// a read-only connection). Best-effort record callers log the warning and
/// continue; validate callers surface it the same way they surface any other DB
/// error, and it never masks a real checksum mismatch.
fn ensure_checksum_table<C>(conn: &mut C) -> Result<(), MigrationError>
where
    C: diesel::connection::LoadConnection<Backend = Pg>,
{
    diesel::sql_query(
        "CREATE TABLE IF NOT EXISTS autumn_migration_checksums (\
             version    TEXT PRIMARY KEY, \
             checksum   TEXT NOT NULL, \
             algorithm  TEXT NOT NULL DEFAULT 'sha256', \
             recorded_at TIMESTAMPTZ NOT NULL DEFAULT now()\
         )",
    )
    .execute(conn)
    .map_err(|e| MigrationError::Migration(e.to_string()))?;
    Ok(())
}

/// Load the full `(version, checksum)` map from `autumn_migration_checksums`.
///
/// **Read-only.** This never creates the table: on a fresh database (before any
/// apply/record path has created it) it probes for the relation with
/// `to_regclass` and returns an empty map when absent. This keeps read paths —
/// `autumn migrate status` and the pre-apply validation — from requiring DDL
/// privileges or mutating the database just to display / check state (issue
/// #1203 review, P2-A).
///
/// The table is created lazily by the *write* helpers ([`record_checksum`],
/// [`record_checksums`], [`rebaseline_checksum`], [`delete_checksum`],
/// [`delete_checksums`]), each of which calls `ensure_checksum_table` before
/// writing. So the "validate → apply → record" sequence on the startup
/// auto-migrate and shard paths still works: the pre-apply validate reads an
/// empty map (nothing to fork from yet, no error), and the subsequent
/// `record_checksums` creates the table and records the freshly-applied hashes.
///
/// `to_regclass` returns NULL for an unknown relation — an existence test that
/// never errors and never depends on localized "does not exist" error text
/// (which breaks under a non-English `lc_messages`).
///
/// # Errors
///
/// Returns [`MigrationError::Migration`] for any database error.
pub fn recorded_checksums<C>(conn: &mut C) -> Result<HashMap<String, String>, MigrationError>
where
    C: diesel::connection::LoadConnection<Backend = Pg>,
{
    let present = diesel::sql_query(
        "SELECT to_regclass('autumn_migration_checksums') IS NOT NULL AS present",
    )
    .get_result::<TableExistsRow>(conn)
    .map_err(|e| MigrationError::Migration(e.to_string()))?
    .present;
    if !present {
        return Ok(HashMap::new());
    }
    let rows: Vec<RecordedChecksumRow> =
        diesel::sql_query("SELECT version, checksum FROM autumn_migration_checksums")
            .load(conn)
            .map_err(|e| MigrationError::Migration(e.to_string()))?;
    Ok(rows.into_iter().map(|r| (r.version, r.checksum)).collect())
}

/// Record a migration's checksum. Idempotent — a repeat call for the same
/// version is a no-op (ON CONFLICT DO NOTHING) so the normal apply path can
/// be re-run without disturbing the historical record.
///
/// # Errors
///
/// Returns [`MigrationError::Migration`] on any database error.
pub fn record_checksum<C>(conn: &mut C, version: &str, checksum: &str) -> Result<(), MigrationError>
where
    C: diesel::connection::LoadConnection<Backend = Pg>,
{
    ensure_checksum_table(conn)?;
    diesel::sql_query(
        "INSERT INTO autumn_migration_checksums (version, checksum) \
         VALUES ($1, $2) ON CONFLICT (version) DO NOTHING",
    )
    .bind::<diesel::sql_types::Text, _>(version)
    .bind::<diesel::sql_types::Text, _>(checksum)
    .execute(conn)
    .map_err(|e| MigrationError::Migration(e.to_string()))?;
    Ok(())
}

/// Overwrite a previously recorded checksum for one version (escape hatch).
///
/// Invoked by `autumn migrate baseline --force <version>` when an operator
/// has deliberately edited an applied migration and accepts the fork risk.
/// Logged at `WARN` so the change is unambiguous in deploy logs.
///
/// The normal path is [`record_checksum`], which never rewrites. This function
/// exists only for the re-baseline command; nothing in the framework calls it
/// automatically.
///
/// # Errors
///
/// Returns [`MigrationError::Migration`] on any database error.
pub fn rebaseline_checksum<C>(
    conn: &mut C,
    version: &str,
    checksum: &str,
) -> Result<(), MigrationError>
where
    C: diesel::connection::LoadConnection<Backend = Pg>,
{
    ensure_checksum_table(conn)?;
    // Read the prior value so the WARN log can name old → new.
    let prior: Vec<RecordedChecksumRow> = diesel::sql_query(
        "SELECT version, checksum FROM autumn_migration_checksums WHERE version = $1",
    )
    .bind::<diesel::sql_types::Text, _>(version)
    .load(conn)
    .map_err(|e| MigrationError::Migration(e.to_string()))?;
    let old = prior.into_iter().next().map(|r| r.checksum);

    diesel::sql_query(
        "INSERT INTO autumn_migration_checksums (version, checksum) VALUES ($1, $2) \
         ON CONFLICT (version) DO UPDATE SET checksum = EXCLUDED.checksum, \
         recorded_at = now()",
    )
    .bind::<diesel::sql_types::Text, _>(version)
    .bind::<diesel::sql_types::Text, _>(checksum)
    .execute(conn)
    .map_err(|e| MigrationError::Migration(e.to_string()))?;

    tracing::warn!(
        version = %version,
        old_checksum = %old.as_deref().unwrap_or("<none>"),
        new_checksum = %checksum,
        "Re-baselined migration checksum (escape hatch) \u{2014} the migration content \
         has been declared canonical; other environments running the previous content will \
         now report a mismatch."
    );
    Ok(())
}

/// Delete the recorded checksum row for a single version — the inverse of
/// [`record_checksum`].
///
/// Called after a migration is rolled back (`autumn migrate down`) so the
/// invariant "a row exists in `autumn_migration_checksums` for a version
/// \u{21D4} that version is currently applied, and its hash matches the
/// currently-applied bytes" is restored. Without this, the row from the
/// *previous* application survives the rollback, and a later re-apply of an
/// edited `up.sql` records nothing new (the additive [`record_checksums`] path
/// skips versions that already have a row), leaving a stale hash that only
/// trips a validate on some *later* migrate run.
///
/// `ensure_checksum_table` runs first (consistent with the other helpers,
/// and safe/idempotent under autocommit). Idempotent: deleting an absent row
/// is a no-op.
///
/// # Errors
///
/// Returns [`MigrationError::Migration`] on any database error.
pub fn delete_checksum<C>(conn: &mut C, version: &str) -> Result<(), MigrationError>
where
    C: diesel::connection::LoadConnection<Backend = Pg>,
{
    ensure_checksum_table(conn)?;
    diesel::sql_query("DELETE FROM autumn_migration_checksums WHERE version = $1")
        .bind::<diesel::sql_types::Text, _>(version)
        .execute(conn)
        .map_err(|e| MigrationError::Migration(e.to_string()))?;
    Ok(())
}

/// Delete the recorded checksum rows for several versions at once (bulk form of
/// [`delete_checksum`]). Returns the number of rows actually removed.
///
/// `ensure_checksum_table` runs first; an empty `versions` slice is a no-op
/// that returns `0`. Idempotent — versions with no recorded row are simply not
/// counted.
///
/// # Errors
///
/// Returns [`MigrationError::Migration`] on any database error.
pub fn delete_checksums<C>(conn: &mut C, versions: &[String]) -> Result<usize, MigrationError>
where
    C: diesel::connection::LoadConnection<Backend = Pg>,
{
    ensure_checksum_table(conn)?;
    let mut deleted = 0usize;
    for version in versions {
        deleted += diesel::sql_query("DELETE FROM autumn_migration_checksums WHERE version = $1")
            .bind::<diesel::sql_types::Text, _>(version)
            .execute(conn)
            .map_err(|e| MigrationError::Migration(e.to_string()))?;
    }
    Ok(deleted)
}

/// Record checksums for every applied version that has a resolvable `up.sql`
/// and no existing recorded checksum. Idempotent: existing rows are left
/// untouched.
///
/// This is used in two places:
///
///   * After a successful apply, to record the freshly-applied migrations
///     (all three apply paths: startup, CLI framework, CLI user).
///   * By `autumn migrate baseline` to backfill hashes for legacy migrations
///     applied before the checksum table existed.
///
/// # Errors
///
/// Returns [`MigrationError::Migration`] on any database error.
pub fn record_checksums<C, S>(
    conn: &mut C,
    applied: &[String],
    up_sql_by_version: &HashMap<String, String, S>,
) -> Result<usize, MigrationError>
where
    C: diesel::connection::LoadConnection<Backend = Pg>,
    S: std::hash::BuildHasher,
{
    ensure_checksum_table(conn)?;
    let existing = recorded_checksums(conn)?;
    let mut recorded = 0usize;
    for version in applied {
        if existing.contains_key(version) {
            continue;
        }
        let Some(up_sql) = up_sql_by_version.get(version) else {
            continue;
        };
        record_checksum(conn, version, &migration_checksum(up_sql))?;
        recorded += 1;
    }
    Ok(recorded)
}

/// Build `(version -> up.sql)` by scanning a migrations directory on disk.
///
/// Uses Diesel's own version normalisation (via [`FileBasedMigrations`]) so
/// hyphenated directory names (`2026-01-01-000000_x`) resolve to the same
/// version string as `__diesel_schema_migrations` records
/// (`20260101000000`).
///
/// Missing / unreadable `up.sql` files are silently skipped — the caller
/// treats absence as [`ChecksumState::Unrecorded`], which never fails
/// validation.
///
/// # Errors
///
/// Returns [`MigrationError::Migration`] if `migrations_dir` itself cannot
/// be read as a migration source.
pub fn read_up_sql_by_version(
    migrations_dir: &Path,
) -> Result<HashMap<String, String>, MigrationError> {
    let source = FileBasedMigrations::from_path(migrations_dir)
        .map_err(|e| MigrationError::Migration(format!("failed to read migrations dir: {e}")))?;
    let migrations: Vec<Box<dyn Migration<Pg>>> = source
        .migrations()
        .map_err(|e| MigrationError::Migration(e.to_string()))?;

    let mut out = HashMap::new();
    for migration in &migrations {
        let version = migration.name().version().to_string();
        let dir = migrations_dir.join(migration.name().to_string());
        let up = dir.join("up.sql");
        if let Ok(content) = std::fs::read_to_string(&up) {
            out.insert(version, content);
        }
    }
    Ok(out)
}

/// Validate recorded checksums for every applied migration.
///
/// Compares each applied migration's recorded checksum with the current
/// on-disk `up.sql` in `migrations_dir`. This is a read-only check: on a fresh
/// DB the checksum table may not exist yet, in which case [`recorded_checksums`]
/// returns an empty map (without creating the table), so every applied migration
/// classifies as `Unrecorded` and validation passes without error — the correct
/// "nothing to fork from yet" outcome.
///
/// Intended to be called immediately **before** applying pending
/// migrations — the fail-fast guard that catches "an already-applied
/// migration was edited after it was applied and now the schema silently
/// forks between environments".
///
/// # Errors
///
/// * [`MigrationError::Connection`] — the database is unreachable.
/// * [`MigrationError::Migration`] — a mismatch was found (message names the
///   offending version and both hashes), or the migrations dir cannot be read.
pub fn validate_recorded_checksums_against_dir(
    database_url: &str,
    migrations_dir: &Path,
) -> Result<(), MigrationError> {
    let up_by_version = read_up_sql_by_version(migrations_dir)?;
    with_migration_connection!(database_url, |conn| {
        let recorded = recorded_checksums(conn)?;
        let applied = load_applied_versions_lenient(conn)?;
        validate_checksums(&applied, &up_by_version, &recorded)
    })
}

/// Read `__diesel_schema_migrations`, returning an empty list when the table
/// does not yet exist (the fresh-DB case, before Diesel's first apply creates
/// it). Any other error is propagated.
fn load_applied_versions_lenient<C>(conn: &mut C) -> Result<Vec<String>, MigrationError>
where
    C: diesel::connection::LoadConnection<Backend = Pg>,
{
    // We do NOT own `__diesel_schema_migrations` (Diesel creates it on its
    // first apply), so it may be absent on a totally fresh DB. Probe for it
    // with `to_regclass`, which returns NULL for an unknown relation — an
    // existence test that never errors, and never depends on localized
    // "does not exist" error text (which breaks under a non-English
    // `lc_messages`). Only SELECT from the table once we know it's present.
    let present = diesel::sql_query(
        "SELECT to_regclass('__diesel_schema_migrations') IS NOT NULL AS present",
    )
    .get_result::<TableExistsRow>(conn)
    .map_err(|e| MigrationError::Migration(e.to_string()))?
    .present;
    if !present {
        return Ok(Vec::new());
    }
    let rows = diesel::sql_query("SELECT version FROM __diesel_schema_migrations ORDER BY version")
        .load::<AppliedMigrationVersion>(conn)
        .map_err(|e| MigrationError::Migration(e.to_string()))?;
    Ok(rows.into_iter().map(|r| r.version).collect())
}

/// Record checksums for every applied migration whose on-disk `up.sql` is
/// resolvable and does not yet have a stored hash. Returns the number of new
/// rows written. Idempotent.
///
/// Called both **after** a successful apply (to record the freshly-applied
/// migrations) and by `autumn migrate baseline` (to backfill legacy versions
/// applied before this feature existed).
///
/// # Errors
///
/// * [`MigrationError::Connection`] — the database is unreachable.
/// * [`MigrationError::Migration`] — the migrations dir cannot be read or a
///   database error occurred.
pub fn record_checksums_from_dir(
    database_url: &str,
    migrations_dir: &Path,
) -> Result<usize, MigrationError> {
    let up_by_version = read_up_sql_by_version(migrations_dir)?;
    with_migration_connection!(database_url, |conn| {
        let applied = load_applied_versions_lenient(conn)?;
        record_checksums(conn, &applied, &up_by_version)
    })
}

/// Overwrite the stored checksum for a single applied version, computed from
/// the current on-disk `up.sql` in `migrations_dir`. Emits a `WARN` log.
///
/// This is the escape hatch behind `autumn migrate baseline --force <version>`
/// — the operator has deliberately edited an applied migration and accepts
/// that other environments running the previous content will now report a
/// mismatch.
///
/// # Errors
///
/// * [`MigrationError::Connection`] — the database is unreachable.
/// * [`MigrationError::Migration`] — the version isn't currently applied, its
///   on-disk `up.sql` is unreadable, or a database error occurred.
pub fn rebaseline_checksum_from_dir(
    database_url: &str,
    migrations_dir: &Path,
    version: &str,
) -> Result<(), MigrationError> {
    let up_by_version = read_up_sql_by_version(migrations_dir)?;
    let Some(up_sql) = up_by_version.get(version) else {
        return Err(MigrationError::Migration(format!(
            "cannot re-baseline {version}: its up.sql was not found in {}",
            migrations_dir.display()
        )));
    };
    let new_checksum = migration_checksum(up_sql);
    with_migration_connection!(database_url, |conn| {
        let is_applied =
            !diesel::sql_query("SELECT version FROM __diesel_schema_migrations WHERE version = $1")
                .bind::<diesel::sql_types::Text, _>(version)
                .load::<AppliedMigrationVersion>(conn)
                .map_err(|e| MigrationError::Migration(e.to_string()))?
                .is_empty();
        if !is_applied {
            return Err(MigrationError::Migration(format!(
                "cannot re-baseline {version}: it is not a currently applied migration"
            )));
        }
        rebaseline_checksum(conn, version, &new_checksum)
    })
}

/// [`record_checksums_from_dir`], serialized under the migration advisory lock.
///
/// This is the primitive `autumn migrate baseline` uses: unlike the bare
/// [`record_checksums_from_dir`] (which is called by `autumn migrate run`
/// *while it already holds* [`hold_migration_lock`] on a separate session, so
/// re-locking there would self-deadlock), baseline runs standalone and must
/// take the lock itself. It mirrors [`revert_user_migrations_locked`] exactly:
/// acquire the lock, then read the applied set **and** record checksums on the
/// *same* session inside the critical section, then release. Holding the lock
/// across the read+write is what prevents a concurrent `autumn migrate down`
/// from reverting a version between baseline's applied-versions read and its
/// checksum write, which would otherwise let baseline re-insert a checksum row
/// for a version that is no longer applied (issue #1203 review).
///
/// Pass `wait_timeout = None` to use [`DEFAULT_LOCK_WAIT_TIMEOUT`] (60 s).
///
/// # Errors
///
/// * [`MigrationError::Connection`] — the database is unreachable.
/// * [`MigrationError::LockTimeout`] — the advisory lock cannot be acquired
///   within `wait_timeout`.
/// * [`MigrationError::Migration`] — the migrations dir cannot be read or a
///   database error occurred.
pub fn record_checksums_from_dir_locked(
    database_url: &str,
    migrations_dir: &Path,
    wait_timeout: Option<std::time::Duration>,
) -> Result<usize, MigrationError> {
    let up_by_version = read_up_sql_by_version(migrations_dir)?;
    let timeout = wait_timeout.unwrap_or(DEFAULT_LOCK_WAIT_TIMEOUT);
    with_migration_connection!(database_url, |conn| {
        acquire_migration_lock_on(conn, timeout)?;

        // The applied-versions READ and the checksum WRITE both run here, under
        // the advisory lock on THIS session, so a concurrent `down` cannot
        // revert a version between them. Always release the lock afterwards.
        let result: Result<usize, MigrationError> = (|| {
            let applied = load_applied_versions_lenient(conn)?;
            record_checksums(conn, &applied, &up_by_version)
        })();

        release_migration_lock_on(conn);
        result
    })
}

/// [`rebaseline_checksum_from_dir`], serialized under the migration advisory
/// lock — the primitive behind `autumn migrate baseline --force <version>`.
///
/// See [`record_checksums_from_dir_locked`] for why baseline must take the lock
/// itself. The applied-check for `version` and the overwrite both run on the
/// *same* locked session so a concurrent `autumn migrate down` cannot revert
/// `version` between the "is it applied?" probe and the write.
///
/// Pass `wait_timeout = None` to use [`DEFAULT_LOCK_WAIT_TIMEOUT`] (60 s).
///
/// # Errors
///
/// * [`MigrationError::Connection`] — the database is unreachable.
/// * [`MigrationError::LockTimeout`] — the advisory lock cannot be acquired
///   within `wait_timeout`.
/// * [`MigrationError::Migration`] — the version isn't currently applied, its
///   on-disk `up.sql` is unreadable, or a database error occurred.
pub fn rebaseline_checksum_from_dir_locked(
    database_url: &str,
    migrations_dir: &Path,
    version: &str,
    wait_timeout: Option<std::time::Duration>,
) -> Result<(), MigrationError> {
    let up_by_version = read_up_sql_by_version(migrations_dir)?;
    let Some(up_sql) = up_by_version.get(version) else {
        return Err(MigrationError::Migration(format!(
            "cannot re-baseline {version}: its up.sql was not found in {}",
            migrations_dir.display()
        )));
    };
    let new_checksum = migration_checksum(up_sql);
    let timeout = wait_timeout.unwrap_or(DEFAULT_LOCK_WAIT_TIMEOUT);
    with_migration_connection!(database_url, |conn| {
        acquire_migration_lock_on(conn, timeout)?;

        // The "is `version` currently applied?" READ and the overwrite WRITE
        // both run here, under the advisory lock on THIS session. Always
        // release the lock afterwards.
        let result: Result<(), MigrationError> = (|| {
            let is_applied = !diesel::sql_query(
                "SELECT version FROM __diesel_schema_migrations WHERE version = $1",
            )
            .bind::<diesel::sql_types::Text, _>(version)
            .load::<AppliedMigrationVersion>(conn)
            .map_err(|e| MigrationError::Migration(e.to_string()))?
            .is_empty();
            if !is_applied {
                return Err(MigrationError::Migration(format!(
                    "cannot re-baseline {version}: it is not a currently applied migration"
                )));
            }
            rebaseline_checksum(conn, version, &new_checksum)
        })();

        release_migration_lock_on(conn);
        result
    })
}

/// The recorded-vs-actual state of every applied migration, for status
/// display. Returns `(version, state)` pairs in the same order as
/// `__diesel_schema_migrations` (ascending by version).
///
/// # Errors
///
/// * [`MigrationError::Connection`] — the database is unreachable.
/// * [`MigrationError::Migration`] — the migrations dir cannot be read or a
///   database error occurred.
pub fn checksum_status(
    database_url: &str,
    migrations_dir: &Path,
) -> Result<Vec<(String, ChecksumState)>, MigrationError> {
    let up_by_version = read_up_sql_by_version(migrations_dir)?;
    let framework = framework_migration_versions()?;
    with_migration_connection!(database_url, |conn| {
        let recorded = recorded_checksums(conn)?;
        let applied = load_applied_versions_lenient(conn)?;
        // Framework-owned versions never record a checksum against the user dir
        // and their up.sql is not in `migrations_dir`, so classifying them would
        // report `Unrecorded` and prompt `baseline` — which cannot record them.
        // Exclude them exactly as rollback does before classifying the rest.
        let user_applied = user_applied_versions(&applied, &up_by_version, &framework);
        Ok(classify(&user_applied, &up_by_version, &recorded))
    })
}

/// Filter framework-owned versions out of the applied set before checksum
/// classification, using the SAME definition rollback uses
/// ([`framework_migration_versions`]): a version is excluded only when it is
/// framework-owned **and** absent from the local dir. Local presence wins, so a
/// user migration colliding with a framework shim version is still classified,
/// and an applied user version absent from disk (a genuine `Missing`/
/// `Unrecorded` problem) still surfaces — the filter keys on framework-set
/// membership, never on "absent from the user dir".
///
/// This mirrors the rollback filter in [`classify_applied_user_migrations`]
/// (`by_version.contains_key(v) || !framework.contains(v)`) so status and
/// rollback share one definition of "framework-owned".
fn user_applied_versions<S>(
    applied: &[String],
    up_sql_by_version: &HashMap<String, String, S>,
    framework: &std::collections::BTreeSet<String>,
) -> Vec<String>
where
    S: std::hash::BuildHasher,
{
    applied
        .iter()
        .filter(|v| up_sql_by_version.contains_key(*v) || !framework.contains(*v))
        .cloned()
        .collect()
}

/// Classify the database's applied migrations into user migrations (ascending
/// by version), excluding framework-owned ones and resolving each to its local
/// directory via Diesel's `name()`/`version()` metadata.
///
/// Generic over the connection so it works with both the native
/// `PgConnection` and the rustls migration wrapper (see
/// [`crate::db::MigrationConnection`]).
fn resolve_applied_user_migrations<C: MigrationHarness<Pg>>(
    conn: &mut C,
    all_migrations: &[Box<dyn Migration<Pg>>],
    migrations_dir: &Path,
) -> Result<Vec<AppliedUserMigration>, MigrationError> {
    // version -> local directory name, using Diesel's normalisation so that
    // hyphenated directories (e.g. `2026-01-01-000000_x`) match the applied
    // version (`20260101000000`).
    let by_version: std::collections::BTreeMap<String, String> = all_migrations
        .iter()
        .map(|m| (m.name().version().to_string(), m.name().to_string()))
        .collect();

    let framework = framework_migration_versions()?;

    let applied: Vec<String> = conn
        .applied_migrations()
        .map_err(|e| MigrationError::Migration(e.to_string()))?
        .iter()
        .map(ToString::to_string)
        .collect();

    Ok(classify_applied_user_migrations(
        &applied,
        &by_version,
        &framework,
        migrations_dir,
    ))
}

/// Pure classification of applied versions into user migrations (ascending by
/// version), separated from DB/IO so it can be unit-tested.
///
/// `by_version` maps a normalised migration version to its local directory name
/// (from the file-based source). `framework` is the embedded framework version
/// set. A version is treated as a **user** migration when it is present locally
/// (`by_version`) — local presence wins over a framework-version collision — or
/// when it is neither local nor framework-owned (applied but missing locally,
/// returned with `dir: None` so callers can surface it).
fn classify_applied_user_migrations(
    applied: &[String],
    by_version: &std::collections::BTreeMap<String, String>,
    framework: &std::collections::BTreeSet<String>,
    migrations_dir: &Path,
) -> Vec<AppliedUserMigration> {
    let mut user: Vec<AppliedUserMigration> = applied
        .iter()
        // Local presence wins: a version present in `migrations_dir` is a user
        // migration even if it collides with a framework shim version (e.g. the
        // placeholder `00000000000000` shared by `create_api_tokens` and some
        // apps' first migration). Only framework-owned versions that are absent
        // locally are excluded.
        .filter(|v| by_version.contains_key(*v) || !framework.contains(*v))
        .map(|version| {
            by_version.get(version).map_or_else(
                || AppliedUserMigration {
                    name: version.clone(),
                    dir: None,
                    version: version.clone(),
                },
                |name| AppliedUserMigration {
                    dir: Some(migrations_dir.join(name)),
                    name: name.clone(),
                    version: version.clone(),
                },
            )
        })
        .collect();
    user.sort_by(|a, b| a.version.cmp(&b.version));
    user
}

/// Return the applied **user** migrations (ascending by version), excluding any
/// framework-owned migrations, each resolved to its local directory.
///
/// Framework migrations are excluded by version (the embedded
/// `FRAMEWORK_MIGRATIONS` set), except where a version is also present in the
/// local `migrations_dir` — local presence wins so a user migration that
/// collides with a framework shim version is not dropped. An applied user
/// migration that is no longer present locally is still returned, with
/// [`AppliedUserMigration::dir`] set to `None`, so callers can surface it rather
/// than silently dropping it.
///
/// This is a read-only listing for status display; it does **not** take the
/// migration advisory lock. Use [`revert_user_migrations_locked`] to plan and
/// execute a rollback atomically under the lock.
///
/// # Errors
///
/// - [`MigrationError::Connection`] if the database is unreachable.
/// - [`MigrationError::Migration`] if `migrations_dir` cannot be read or if
///   querying applied versions from the database fails.
pub fn applied_user_migrations(
    database_url: &str,
    migrations_dir: &Path,
) -> Result<Vec<AppliedUserMigration>, MigrationError> {
    // TLS-aware: honors the URL's sslmode (`autumn migrate status` rollback
    // availability must work against TLS-only servers too).
    with_migration_connection!(database_url, |conn| {
        let source = FileBasedMigrations::from_path(migrations_dir).map_err(|e| {
            MigrationError::Migration(format!("failed to read migrations dir: {e}"))
        })?;
        let all_migrations: Vec<Box<dyn Migration<Pg>>> = source
            .migrations()
            .map_err(|e| MigrationError::Migration(e.to_string()))?;

        resolve_applied_user_migrations(conn, &all_migrations, migrations_dir)
    })
}

/// Plan and execute a user-migration rollback atomically under the migration
/// advisory lock.
///
/// After acquiring the lock, the applied user migrations are listed and
/// resolved (framework migrations excluded), then `plan` is invoked to choose
/// the versions to revert (newest-first). Because listing, planning, and
/// reverting all happen while the lock is held, the plan cannot go stale: two
/// concurrent `down` runs are fully serialized, so neither double-reverts.
///
/// `plan` may inspect each [`AppliedUserMigration`] (including whether it is
/// resolvable locally) and return an error — or terminate the process — to
/// refuse the rollback. `on_reverted` is invoked after each successful revert
/// so the caller can stream per-migration UX. Returns the number reverted.
///
/// If a planned version is applied but missing from `migrations_dir`, the
/// revert fails (rather than skipping it) because its `down.sql` is unavailable.
///
/// # Errors
///
/// - [`MigrationError::Connection`] if the database is unreachable.
/// - [`MigrationError::LockTimeout`] if the advisory lock cannot be acquired.
/// - [`MigrationError::Migration`] if `plan` returns an error, a revert fails,
///   or a planned version is not present in `migrations_dir`.
pub fn revert_user_migrations_locked<P, F>(
    database_url: &str,
    migrations_dir: &Path,
    wait_timeout: Option<std::time::Duration>,
    plan: P,
    mut on_reverted: F,
) -> Result<usize, MigrationError>
where
    P: FnOnce(&[AppliedUserMigration]) -> Result<Vec<String>, MigrationError> + Send,
    F: FnMut(&RevertedMigration) + Send,
{
    let timeout = wait_timeout.unwrap_or(DEFAULT_LOCK_WAIT_TIMEOUT);

    // TLS-aware: honors the URL's sslmode (`autumn migrate down` must work
    // against TLS-only servers too).
    with_migration_connection!(database_url, |conn| {
        let source = FileBasedMigrations::from_path(migrations_dir).map_err(|e| {
            MigrationError::Migration(format!("failed to read migrations dir: {e}"))
        })?;
        let all_migrations: Vec<Box<dyn Migration<Pg>>> = source
            .migrations()
            .map_err(|e| MigrationError::Migration(e.to_string()))?;

        acquire_migration_lock_on(conn, timeout)?;

        let result: Result<usize, MigrationError> = (|| {
            let applied_user =
                resolve_applied_user_migrations(&mut *conn, &all_migrations, migrations_dir)?;
            let versions = plan(&applied_user)?;

            let mut count = 0;
            for version in &versions {
                // Build a borrowed `MigrationVersion` once per version (no heap
                // allocation) instead of allocating a `String` for every migration.
                let target = diesel::migration::MigrationVersion::from(version.as_str());
                let migration = all_migrations
                    .iter()
                    .find(|m| m.name().version() == target)
                    .ok_or_else(|| {
                        MigrationError::Migration(format!(
                            "migration version {version} is applied but not present in {} — \
                             cannot revert (its down.sql is unavailable)",
                            migrations_dir.display()
                        ))
                    })?;

                let started = std::time::Instant::now();
                conn.revert_migration(migration.as_ref())
                    .map_err(|e| MigrationError::Migration(e.to_string()))?;
                let duration = started.elapsed();

                // This version is no longer applied, so its recorded checksum row
                // must go: reverting exactly this migration removes it from
                // `__diesel_schema_migrations`, and `version` is the key the row
                // was recorded under. Deleting it restores the "row exists iff
                // version applied with matching bytes" invariant, so a later
                // re-apply of an edited `up.sql` records the new hash instead of
                // leaving the stale one (#1203 review). Best-effort: the schema
                // revert has already committed, so a failed delete only warns, and
                // a later `autumn migrate baseline` or re-apply reconciles it.
                if let Err(e) = delete_checksum(&mut *conn, version) {
                    tracing::warn!(
                        version = %version,
                        error = %e,
                        "Rolled back migration but could not clear its recorded content \
                         checksum; a later migrate may report drift for this version until \
                         it is re-applied or re-baselined"
                    );
                }

                on_reverted(&RevertedMigration {
                    version: version.clone(),
                    name: migration.name().to_string(),
                    duration,
                });
                count += 1;
            }
            Ok(count)
        })();

        release_migration_lock_on(conn);

        result
    })
}

// ── SQLite migrate up/down (issue #2058) ─────────────────────────────────────

/// Backend-generic core of [`resolve_applied_user_migrations`], parameterized
/// over the diesel backend so the Postgres and `SQLite` `MigrationHarness`
/// connections share one classification path.
///
/// The version normalisation, framework-exclusion, and local-directory
/// resolution are backend-independent; only the connection's `applied_migrations`
/// call and the migration boxes' backend differ. The pure
/// [`classify_applied_user_migrations`] does the rest.
#[cfg(feature = "sqlite")]
fn resolve_applied_user_migrations_sqlite<C>(
    conn: &mut C,
    all_migrations: &[Box<dyn Migration<diesel::sqlite::Sqlite>>],
    migrations_dir: &Path,
) -> Result<Vec<AppliedUserMigration>, MigrationError>
where
    C: MigrationHarness<diesel::sqlite::Sqlite>,
{
    let by_version: std::collections::BTreeMap<String, String> = all_migrations
        .iter()
        .map(|m| (m.name().version().to_string(), m.name().to_string()))
        .collect();

    // The framework version strings are backend-independent; enumerate them via
    // the `Sqlite` source so a framework version that somehow landed in
    // `__diesel_schema_migrations` is still excluded from user rollback planning.
    let framework = framework_migration_versions_for::<diesel::sqlite::Sqlite>()?;

    let applied: Vec<String> = conn
        .applied_migrations()
        .map_err(|e| MigrationError::Migration(e.to_string()))?
        .iter()
        .map(ToString::to_string)
        .collect();

    Ok(classify_applied_user_migrations(
        &applied,
        &by_version,
        &framework,
        migrations_dir,
    ))
}

/// `SQLite` counterpart to [`applied_user_migrations`]: return the applied
/// **user** migrations (ascending by version), each resolved to its local
/// directory, from a `SQLite` database.
///
/// `SQLite` is a single-writer local database, so — unlike the Postgres path —
/// there is **no advisory lock** (issue #1999 / #2036 precedent); the read runs
/// directly on a synchronous `SqliteConnection`. Read-only status listing used by
/// the `autumn migrate down` preflight on a `sqlite://` target.
///
/// # Errors
///
/// - [`MigrationError::Connection`] if the database cannot be opened.
/// - [`MigrationError::Migration`] if `migrations_dir` cannot be read or querying
///   applied versions fails.
#[cfg(feature = "sqlite")]
pub fn applied_user_migrations_sqlite(
    database_url: &str,
    migrations_dir: &Path,
) -> Result<Vec<AppliedUserMigration>, MigrationError> {
    let mut conn = crate::db::establish_sqlite_migration_connection(database_url)
        .map_err(|e| MigrationError::Connection(e.to_string()))?;
    let source = FileBasedMigrations::from_path(migrations_dir)
        .map_err(|e| MigrationError::Migration(format!("failed to read migrations dir: {e}")))?;
    let all_migrations: Vec<Box<dyn Migration<diesel::sqlite::Sqlite>>> = source
        .migrations()
        .map_err(|e| MigrationError::Migration(e.to_string()))?;
    resolve_applied_user_migrations_sqlite(&mut conn, &all_migrations, migrations_dir)
}

/// `SQLite` counterpart to [`revert_user_migrations_locked`]: plan and execute a
/// user-migration rollback against a `SQLite` database.
///
/// The applied set is listed, `plan` chooses the newest-first versions to revert,
/// and each is reverted through diesel's `MigrationHarness` — the whole
/// list→plan→revert sequence held under the shared [`with_sqlite_migration_lock`]
/// write lock (issue #2065, deferred from PR #2062) so a concurrent migrator
/// cannot interleave and re-revert an already-reverted `down.sql`. There is
/// deliberately **no Postgres advisory lock** on this path (issue #1999 / #2036
/// precedent) — `SQLite` has no `pg_advisory_lock` primitive; the on-disk write
/// lock is the whole cross-process serialization mechanism. There is likewise
/// **no content-checksum bookkeeping** — the
/// `SQLite` `autumn migrate up` path applies through the unlocked harness and
/// records no `autumn_migration_checksums` rows (that table's DDL is
/// Postgres-specific), so there is nothing to delete on revert.
///
/// `plan` may inspect each [`AppliedUserMigration`] and return an error (or
/// terminate the process) to refuse the rollback; `on_reverted` streams
/// per-migration UX. Returns the number reverted.
///
/// # Errors
///
/// - [`MigrationError::Connection`] if the database cannot be opened.
/// - [`MigrationError::Migration`] if `plan` returns an error, a revert fails, or
///   a planned version is not present in `migrations_dir`.
#[cfg(feature = "sqlite")]
pub fn revert_user_migrations_sqlite<P, F>(
    database_url: &str,
    migrations_dir: &Path,
    plan: P,
    mut on_reverted: F,
) -> Result<usize, MigrationError>
where
    P: FnOnce(&[AppliedUserMigration]) -> Result<Vec<String>, MigrationError> + Send,
    F: FnMut(&RevertedMigration) + Send,
{
    let mut conn = crate::db::establish_sqlite_migration_connection(database_url)
        .map_err(|e| MigrationError::Connection(e.to_string()))?;
    let source = FileBasedMigrations::from_path(migrations_dir)
        .map_err(|e| MigrationError::Migration(format!("failed to read migrations dir: {e}")))?;
    let all_migrations: Vec<Box<dyn Migration<diesel::sqlite::Sqlite>>> = source
        .migrations()
        .map_err(|e| MigrationError::Migration(e.to_string()))?;

    with_sqlite_migration_lock(&mut conn, |conn| {
        let applied_user =
            resolve_applied_user_migrations_sqlite(conn, &all_migrations, migrations_dir)?;
        let versions = plan(&applied_user)?;

        let mut count = 0;
        for version in &versions {
            let target = diesel::migration::MigrationVersion::from(version.as_str());
            let migration = all_migrations
                .iter()
                .find(|m| m.name().version() == target)
                .ok_or_else(|| {
                    MigrationError::Migration(format!(
                        "migration version {version} is applied but not present in {} — \
                         cannot revert (its down.sql is unavailable)",
                        migrations_dir.display()
                    ))
                })?;

            let started = std::time::Instant::now();
            conn.revert_migration(migration.as_ref())
                .map_err(|e| MigrationError::Migration(e.to_string()))?;
            let duration = started.elapsed();

            on_reverted(&RevertedMigration {
                version: version.clone(),
                name: migration.name().to_string(),
                duration,
            });
            count += 1;
        }
        Ok(count)
    })
}

// ── Startup wait-for-database ─────────────────────────────────────────────────

/// A single connect attempt returned by the injected `try_connect` closure.
#[derive(Debug)]
pub(crate) enum AttemptError {
    /// The server is not yet reachable (connection refused, starting up, …).
    /// Carries the raw error message so a timeout can include the last error.
    /// The caller will retry after a backoff delay.
    Retryable(String),
    /// A non-transient failure (auth error, bad URL, missing database, …).
    /// The caller must surface the error immediately without retrying.
    Fatal(String),
}

/// Classify an error message from `PgConnection::establish` as retryable or
/// fatal so the startup wait loop can decide whether to keep waiting.
///
/// The default is **fatal** (deny-list for retry) so that unknown errors always
/// fail fast rather than silently burning the whole startup-wait window.
/// Only "server not yet reachable" patterns are allowed to retry (AC #5).
pub(crate) fn is_retryable_connection_error(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    lower.contains("connection refused")
        || lower.contains("could not connect to server")
        || lower.contains("the database system is starting up")
        || lower.contains("connection reset")
        || lower.contains("no route to host")
        || lower.contains("host is unreachable")
        || lower.contains("network is unreachable")
        || lower.contains("timed out")
        // libpq reports a connect_timeout expiry as "timeout expired" (not
        // "timed out"), so firewalled hosts that silently drop packets are
        // also retryable rather than being mis-classified as fatal.
        || lower.contains("timeout expired")
        || lower.contains("connection closed")
        // DNS resolution failures — common in Docker Compose / Kubernetes
        // cold-start where the 'db' hostname resolves only after the DNS
        // service is ready (Linux/glibc, macOS, and generic forms).
        || lower.contains("name or service not known")
        || lower.contains("nodename nor servname provided")
        || lower.contains("failed to lookup")
        || lower.contains("temporary failure in name resolution")
}

/// Capped exponential backoff for the startup wait loop.
///
/// Returns `500ms * 2^(attempt - 1)`, capped at `5s`.
pub(crate) fn backoff_delay(attempt: u32) -> std::time::Duration {
    let ms = 500u64.saturating_mul(1u64 << (attempt.saturating_sub(1).min(10)));
    std::time::Duration::from_millis(ms.min(5_000))
}

/// Redact the password from any `postgres://` or `postgresql://` URL embedded
/// in `msg`.
///
/// Replaces the password component with `****`, mirroring the approach used
/// by `mask_database_url` in `autumn/src/app.rs`.  When the URL token cannot
/// be parsed, the entire token is replaced with `****` as a safe fallback so
/// a malformed-but-credential-bearing string is never surfaced (also matches
/// `mask_database_url`'s parse-failure behaviour).  Leaves the rest of the
/// message unchanged.
pub(crate) fn redact_db_url_credentials(msg: &str) -> String {
    let mut out = String::with_capacity(msg.len());
    let mut rest = msg;
    loop {
        // Find the leftmost postgres:// or postgresql:// occurrence.
        let pg = rest.find("postgres://");
        let pgl = rest.find("postgresql://");
        let start = match (pg, pgl) {
            (None, None) => {
                out.push_str(rest);
                break;
            }
            (Some(a), None) => a,
            (None, Some(b)) => b,
            (Some(a), Some(b)) => a.min(b),
        };
        // Push everything before the URL token unchanged.
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        // Extract the URL-shaped token (everything until first whitespace or EOS).
        let token_end = rest
            .find(|c: char| c.is_ascii_whitespace())
            .unwrap_or(rest.len());
        let token = &rest[..token_end];
        if let Ok(mut parsed) = url::Url::parse(token) {
            if parsed.password().is_some() {
                let _ = parsed.set_password(Some("****"));
                out.push_str(parsed.as_str());
            } else {
                out.push_str(token);
            }
        } else {
            // Parse failed — mask the whole token rather than risk leaking a
            // malformed credential-bearing URL.
            out.push_str("****");
        }
        rest = &rest[token_end..];
    }
    out
}

/// Inner (dependency-injected) startup wait loop.
///
/// All I/O is supplied via closures so the logic is unit-testable without a
/// real Postgres instance or wall-clock sleeps (AC #2).
///
/// # Arguments
///
/// * `max_wait` — maximum total time to spend waiting (> 0 enforced by callers)
/// * `try_connect` — attempts one connection; returns `Ok(())` on success,
///   `Err(AttemptError::Retryable(msg))` for transient errors (the message is
///   included in the timeout error), or `Err(AttemptError::Fatal(_))` for
///   non-transient failures
/// * `sleep` — called with the computed backoff delay; may be a no-op in tests
/// * `elapsed` — returns the total time elapsed since the wait started
/// * `on_retry` — called **before** sleeping with `(attempt, next_delay)` so
///   the caller can print a user-visible retry message (AC #4)
pub(crate) fn wait_for_database_inner(
    max_wait: std::time::Duration,
    mut try_connect: impl FnMut() -> Result<(), AttemptError>,
    mut sleep: impl FnMut(std::time::Duration),
    elapsed: impl Fn() -> std::time::Duration,
    mut on_retry: impl FnMut(u32, std::time::Duration),
) -> Result<(), MigrationError> {
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        // Extract the transient error message or return immediately for Ok/Fatal.
        let retryable_msg = match try_connect() {
            Ok(()) => return Ok(()),
            Err(AttemptError::Fatal(msg)) => {
                return Err(MigrationError::Connection(redact_db_url_credentials(&msg)));
            }
            Err(AttemptError::Retryable(msg)) => msg,
        };
        let elapsed_now = elapsed();
        if elapsed_now >= max_wait {
            return Err(MigrationError::Connection(format!(
                "database did not become reachable within {}s after {} attempt(s); \
                 timed out waiting for startup (last error: {})",
                max_wait.as_secs(),
                attempt,
                redact_db_url_credentials(&retryable_msg),
            )));
        }
        let delay = backoff_delay(attempt).min(max_wait.saturating_sub(elapsed_now));
        on_retry(attempt, delay);
        sleep(delay);
        // Guard against sleep overshoot: if the deadline has now passed, don't
        // start another connection attempt (which could block for a full
        // per-attempt connect_timeout before detecting the expiry).
        if elapsed() >= max_wait {
            return Err(MigrationError::Connection(format!(
                "database did not become reachable within {}s after {} attempt(s); \
                 timed out waiting for startup (last error: {})",
                max_wait.as_secs(),
                attempt,
                redact_db_url_credentials(&retryable_msg),
            )));
        }
    }
}

fn with_connect_timeout(url: &str, timeout_secs: u64) -> String {
    if crate::pg_conn_str::is_url(url) {
        // Splice `connect_timeout` directly into the raw percent-encoded query
        // string so existing parameters (e.g. `options=-c%20search_path%3Dapp`)
        // are preserved byte-for-byte.  Using query_pairs() / append_pair() would
        // decode and re-encode them via form encoding (spaces → `+`), which libpq
        // does not accept.
        return url::Url::parse(url).map_or_else(
            |_| url.to_owned(),
            |mut parsed| {
                let pair = format!("connect_timeout={timeout_secs}");
                let raw = parsed
                    .query()
                    .unwrap_or("")
                    .split('&')
                    .filter(|p| !p.is_empty() && !p.starts_with("connect_timeout="))
                    .chain(std::iter::once(pair.as_str()))
                    .collect::<Vec<_>>()
                    .join("&");
                parsed.set_query(Some(&raw));
                parsed.to_string()
            },
        );
    }
    // Keyword/value form (accepted by config validation since the keyword
    // parser landed): without this, a `--wait` attempt against a blackholed
    // host gets no per-attempt connect_timeout and a single hung connect can
    // outlive the whole wait budget. Append the parameter — unless the user
    // already set their own connect_timeout, which is respected as-is.
    match crate::pg_conn_str::keyword_value_pairs(url) {
        Some(pairs) if !pairs.iter().any(|(key, _)| key == "connect_timeout") => {
            format!("{url} connect_timeout={timeout_secs}")
        }
        // User-provided timeout, or a string tokio-postgres itself would
        // reject: pass through untouched so connect reports its own error.
        _ => url.to_owned(),
    }
}

/// Wait for the database at `database_url` to accept connections, retrying
/// with capped exponential backoff until either success or `max_wait` elapses.
///
/// * `max_wait == Duration::ZERO` — callers **must not** call this function;
///   skip it and connect directly (preserves today's fail-fast behaviour).
/// * Non-retryable errors (auth failures, bad URL, missing database) are
///   surfaced immediately regardless of `max_wait`.
/// * Connection credentials are never included in retry log output.
///
/// `on_retry` is called **before** each sleep with `(attempt, next_delay)` so
/// the CLI can print a visible message with attempt count and next delay.
///
/// # Errors
///
/// Returns [`MigrationError::Connection`] when a fatal (non-retryable) error
/// is encountered or when `max_wait` elapses without a successful connection.
pub fn wait_for_database(
    database_url: &str,
    max_wait: std::time::Duration,
    mut on_retry: impl FnMut(u32, std::time::Duration),
) -> Result<(), MigrationError> {
    let start = std::time::Instant::now();
    wait_for_database_inner(
        max_wait,
        || {
            // Cap the per-attempt connect_timeout to the remaining budget so
            // a single hung establish() call cannot extend the total wait beyond
            // max_wait (e.g. a DROP-firewalled host that accepts the SYN but
            // never completes the handshake).  libpq's connect_timeout is in
            // whole seconds; clamp to at least 1.
            let remaining = max_wait.saturating_sub(start.elapsed());
            let connect_timeout_secs = remaining.as_secs().max(1);
            let timed_url = with_connect_timeout(database_url, connect_timeout_secs);
            // TLS-aware: honors the URL's sslmode (the bundled libpq has no
            // SSL support, so waiting on a TLS-only server through the
            // native path would never succeed).
            crate::db::establish_migration_connection(&timed_url)
                .map(|_conn| ())
                .map_err(|e| {
                    let msg = e.to_string();
                    if is_retryable_connection_error(&msg) {
                        AttemptError::Retryable(msg)
                    } else {
                        AttemptError::Fatal(msg)
                    }
                })
        },
        std::thread::sleep,
        move || start.elapsed(),
        |attempt, delay| {
            tracing::warn!(
                attempt,
                next_delay_ms = delay.as_millis(),
                "Database not reachable; retrying after backoff",
            );
            on_retry(attempt, delay);
        },
    )
}

/// Open a new Postgres connection and acquire the migration advisory lock,
/// returning a [`MigrationLockGuard`] that releases it on drop.
///
/// This is the right primitive when migrations are run by an external process
/// (e.g. the `diesel` CLI subprocess in `autumn migrate run`): the guard keeps
/// the lock connection alive for the duration of the external run.
///
/// Use [`run_pending_locked`] when the Rust harness runs migrations directly.
///
/// # Errors
///
/// Returns [`MigrationError::Connection`] if the database is unreachable, or
/// [`MigrationError::LockTimeout`] if the lock cannot be acquired within
/// `wait_timeout`.
pub fn hold_migration_lock(
    database_url: &str,
    wait_timeout: std::time::Duration,
) -> Result<MigrationLockGuard, MigrationError> {
    // TLS-aware: honors the URL's sslmode, exactly like `run_pending_locked`
    // (see `crate::db::establish_migration_connection`) — this lock is taken
    // before spawning the external diesel CLI, so it must reach TLS-only
    // servers too.
    let mut conn = crate::db::establish_migration_connection(database_url)
        .map_err(|e| MigrationError::Connection(e.to_string()))?;

    match &mut conn {
        crate::db::MigrationConnection::Native(conn) => {
            acquire_migration_lock_on(conn, wait_timeout)?;
        }
        crate::db::MigrationConnection::Rustls { conn, .. } => {
            acquire_migration_lock_on(conn, wait_timeout)?;
        }
    }

    Ok(MigrationLockGuard { conn })
}

/// Run all pending migrations under a Postgres advisory lock.
///
/// Serializes concurrent migration attempts across processes: exactly one
/// process applies pending migrations while the rest wait, find no pending
/// work, and return a [`MigrationResult`] with an empty `applied` list.
///
/// The lock is acquired **before** the pending-migration list is read,
/// closing the check-then-apply race. It is released after the harness
/// commits or rolls back all migrations.
///
/// Pass `wait_timeout = None` to use [`DEFAULT_LOCK_WAIT_TIMEOUT`] (60 s).
///
/// # Non-`PostgreSQL` note
///
/// Advisory locks are `PostgreSQL`-specific. For `SQLite` or in-memory test
/// harnesses call [`run_pending`] directly — those backends are single-process
/// and do not require cross-process serialization.
///
/// # Errors
///
/// Returns [`MigrationError::Connection`] if the database is unreachable,
/// [`MigrationError::LockTimeout`] if the advisory lock cannot be acquired
/// within `wait_timeout`, or [`MigrationError::Migration`] if a migration
/// fails to apply.
pub fn run_pending_locked(
    database_url: &str,
    migrations: impl diesel::migration::MigrationSource<diesel::pg::Pg> + Send,
    wait_timeout: Option<std::time::Duration>,
) -> Result<MigrationResult, MigrationError> {
    run_pending_locked_inner(database_url, migrations, wait_timeout, None)
}

/// Shared engine for [`run_pending_locked`] that additionally performs
/// content-checksum validation **inside** the advisory-locked critical section
/// when `up_sql_by_version` is `Some` (issue #1203).
///
/// Passing the on-disk `up.sql` map runs, on the *same* Postgres session that
/// holds the advisory lock and in this order:
///
/// 1. Acquire the advisory lock.
/// 2. **Validate** every already-applied version's recorded checksum against
///    its current on-disk `up.sql` (fail fast on `Changed`/`Missing`). This is
///    the authoritative, race-free drift guard.
/// 3. **Apply** pending migrations.
/// 4. **Re-validate** after apply — belt-and-suspenders for the interleaving
///    where a sibling replica applied and recorded THIS version's *original*
///    content between our step 2 and our apply: the now-recorded hash is
///    compared against our edited on-disk `up.sql` so the mismatch is caught
///    before boot.
/// 5. Release the lock.
///
/// This path deliberately does **not** record checksums for the freshly-applied
/// versions. It applies the EMBEDDED migration set compiled into the binary,
/// whereas `up_sql_by_version` is read from the on-disk `./migrations/` dir; the
/// two can diverge (files edited/mounted after the build). Recording the disk
/// bytes here would store a hash for content that was never applied, so recording
/// is deferred to the CLI/baseline paths (`autumn migrate run` /
/// `autumn migrate baseline`), where the applied bytes ARE the on-disk bytes
/// (issue #1203 review). See the inline comment at step (4) for detail.
///
/// Holding the lock across steps 2–4 on one session is what closes the TOCTOU
/// race a naive pre-lock check leaves open under a concurrent rolling deploy:
/// with the check outside the lock, replica A (holding an edited `up.sql` for
/// an as-yet-unapplied version) can validate successfully, then a sibling
/// applies and records the original checksum, and A later finds nothing pending
/// and boots without ever comparing its edited content. Running the compare
/// under the lock removes every such interleaving.
///
/// `up_sql_by_version` is pre-read by the caller (best-effort), so a missing or
/// unreadable migrations dir simply yields `None` and disables the checksum
/// steps rather than failing the migration run.
fn run_pending_locked_inner(
    database_url: &str,
    migrations: impl diesel::migration::MigrationSource<diesel::pg::Pg> + Send,
    wait_timeout: Option<std::time::Duration>,
    up_sql_by_version: Option<&HashMap<String, String>>,
) -> Result<MigrationResult, MigrationError> {
    let timeout = wait_timeout.unwrap_or(DEFAULT_LOCK_WAIT_TIMEOUT);

    with_migration_connection!(database_url, |conn| {
        acquire_migration_lock_on(conn, timeout)?;

        // Everything from here runs under the advisory lock on THIS session, so
        // no concurrent runner can interleave between the validate, apply,
        // record, and re-validate steps. Compute the outcome, then always
        // release the lock (the immediately-invoked closure drops its borrow of
        // `conn` before the release below).
        let outcome: Result<MigrationResult, MigrationError> = (|| {
            // (2) Authoritative pre-apply validation: every already-applied
            //     version's recorded checksum vs its current on-disk up.sql.
            if let Some(up) = up_sql_by_version {
                let recorded = recorded_checksums(conn)?;
                let applied = load_applied_versions_lenient(conn)?;
                validate_checksums(&applied, up, &recorded)?;
            }

            // (3) Apply pending migrations. Collect names eagerly so the harness
            //     borrow on `conn` is dropped before the checksum steps reuse it.
            let applied: Vec<String> = {
                let mut harness = HarnessWithOutput::write_to_stdout(&mut *conn);
                harness
                    .run_pending_migrations(migrations)
                    .map(|applied| applied.iter().map(|m| format!("{m}")).collect())
                    .map_err(|e| MigrationError::Migration(e.to_string()))?
            };

            // (5) Post-apply re-validation — still under the lock, on the same
            //     session. It deliberately does not record checksums for the
            //     freshly-applied versions.
            //
            //     This path applies the embedded migration set compiled into the
            //     binary, but `up_sql_by_version` is read from the on-disk
            //     `./migrations/` dir. When the dir is not byte-identical to the
            //     embedded set — files edited or mounted after the binary was built
            //     — recording the disk bytes would store a hash for content that
            //     was never applied, since the DB holds the embedded schema,
            //     silently making the edited file canonical and defeating later
            //     drift checks. Diesel's `Migration` API does not expose each
            //     embedded migration's raw `up.sql` bytes, so we cannot hash what
            //     was applied. Validate only, and defer authoritative recording to
            //     the CLI and baseline paths (`autumn migrate run`, `autumn migrate
            //     baseline`), where the applied bytes are the on-disk bytes (#1203).
            //
            //     Re-validating catches the interleaving where a sibling replica
            //     applied and recorded this version's original content between our
            //     step 2 and our apply: that now-recorded hash is compared against
            //     our edited on-disk `up.sql`, so the mismatch still fails fast
            //     before boot.
            if let Some(up) = up_sql_by_version {
                let applied_versions = load_applied_versions_lenient(conn)?;
                let recorded = recorded_checksums(conn)?;
                validate_checksums(&applied_versions, up, &recorded)?;
            }

            Ok(MigrationResult { applied })
        })();

        release_migration_lock_on(conn);
        outcome
    })
}

/// Apply the framework migrations required on every **shard** target.
///
/// Shard databases hold tenant data and must have the version-history and
/// commit-hook queue tables, but do **not** host the full control-plane schema
/// (API tokens, sessions, job queues, etc.). This function applies only those
/// two migration sets under the migration advisory lock.
///
/// Called by `autumn migrate` when iterating over `[[database.shards]]`
/// entries, in contrast to [`run_pending`] with [`FRAMEWORK_MIGRATIONS`]
/// which is used for the control database.
///
/// Like [`run_pending`] (the control-database path), this does **not** acquire
/// the migration advisory lock itself: the caller (`autumn migrate`) already
/// holds it via [`hold_migration_lock`] for the whole target. Re-acquiring the
/// session-level advisory lock here on a fresh connection would block on the
/// caller's own lock until timeout.
///
/// # Errors
///
/// Returns [`MigrationError::Connection`] if the database is unreachable,
/// or [`MigrationError::Migration`] if a migration fails to apply.
pub fn run_pending_shard_framework_migrations(
    database_url: &str,
) -> Result<MigrationResult, MigrationError> {
    #[cfg(feature = "db")]
    {
        let mut applied: Vec<String> = Vec::new();

        let vh_result = run_pending(
            database_url,
            EmbeddedMigrationsRef(&crate::version_history::VERSION_HISTORY_MIGRATIONS),
        )?;
        applied.extend(vh_result.applied);

        let ch_result = run_pending(
            database_url,
            EmbeddedMigrationsRef(
                &crate::repository_commit_hooks::REPOSITORY_COMMIT_HOOK_MIGRATIONS,
            ),
        )?;
        applied.extend(ch_result.applied);

        Ok(MigrationResult { applied })
    }
    #[cfg(not(feature = "db"))]
    {
        let _ = database_url;
        Ok(MigrationResult {
            applied: Vec::new(),
        })
    }
}

/// Names of pending shard-required framework migrations (version-history +
/// commit-hook queue) on `database_url`.
///
/// The status counterpart to [`run_pending_shard_framework_migrations`]: used
/// by `autumn migrate status --shard ...` so a shard reports only the framework
/// migrations it actually requires, not the full control-plane
/// [`FRAMEWORK_MIGRATIONS`] set (which would otherwise always show as pending on
/// a shard).
///
/// # Errors
///
/// Returns [`MigrationError::Connection`] if the database is unreachable, or
/// [`MigrationError::Migration`] if status cannot be determined.
pub fn pending_shard_framework_migrations(
    database_url: &str,
) -> Result<Vec<String>, MigrationError> {
    #[cfg(feature = "db")]
    {
        let mut pending: Vec<String> = Vec::new();
        pending.extend(pending_migrations(
            database_url,
            EmbeddedMigrationsRef(&crate::version_history::VERSION_HISTORY_MIGRATIONS),
        )?);
        pending.extend(pending_migrations(
            database_url,
            EmbeddedMigrationsRef(
                &crate::repository_commit_hooks::REPOSITORY_COMMIT_HOOK_MIGRATIONS,
            ),
        )?);
        Ok(pending)
    }
    #[cfg(not(feature = "db"))]
    {
        let _ = database_url;
        Ok(Vec::new())
    }
}

/// Decide whether pending migrations are auto-applied at startup (issue #1903).
///
/// Profile-agnostic, convention-over-configuration:
///
/// 1. An explicit `database.auto_migrate` (`auto_migrate = Some(_)`) overrides
///    everything on **any** profile — `Some(true)` forces apply, `Some(false)`
///    forces report-only.
/// 2. Otherwise `dev`/`development` auto-applies by convention.
/// 3. Otherwise the back-compat `auto_migrate_in_production` alias enables
///    auto-apply on **any** non-`dev` profile — `prod`/`production` **and**
///    custom names like `fly`/`staging` (the previous name-gated check silently
///    skipped custom profiles, so their opt-in was ignored: the bug).
/// 4. Otherwise report-only.
fn should_auto_apply(
    profile: Option<&str>,
    auto_migrate: Option<bool>,
    auto_migrate_in_production: bool,
) -> bool {
    if let Some(explicit) = auto_migrate {
        return explicit;
    }
    let profile_name = profile.unwrap_or("none");
    if matches!(profile_name, "dev" | "development") {
        return true;
    }
    auto_migrate_in_production
}

/// Run migrations according to the active profile and migration policy
/// (decision by [`should_auto_apply`], issue #1903).
///
/// - **dev/development**: runs all pending migrations automatically and logs each one.
/// - **any other profile** (`prod`/`production` **and** custom names like
///   `fly`/`staging`): logs pending migrations unless auto-apply is explicitly
///   opted into via `auto_migrate = true` or the `auto_migrate_in_production`
///   alias.
/// - An explicit `auto_migrate = Some(false)` forces report-only on any profile.
///
/// `target` labels the database being migrated (`"control"` or
/// `"shard:<name>"`) so a failing target is unambiguous in sharded
/// deployments. Apply failures exit the process (fail fast): a
/// half-migrated fleet that boots is worse than a crashed deploy, and
/// already-migrated targets are skipped idempotently on retry.
///
/// Called internally by [`AppBuilder::run`](crate::app::AppBuilder::run)
/// when migrations are registered via `.migrations()`.
#[allow(clippy::cognitive_complexity)]
pub(crate) fn auto_migrate(
    database_url: &str,
    profile: Option<&str>,
    auto_migrate: Option<bool>,
    auto_migrate_in_production: bool,
    migrations: impl MigrationSource<Pg> + Send,
    target: &str,
) {
    let profile_name = profile.unwrap_or("none");
    let is_dev = matches!(profile_name, "dev" | "development");
    let should_auto_apply = should_auto_apply(profile, auto_migrate, auto_migrate_in_production);

    if should_auto_apply {
        if is_dev {
            tracing::info!(target = %target, "Development profile: running pending database migrations...");
        } else {
            // Non-dev auto-apply is always an explicit opt-in (issue #1903):
            // either `database.auto_migrate = true` or the
            // `auto_migrate_in_production` alias. Name the profile and the key
            // that enabled it so a custom-profile operator (`fly`, `staging`, …)
            // sees clearly that their opt-in was honored.
            let key = if auto_migrate.is_some() {
                "database.auto_migrate"
            } else {
                "database.auto_migrate_in_production"
            };
            tracing::warn!(
                profile = profile_name,
                key,
                target = %target,
                "Auto-migration is explicitly enabled for this profile; running pending database migrations"
            );
        }

        // Content-checksum drift guard (#1203). When the local `./migrations/` directory
        // is present, read every migration's on-disk `up.sql`, so the locked apply path
        // can validate already-applied versions against their recorded checksums under
        // the advisory lock and on the same Postgres session, fail fast on drift, then
        // re-validate after applying. Running the compare inside the lock, rather than in
        // a pre-lock check, is what makes the guard race-free under a concurrent rolling
        // deploy (see [`run_pending_locked_inner`]).
        //
        // This startup path validates only; it records no new checksums. It applies the
        // embedded migration set, which may not be byte-identical to the on-disk `up.sql`
        // read here, so recording the disk bytes would store a hash for content that was
        // never applied. Authoritative recording happens on the CLI and baseline paths
        // (`autumn migrate run`, `autumn migrate baseline`), where applied bytes are the
        // on-disk bytes.
        //
        // Best-effort read: production binaries typically ship without the source tree, so
        // an absent `./migrations/` is not an error — the map is `None`, the checksum steps
        // are skipped, and `autumn migrate` is the canonical strict apply path in prod. A
        // present-but-unreadable dir likewise degrades to `None` with a warning rather than
        // blocking boot; genuine drift on a readable dir still hard-fails, under the lock.
        let migrations_dir = std::path::Path::new("migrations");
        let up_by_version = if migrations_dir.is_dir() {
            match read_up_sql_by_version(migrations_dir) {
                Ok(map) => Some(map),
                Err(e) => {
                    tracing::warn!(error = %e, target = %target, "Could not read migrations dir for checksum validation; continuing without the drift guard");
                    None
                }
            }
        } else {
            None
        };

        match run_pending_locked_inner(database_url, migrations, None, up_by_version.as_ref()) {
            Ok(result) if result.applied.is_empty() => {
                tracing::info!(target = %target, "No pending migrations");
            }
            Ok(result) => {
                for name in &result.applied {
                    tracing::info!(migration = %name, target = %target, "Applied migration");
                }
                tracing::info!(
                    count = result.applied.len(),
                    target = %target,
                    "All pending migrations applied"
                );
            }
            // Hard-fail on genuine drift: an applied migration was either edited
            // ("checksum mismatch") or deleted/renamed ("up.sql is missing from
            // the source tree") after being applied. Both mean the deployed
            // schema silently forks from a fresh build. The validation now runs
            // under the advisory lock, so this is caught even in the rolling
            // deploy race a pre-lock check would miss.
            Err(MigrationError::Migration(msg))
                if msg.contains("checksum mismatch")
                    || msg.contains("up.sql is missing from the source tree") =>
            {
                tracing::error!(error = %msg, target = %target, "Applied migration has drifted from the source tree since it was applied");
                #[cfg(feature = "managed-pg")]
                crate::managed_pg::emergency_stop();
                std::process::exit(1);
            }
            Err(e) => {
                tracing::error!(error = %e, target = %target, "Failed to run migrations");
                // Aborting boot via `process::exit` skips `on_shutdown`; stop any
                // managed Postgres first so a bad migration doesn't orphan the
                // supervised child holding the data dir and port.
                #[cfg(feature = "managed-pg")]
                crate::managed_pg::emergency_stop();
                std::process::exit(1);
            }
        }
    } else {
        // In non-dev modes, just report status
        match pending_migrations(database_url, migrations) {
            Ok(pending) if pending.is_empty() => {
                tracing::info!(target = %target, "Database migrations are up to date");
            }
            Ok(pending) => {
                if !is_dev {
                    // Any non-dev profile (prod/production AND custom names like
                    // `fly`/`staging`) is opt-in (issue #1903). Name the profile
                    // and the key an operator would set so a custom-profile
                    // deploy is not left with only the generic "Run `autumn
                    // migrate`" line as its signal.
                    tracing::warn!(
                        profile = profile_name,
                        target = %target,
                        "Profile is opt-in for startup migrations: automatic migrations are \
                         disabled by default. Run `autumn migrate check` to review safety before \
                         applying, then `autumn migrate` in your deployment job. Set \
                         database.auto_migrate=true (or the auto_migrate_in_production alias) only \
                         for single-process deployments after confirming all pending migrations \
                         are safe for a rolling deploy (expand/contract pattern)."
                    );
                }
                tracing::warn!(
                    count = pending.len(),
                    target = %target,
                    "Pending migrations detected. Run `autumn migrate` to apply them."
                );
                for name in &pending {
                    tracing::warn!(migration = %name, target = %target, "Pending migration");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, target = %target, "Could not check migration status");
            }
        }
    }
}

/// Run migrations against a `SQLite` control target at startup (issue #1614, PR3).
///
/// The `SQLite` counterpart to [`auto_migrate`]: it honors the same profile
/// policy via [`should_auto_apply`] (dev / development auto-applies; every other
/// profile — prod/production and custom — applies only when `auto_migrate` or
/// the `auto_migrate_in_production` alias opts in; otherwise it reports pending
/// work), and fails fast (`process::exit(1)`) on an apply error exactly like the
/// Postgres path.
///
/// It deliberately omits the two Postgres-specific mechanisms
/// [`auto_migrate`] relies on:
///
///   * **the advisory lock** — `SQLite` is single-writer; there is no
///     `pg_advisory_lock` and no cross-process migration race to serialize
///     (`run_pending_sqlite` applies directly, unlocked).
///   * **the content-checksum drift guard** — its `autumn_migration_checksums`
///     bookkeeping is Postgres DDL (`TIMESTAMPTZ`, `now()`, `to_regclass`) and
///     is not ported to `SQLite` in this PR.
///
/// Sharding (directory / shard-map control tables, per-shard fan-out) is
/// Postgres-only and is rejected upstream at boot
/// (`sqlite_sharding_unsupported_guard`), so this only ever applies the
/// registered control migration set.
#[cfg(feature = "sqlite")]
pub(crate) fn auto_migrate_sqlite(
    database_url: &str,
    profile: Option<&str>,
    auto_migrate: Option<bool>,
    auto_migrate_in_production: bool,
    migrations: impl diesel::migration::MigrationSource<diesel::sqlite::Sqlite> + Send,
    target: &str,
) {
    // An in-memory target (private OR shared-cache) with registered migrations
    // cannot work: the migrated schema is lost before the runtime pool anchors
    // it — a private connection is its own empty database, and a shared in-memory
    // database is destroyed when its last connection closes. Fail startup fast
    // with an actionable message — in both the auto-apply and report-pending
    // profiles — rather than booting into a schema-less pool whose every
    // DB-backed request 500s (issue #1614 follow-up).
    if let Some(err) = reject_in_memory_migrations(database_url, &migrations) {
        tracing::error!(
            target = %target,
            error = %err,
            "Refusing to run SQLite migrations against an in-memory target",
        );
        std::process::exit(1);
    }
    if should_auto_apply(profile, auto_migrate, auto_migrate_in_production) {
        tracing::info!(target = %target, "Running pending SQLite database migrations...");
        match run_pending_sqlite(database_url, migrations) {
            Ok(result) if result.applied.is_empty() => {
                tracing::info!(target = %target, "No pending migrations");
            }
            Ok(result) => {
                for name in &result.applied {
                    tracing::info!(migration = %name, target = %target, "Applied migration");
                }
                tracing::info!(
                    count = result.applied.len(),
                    target = %target,
                    "All pending migrations applied"
                );
            }
            Err(e) => {
                tracing::error!(error = %e, target = %target, "Failed to run migrations");
                std::process::exit(1);
            }
        }
    } else {
        match pending_migrations_sqlite(database_url, migrations) {
            Ok(pending) if pending.is_empty() => {
                tracing::info!(target = %target, "Database migrations are up to date");
            }
            Ok(pending) => {
                tracing::warn!(
                    count = pending.len(),
                    target = %target,
                    "Pending migrations detected. Run `autumn migrate` to apply them."
                );
                for name in &pending {
                    tracing::warn!(migration = %name, target = %target, "Pending migration");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, target = %target, "Could not check migration status");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Migration-version collision auto-resolution (plugin/app/framework) ─

    #[test]
    fn no_disambiguation_when_versions_are_disjoint() {
        const A: EmbeddedMigrations =
            diesel_migrations::embed_migrations!("../examples/todo-app/migrations");
        const B: EmbeddedMigrations =
            diesel_migrations::embed_migrations!("tests/fixtures/plugin_migrations_ok");
        let disambiguated = compute_migration_disambiguation(&[("app", &A), ("test-plugin", &B)]);
        assert!(
            disambiguated.is_empty(),
            "disjoint version sets must not be touched: {disambiguated:?}"
        );
    }

    #[test]
    fn no_disambiguation_when_same_set_registered_twice() {
        // The intentional, harmless case: the exact same migration set
        // registered from two different call sites (e.g. two plugins that
        // both depend on a shared migrations bundle) reuses the same
        // versions AND full names.
        const A: EmbeddedMigrations =
            diesel_migrations::embed_migrations!("tests/fixtures/plugin_migrations_ok");
        let disambiguated = compute_migration_disambiguation(&[("plugin-a", &A), ("plugin-b", &A)]);
        assert!(
            disambiguated.is_empty(),
            "re-registering the identical set must not be treated as a collision: {disambiguated:?}"
        );
    }

    #[test]
    fn disambiguates_a_real_version_collision() {
        // Real-world shape: a framework migration and an app's own first
        // migration both picking the all-zero placeholder version — exactly
        // what `examples/todo-app` hits against the framework's legacy
        // `create_api_tokens` migration.
        const APP: EmbeddedMigrations =
            diesel_migrations::embed_migrations!("../examples/todo-app/migrations");
        const COLLIDING: EmbeddedMigrations =
            diesel_migrations::embed_migrations!("tests/fixtures/plugin_migrations_collision");

        let disambiguated =
            compute_migration_disambiguation(&[("app", &APP), ("test-plugin", &COLLIDING)]);

        // Deterministic, content-based order (lexicographic full name):
        // "00000000000000_create_gadgets" < "00000000000000_create_todos",
        // so the plugin's migration keeps the plain version regardless of
        // registration order.
        assert!(
            !disambiguated.contains_key("00000000000000_create_gadgets"),
            "the lexicographically-first migration must keep its original version: {disambiguated:?}"
        );
        let substitute = disambiguated
            .get("00000000000000_create_todos")
            .expect("the other colliding migration must be disambiguated");
        assert_eq!(
            substitute,
            &bounded_substitute_version("00000000000000", "00000000000000_create_todos", 1)
        );
        assert!(
            substitute.len() <= 50,
            "must fit __diesel_schema_migrations.version (VARCHAR(50)): {substitute}"
        );
    }

    #[test]
    fn duplicate_migrations_substitute_hash_is_independent_of_registration_order() {
        // A migration folded into two bundles under different source names — the
        // intentional, harmless "same set registered twice" case — that also collides at
        // its version with a third, differently-named migration: the duplicate is the one
        // that must be substituted, since "20260101000000_a_third_migration" sorts before
        // "20260101000000_create_gizmos". The substitute hash derives from the duplicate's
        // own full name, not from which sources register it, so it must be identical
        // whatever the registration order — and whether or not it is ever registered under
        // a second name at all (see the next test).
        const DUP: EmbeddedMigrations =
            diesel_migrations::embed_migrations!("tests/fixtures/plugin_migrations_ok");
        const OTHER: EmbeddedMigrations =
            diesel_migrations::embed_migrations!("tests/fixtures/plugin_migrations_collision_2");

        let order_a = compute_migration_disambiguation(&[
            ("other", &OTHER),
            ("zzz-plugin", &DUP),
            ("aaa-plugin", &DUP),
        ]);
        let order_b = compute_migration_disambiguation(&[
            ("other", &OTHER),
            ("aaa-plugin", &DUP),
            ("zzz-plugin", &DUP),
        ]);

        let substitute_a = order_a
            .get("20260101000000_create_gizmos")
            .expect("the duplicate must be disambiguated in registration order A");
        let substitute_b = order_b
            .get("20260101000000_create_gizmos")
            .expect("the duplicate must be disambiguated in registration order B");
        assert_eq!(
            substitute_a, substitute_b,
            "the substitute must not depend on which of the duplicate's two \
             registrations happened to run first"
        );
        assert_eq!(
            substitute_a,
            &bounded_substitute_version("20260101000000", "20260101000000_create_gizmos", 1),
            "the substitute is salted with the duplicate's own full name, not any source name"
        );
    }

    #[test]
    fn duplicate_migrations_substitute_hash_is_stable_as_registrations_change() {
        // The stronger guarantee `duplicate_migrations_substitute_hash_is_independent_of_registration_order`
        // only hints at: an ALREADY-APPLIED migration's substitute must not
        // change merely because the set of sources registering it grows or
        // shrinks across releases (e.g. the same bundle later folded into an
        // additional plugin too) -- only the migration's own, permanently
        // fixed full name may drive the hash. Registered once vs. registered
        // twice (under an entirely different second name) must produce the
        // SAME substitute for the losing migration.
        const DUP: EmbeddedMigrations =
            diesel_migrations::embed_migrations!("tests/fixtures/plugin_migrations_ok");
        const OTHER: EmbeddedMigrations =
            diesel_migrations::embed_migrations!("tests/fixtures/plugin_migrations_collision_2");

        let registered_once =
            compute_migration_disambiguation(&[("other", &OTHER), ("only-plugin", &DUP)]);
        let registered_twice = compute_migration_disambiguation(&[
            ("other", &OTHER),
            ("only-plugin", &DUP),
            ("a-brand-new-plugin-added-later", &DUP),
        ]);

        assert_eq!(
            registered_once.get("20260101000000_create_gizmos"),
            registered_twice.get("20260101000000_create_gizmos"),
            "adding a second registration of an already-duplicated migration must not \
             change the substitute already assigned to it"
        );
    }

    #[test]
    fn substitute_never_collides_with_an_unrelated_raw_version() {
        // If a generated substitute happened to equal some OTHER, unrelated
        // migration's own plain version, the two would share one Diesel
        // tracking key -- exactly the bug this guard exists to prevent.
        // Regression-guard the underlying invariant directly: no output
        // substitute may equal any INPUT raw version.
        const APP: EmbeddedMigrations =
            diesel_migrations::embed_migrations!("../examples/todo-app/migrations");
        const COLLIDING: EmbeddedMigrations =
            diesel_migrations::embed_migrations!("tests/fixtures/plugin_migrations_collision");
        let raw_versions: std::collections::HashSet<String> =
            migration_versions_and_names::<Pg>(&APP)
                .unwrap()
                .into_iter()
                .map(|(v, _)| v)
                .chain(
                    migration_versions_and_names::<Pg>(&COLLIDING)
                        .unwrap()
                        .into_iter()
                        .map(|(v, _)| v),
                )
                .collect();

        let disambiguated =
            compute_migration_disambiguation(&[("app", &APP), ("test-plugin", &COLLIDING)]);
        for substitute in disambiguated.values() {
            assert!(
                !raw_versions.contains(substitute),
                "substitute {substitute:?} must never coincide with an unrelated migration's own raw version"
            );
        }
    }

    #[test]
    fn disambiguates_a_collision_against_the_standalone_shard_control_migrations() {
        // `SHARD_DIRECTORY_MIGRATIONS`/`SHARD_MAP_MIGRATIONS` are applied
        // straight from their own `const`s (not through the app's
        // `migrations`), so callers must pass them into
        // `compute_migration_disambiguation` explicitly alongside the
        // registered set -- this fixture's version
        // ("20260612000000") matches the real shard-directory migration's,
        // under a different name, exactly the scenario a plugin could hit.
        const PLUGIN: EmbeddedMigrations = diesel_migrations::embed_migrations!(
            "tests/fixtures/plugin_migrations_collision_shard_directory"
        );
        let disambiguated = compute_migration_disambiguation(&[
            ("test-plugin", &PLUGIN),
            (
                "shard-directory",
                &crate::sharding::SHARD_DIRECTORY_MIGRATIONS,
            ),
        ]);
        // "20260612000000_a_plugin_thing" < "20260612000000_create_shard_directory"
        // lexicographically, so the PLUGIN keeps the plain version and the
        // framework's own shard-directory migration is the one substituted.
        assert!(
            !disambiguated.contains_key("20260612000000_a_plugin_thing"),
            "the lexicographically-first migration must keep its original version: {disambiguated:?}"
        );
        assert!(
            disambiguated.contains_key("20260612000000_create_shard_directory"),
            "the collision against the standalone shard-directory set must be caught: {disambiguated:?}"
        );
    }

    #[test]
    fn bounded_substitute_version_fits_varchar_50_even_with_a_long_source_name() {
        // `plugin_migrations` accepts an arbitrary `&'static str` name --
        // an unbounded "{version}+{name}" would overflow
        // __diesel_schema_migrations.version (VARCHAR(50)) and fail the
        // INSERT at migration time.
        let long_name = "a-plugin-with-a-very-long-descriptive-crate-name-that-keeps-going";
        let substitute = bounded_substitute_version("20260101000000", long_name, 1);
        assert!(
            substitute.len() <= 50,
            "must fit VARCHAR(50): {substitute} ({} chars)",
            substitute.len()
        );
        assert!(substitute.starts_with("20260101000000"));
    }

    #[test]
    fn bounded_substitute_version_tie_breaker_changes_the_result() {
        let first = bounded_substitute_version("20260101000000", "same-name", 1);
        let second = bounded_substitute_version("20260101000000", "same-name", 2);
        assert_ne!(first, second);
    }

    #[test]
    fn migration_versions_and_names_enumerates_todo_app_fixture() {
        const MIGRATIONS: EmbeddedMigrations =
            diesel_migrations::embed_migrations!("../examples/todo-app/migrations");
        let pairs = migration_versions_and_names::<Pg>(&MIGRATIONS).unwrap();
        assert!(
            pairs
                .iter()
                .any(|(v, n)| v == "00000000000000" && n == "00000000000000_create_todos"),
            "expected the todo-app fixture's first migration, got {pairs:?}"
        );
    }

    // ── Per-attempt connect_timeout injection (`--wait`) ───────────────────

    #[test]
    fn with_connect_timeout_appends_to_keyword_strings() {
        // Keyword/value strings pass config validation, so the wait loop
        // must bound their connect attempts too — a blackholed host must
        // not hang one attempt past the whole wait budget.
        assert_eq!(
            with_connect_timeout("host=db user=app sslmode=require", 7),
            "host=db user=app sslmode=require connect_timeout=7"
        );
        // Quoted/spaced variants still gain the parameter.
        assert_eq!(
            with_connect_timeout("host=db password='p w' sslmode = require", 7),
            "host=db password='p w' sslmode = require connect_timeout=7"
        );
    }

    #[test]
    fn with_connect_timeout_respects_a_user_provided_keyword_timeout() {
        assert_eq!(
            with_connect_timeout("host=db connect_timeout=42 user=app", 7),
            "host=db connect_timeout=42 user=app",
            "an explicit user timeout must not be overridden"
        );
    }

    #[test]
    fn with_connect_timeout_url_behavior_is_unchanged() {
        assert_eq!(
            with_connect_timeout("postgres://u@h/db", 7),
            "postgres://u@h/db?connect_timeout=7"
        );
        // Existing raw query params survive byte-for-byte; a stale
        // connect_timeout is replaced (the wait loop shrinks it per attempt).
        assert_eq!(
            with_connect_timeout(
                "postgres://u@h/db?options=-c%20search_path%3Dapp&connect_timeout=99",
                7
            ),
            "postgres://u@h/db?options=-c%20search_path%3Dapp&connect_timeout=7"
        );
        // Malformed strings pass through so connect reports its own error.
        assert_eq!(with_connect_timeout("host=", 7), "host=");
    }

    // ── Red-phase tests for advisory-lock API (fail until implemented) ─────

    #[test]
    fn lock_timeout_error_display() {
        let err = MigrationError::LockTimeout { timeout_secs: 60 };
        let msg = err.to_string();
        assert!(msg.contains("60"), "message must contain the timeout value");
        assert!(
            msg.to_lowercase().contains("lock") || msg.to_lowercase().contains("timeout"),
            "message must mention lock or timeout: {msg}"
        );
    }

    #[test]
    fn migration_advisory_lock_key_is_positive_and_stable() {
        const { assert!(MIGRATION_ADVISORY_LOCK_KEY > 0) };
        // Exact value is part of the public API; it must not drift across versions.
        assert_eq!(
            MIGRATION_ADVISORY_LOCK_KEY,
            0x6175_746E_5F6D_6967_u64.cast_signed()
        );
    }

    #[test]
    fn default_lock_wait_timeout_is_sixty_seconds() {
        assert_eq!(DEFAULT_LOCK_WAIT_TIMEOUT.as_secs(), 60);
    }

    #[test]
    fn run_pending_locked_fails_with_connection_error_on_bad_url() {
        const MIGRATIONS: EmbeddedMigrations =
            diesel_migrations::embed_migrations!("../examples/todo-app/migrations");
        let url = "postgres://invalid_user:invalid_password@0.0.0.0:1/invalid_db";
        let result = run_pending_locked(url, MIGRATIONS, None);
        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), MigrationError::Connection(_)),
            "unreachable host must produce Connection error, not LockTimeout"
        );
    }

    #[test]
    fn run_pending_locked_inner_with_checksum_map_still_errors_on_bad_url() {
        // The checksum-carrying locked path (the one `auto_migrate` uses) must
        // reach the connection stage and surface a Connection error on an
        // unreachable host — the under-lock validation never runs before the
        // session exists, so the map does not change the failure mode.
        const MIGRATIONS: EmbeddedMigrations =
            diesel_migrations::embed_migrations!("../examples/todo-app/migrations");
        let url = "postgres://invalid_user:invalid_password@0.0.0.0:1/invalid_db";
        let mut up_by_version: HashMap<String, String> = HashMap::new();
        up_by_version.insert("20260101000000".to_string(), "SELECT 1;".to_string());
        let result = run_pending_locked_inner(url, MIGRATIONS, None, Some(&up_by_version));
        assert!(
            matches!(result.unwrap_err(), MigrationError::Connection(_)),
            "unreachable host must produce Connection error even with a checksum map"
        );
    }

    /// Spawns 4 concurrent migration runners against a real Postgres container
    /// and asserts that exactly one applies the pending migrations while the
    /// rest find no pending work and exit successfully.
    #[cfg(feature = "test-support")]
    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn four_concurrent_runners_serialize_and_exactly_one_applies() {
        use testcontainers::runners::AsyncRunner as _;
        use testcontainers_modules::postgres::Postgres;

        const TEST_MIGRATIONS: EmbeddedMigrations =
            diesel_migrations::embed_migrations!("../examples/todo-app/migrations");

        let container = Postgres::default()
            .start()
            .await
            .expect("failed to start Postgres testcontainer (is Docker running?)");

        let host = container.get_host().await.unwrap();
        let port = container.get_host_port_ipv4(5432).await.unwrap();
        let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

        let handles: Vec<_> = (0..4)
            .map(|_| {
                let url = url.clone();
                tokio::task::spawn_blocking(move || run_pending_locked(&url, TEST_MIGRATIONS, None))
            })
            .collect();

        let mut results = Vec::new();
        for handle in handles {
            results.push(handle.await.expect("task panicked"));
        }

        // (c) No runner should produce an error.
        for result in &results {
            assert!(
                result.is_ok(),
                "runner produced unexpected error: {result:?}"
            );
        }

        // (a) Exactly one runner applied migrations.
        let applied_count = results
            .iter()
            .filter(|r| r.as_ref().is_ok_and(|m| !m.applied.is_empty()))
            .count();
        assert_eq!(
            applied_count, 1,
            "exactly one runner should apply migrations; results={results:?}"
        );

        // (b) The final schema must include all expected tables.
        // We verify by checking that a subsequent run finds no pending migrations.
        let final_check =
            run_pending_locked(&url, TEST_MIGRATIONS, None).expect("post-run check failed");
        assert!(
            final_check.applied.is_empty(),
            "schema must be fully applied after concurrent run"
        );
    }

    /// End-to-end proof of the migration-checksum loop (issue #1203) against a
    /// live Postgres container. Exercises, in order: the fresh-DB behaviour of
    /// [`recorded_checksums`] (read-only — empty map, connection not poisoned,
    /// table still absent afterward — then created by the framework migration),
    /// recording a freshly-applied migration's checksum, re-validating the same
    /// content (Ok), detecting edited content (Err naming the version and both
    /// hashes), the legacy/`Unrecorded` path (present in
    /// `__diesel_schema_migrations` with no recorded checksum → never errors →
    /// baselined by [`record_checksums`]), and the [`rebaseline_checksum`]
    /// escape hatch.
    #[cfg(feature = "test-support")]
    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn checksum_loop_records_validates_and_detects_edits_against_live_db() {
        use testcontainers::runners::AsyncRunner as _;
        use testcontainers_modules::postgres::Postgres;

        let container = Postgres::default()
            .start()
            .await
            .expect("failed to start Postgres testcontainer (is Docker running?)");

        let host = container.get_host().await.unwrap();
        let port = container.get_host_port_ipv4(5432).await.unwrap();
        let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

        // Diesel's sync API is blocking; run it off the runtime thread. The
        // container handle stays owned here so it outlives the blocking work.
        tokio::task::spawn_blocking(move || checksum_loop_body(&url))
            .await
            .expect("checksum loop task panicked");
    }

    /// Whether `autumn_migration_checksums` currently exists, via the same
    /// non-erroring `to_regclass` probe the production code uses.
    #[cfg(feature = "test-support")]
    fn checksum_table_exists(conn: &mut diesel::PgConnection) -> bool {
        diesel::sql_query("SELECT to_regclass('autumn_migration_checksums') IS NOT NULL AS present")
            .get_result::<TableExistsRow>(conn)
            .expect("to_regclass probe must not error")
            .present
    }

    /// Synchronous body of
    /// [`checksum_loop_records_validates_and_detects_edits_against_live_db`],
    /// factored out so the blocking diesel work runs on a `spawn_blocking`
    /// thread.
    #[cfg(feature = "test-support")]
    #[allow(clippy::too_many_lines)] // Linear end-to-end walk of the checksum loop.
    fn checksum_loop_body(url: &str) {
        use diesel::Connection as _;

        #[derive(diesel::QueryableByName)]
        struct One {
            #[diesel(sql_type = diesel::sql_types::Integer)]
            one: i32,
        }

        let mut conn =
            diesel::PgConnection::establish(url).expect("failed to connect to Postgres container");

        // ── Step 7: fresh-DB behaviour (read-only contract) ─────────────────
        // Before any framework migration runs, `autumn_migration_checksums`
        // does not exist. `recorded_checksums` is READ-ONLY: it must return an
        // empty map — never an error — WITHOUT creating the table and WITHOUT
        // poisoning the session (issue #1203 review, P2-A). Read paths
        // (`autumn migrate status`, pre-apply validation) must not require DDL
        // privileges or mutate the DB just to display / check state.
        assert!(
            !checksum_table_exists(&mut conn),
            "checksum table must be absent before the first checksum call"
        );
        let empty = recorded_checksums(&mut conn)
            .expect("recorded_checksums must not error on a fresh DB (read-only, empty map)");
        assert!(
            empty.is_empty(),
            "fresh DB must report no recorded checksums"
        );
        // The table must STILL be absent — `recorded_checksums` is read-only and
        // must NOT create it (that is the write helpers' job on apply/record).
        assert!(
            !checksum_table_exists(&mut conn),
            "recorded_checksums must NOT create the checksum table (read-only)"
        );
        // The connection must remain usable after the fresh-DB path — this is
        // the highest-risk untested path.
        let row = diesel::sql_query("SELECT 1 AS one")
            .get_result::<One>(&mut conn)
            .expect("connection must remain usable after the fresh-DB path");
        assert_eq!(row.one, 1, "post-fresh-DB query must succeed");

        // ── Step 1: run framework migrations, which create the checksum table ─
        // The table is still absent (read-only reads above never created it);
        // the framework migration set includes the managed
        // `create_migration_checksums` DDL, so after applying it the table
        // exists and starts empty.
        run_pending(url, FRAMEWORK_MIGRATIONS).expect("framework migrations must apply");
        assert!(
            checksum_table_exists(&mut conn),
            "framework migrations must create the checksum table"
        );
        assert!(
            recorded_checksums(&mut conn)
                .expect("table exists")
                .is_empty(),
            "checksum table must start empty"
        );

        // ── Step 2: simulate an applied migration + record its checksum ─────
        let version = "20990101000000".to_string();
        let up_sql = "CREATE TABLE checksum_demo (id BIGINT PRIMARY KEY);";
        diesel::sql_query("INSERT INTO __diesel_schema_migrations (version) VALUES ($1)")
            .bind::<diesel::sql_types::Text, _>(&version)
            .execute(&mut conn)
            .expect("record applied migration version");
        let recorded_hash = migration_checksum(up_sql);
        record_checksum(&mut conn, &version, &recorded_hash).expect("record_checksum");

        let applied = vec![version.clone()];
        let mut up_by_version: HashMap<String, String> = HashMap::new();
        up_by_version.insert(version.clone(), up_sql.to_string());

        // ── Step 3: re-validate the same content → Ok ──────────────────────
        let recorded = recorded_checksums(&mut conn).expect("read recorded checksums");
        assert_eq!(
            recorded.get(&version).map(String::as_str),
            Some(recorded_hash.as_str()),
            "the recorded checksum must round-trip through the DB"
        );
        validate_checksums(&applied, &up_by_version, &recorded)
            .expect("unedited content must validate Ok");
        assert_eq!(
            classify(&applied, &up_by_version, &recorded),
            vec![(version.clone(), ChecksumState::Ok)],
        );

        // ── Step 4: edited content → Err naming version + both hashes ───────
        let edited_sql = "CREATE TABLE checksum_demo (id BIGINT PRIMARY KEY, extra TEXT);";
        let actual_hash = migration_checksum(edited_sql);
        assert_ne!(
            recorded_hash, actual_hash,
            "edited content must hash differently"
        );
        let mut edited_by_version: HashMap<String, String> = HashMap::new();
        edited_by_version.insert(version.clone(), edited_sql.to_string());
        let err = validate_checksums(&applied, &edited_by_version, &recorded)
            .expect_err("edited content must fail validation");
        let msg = err.to_string();
        assert!(msg.contains(&version), "error must name the version: {msg}");
        assert!(
            msg.contains(&recorded_hash),
            "error must contain the recorded hash: {msg}"
        );
        assert!(
            msg.contains(&actual_hash),
            "error must contain the actual on-disk hash: {msg}"
        );
        assert_eq!(
            classify(&applied, &edited_by_version, &recorded),
            vec![(
                version.clone(),
                ChecksumState::Changed {
                    recorded: recorded_hash.clone(),
                    actual: actual_hash.clone(),
                }
            )],
        );

        // ── Step 5: legacy path — applied but with no recorded checksum ─────
        let legacy = "20990102000000".to_string();
        let legacy_sql = "CREATE TABLE checksum_legacy (id BIGINT PRIMARY KEY);";
        diesel::sql_query("INSERT INTO __diesel_schema_migrations (version) VALUES ($1)")
            .bind::<diesel::sql_types::Text, _>(&legacy)
            .execute(&mut conn)
            .expect("record legacy applied migration version");
        up_by_version.insert(legacy.clone(), legacy_sql.to_string());
        let applied_with_legacy = vec![version.clone(), legacy.clone()];

        let recorded = recorded_checksums(&mut conn).expect("read recorded checksums");
        assert!(
            !recorded.contains_key(&legacy),
            "legacy version must have no recorded checksum yet"
        );
        let states = classify(&applied_with_legacy, &up_by_version, &recorded);
        assert_eq!(
            states.iter().find(|(v, _)| v == &legacy).map(|(_, s)| s),
            Some(&ChecksumState::Unrecorded),
            "an applied migration with no recorded checksum must classify as Unrecorded"
        );
        validate_checksums(&applied_with_legacy, &up_by_version, &recorded)
            .expect("an Unrecorded legacy migration must NOT fail validation");
        // Baseline: record checksums for versions that lack them.
        let newly = record_checksums(&mut conn, &applied_with_legacy, &up_by_version)
            .expect("record_checksums baseline");
        assert_eq!(
            newly, 1,
            "only the legacy version should be newly recorded (the other already has one)"
        );
        let recorded = recorded_checksums(&mut conn).expect("read recorded checksums");
        assert_eq!(
            classify(&applied_with_legacy, &up_by_version, &recorded)
                .iter()
                .find(|(v, _)| v == &legacy)
                .map(|(_, s)| s),
            Some(&ChecksumState::Ok),
            "after baseline the legacy migration must validate Ok"
        );

        // ── Step 6: escape hatch — rebaseline overwrites, then validates ────
        // Declare the EDITED content canonical for `version`.
        rebaseline_checksum(&mut conn, &version, &actual_hash).expect("rebaseline_checksum");
        let recorded = recorded_checksums(&mut conn).expect("read recorded checksums");
        assert_eq!(
            recorded.get(&version).map(String::as_str),
            Some(actual_hash.as_str()),
            "rebaseline must overwrite the stored checksum"
        );
        validate_checksums(&applied, &edited_by_version, &recorded)
            .expect("edited content must validate Ok after re-baseline");

        // Step 8: the drift guard runs under the advisory lock. Prove
        // `run_pending_locked_inner` performs the checksum comparison inside its locked
        // critical section, not only in the pre-lock caller. At this point `version` is
        // recorded as `actual_hash` from step 6's re-baseline, but
        // `up_by_version[version]` still holds the original `up_sql`, which hashes to
        // `recorded_hash`, so the on-disk content and the recorded checksum disagree.
        // Feeding that map to the locked apply path must fail fast with a "checksum
        // mismatch" before any migration is applied. The framework migrations passed here
        // are already applied, so the only way this errors is the under-lock validation.
        assert_ne!(
            migration_checksum(up_sql),
            actual_hash,
            "sanity: original up.sql must not match the re-baselined checksum"
        );
        let drift = run_pending_locked_inner(url, FRAMEWORK_MIGRATIONS, None, Some(&up_by_version))
            .expect_err("under-lock validation must reject an applied migration that drifted");
        assert!(
            matches!(&drift, MigrationError::Migration(m) if m.contains("checksum mismatch") && m.contains(&version)),
            "run_pending_locked_inner must validate checksums inside the locked section: {drift:?}"
        );

        // ── Step 9: rollback clears the checksum, re-apply records fresh ────
        // Reproduces the PR review's P2: down + edit up.sql + re-apply must NOT
        // leave a stale hash. Simulate `autumn migrate down` for `version` the
        // same way the wired revert path does — remove it from
        // `__diesel_schema_migrations` and call `delete_checksums` — then prove
        // its checksum row is gone (so the version is neither applied nor
        // recorded → no drift), that re-applying an EDITED up.sql records the
        // NEW hash (not the stale one), and that a subsequent validate passes.
        diesel::sql_query("DELETE FROM __diesel_schema_migrations WHERE version = $1")
            .bind::<diesel::sql_types::Text, _>(&version)
            .execute(&mut conn)
            .expect("simulate down: remove applied version");
        let deleted = delete_checksums(&mut conn, std::slice::from_ref(&version))
            .expect("delete_checksums must succeed");
        assert_eq!(
            deleted, 1,
            "the reverted version's checksum row must be deleted"
        );
        let recorded = recorded_checksums(&mut conn).expect("read recorded checksums");
        assert!(
            !recorded.contains_key(&version),
            "after rollback the reverted version must have NO recorded checksum"
        );

        // Re-apply the EDITED up.sql: re-record the applied version and record
        // checksums from the edited content. The additive `record_checksums`
        // now writes a fresh row (the stale one was deleted on rollback) whose
        // hash matches the edited bytes.
        diesel::sql_query("INSERT INTO __diesel_schema_migrations (version) VALUES ($1)")
            .bind::<diesel::sql_types::Text, _>(&version)
            .execute(&mut conn)
            .expect("re-apply: re-record applied version");
        let newly = record_checksums(&mut conn, &applied, &edited_by_version)
            .expect("record_checksums after re-apply");
        assert_eq!(
            newly, 1,
            "re-apply after rollback must record a fresh checksum for the reverted version"
        );
        let recorded = recorded_checksums(&mut conn).expect("read recorded checksums");
        assert_eq!(
            recorded.get(&version).map(String::as_str),
            Some(actual_hash.as_str()),
            "re-apply must record the EDITED content's hash, not the stale original"
        );
        validate_checksums(&applied, &edited_by_version, &recorded)
            .expect("edited content validates Ok after rollback + fresh re-apply");

        // ── Step 10: the startup path VALIDATES but does NOT record ─────────
        // The startup auto-migrate path (`run_pending_locked_inner` with a disk
        // map) applies the EMBEDDED migration set, which may differ from the
        // on-disk `up.sql` read into the map, so it must never record disk bytes
        // for versions it "applies" (issue #1203 review). Authoritative
        // recording is deferred to the CLI/baseline paths where applied == disk.
        //
        // Simulate a freshly-applied-but-unrecorded version exactly as the
        // startup path would see it: present in `__diesel_schema_migrations` and
        // in the disk map, but with NO recorded checksum. Running the locked path
        // must leave it Unrecorded — it must NOT write a checksum row from the
        // disk bytes.
        let startup_version = "20990103000000".to_string();
        let startup_sql = "CREATE TABLE checksum_startup (id BIGINT PRIMARY KEY);";
        diesel::sql_query("INSERT INTO __diesel_schema_migrations (version) VALUES ($1)")
            .bind::<diesel::sql_types::Text, _>(&startup_version)
            .execute(&mut conn)
            .expect("simulate startup-applied version");
        let mut startup_by_version: HashMap<String, String> = HashMap::new();
        startup_by_version.insert(startup_version.clone(), startup_sql.to_string());
        assert!(
            !recorded_checksums(&mut conn)
                .expect("read recorded checksums")
                .contains_key(&startup_version),
            "precondition: the startup version must have no recorded checksum yet"
        );
        // FRAMEWORK_MIGRATIONS are all applied, so nothing pending; the map is
        // used for validation only. Unrecorded → Ok, so this must succeed.
        run_pending_locked_inner(url, FRAMEWORK_MIGRATIONS, None, Some(&startup_by_version))
            .expect("startup path validates an Unrecorded version as Ok");
        assert!(
            !recorded_checksums(&mut conn)
                .expect("read recorded checksums")
                .contains_key(&startup_version),
            "the startup auto-migrate path must NOT record checksums from disk bytes; \
             recording is deferred to the CLI/baseline paths"
        );
    }

    // ── Existing tests ─────────────────────────────────────────────────────

    // ── applied_user_migrations / revert_user_migrations ─────────────────────

    #[test]
    fn applied_user_migrations_fails_with_connection_error_on_bad_url() {
        // Red-phase: function exists and returns Connection error on unreachable host.
        let dir = std::path::Path::new("../examples/todo-app/migrations");
        let result =
            applied_user_migrations("postgres://invalid:invalid@0.0.0.0:1/invalid_db", dir);
        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), MigrationError::Connection(_)),
            "unreachable host must produce Connection error"
        );
    }

    #[test]
    fn revert_user_migrations_locked_fails_with_connection_error_on_bad_url() {
        // The connection is established before the lock/plan, so an unreachable
        // host produces a Connection error and the plan closure never runs.
        let dir = std::path::Path::new("../examples/todo-app/migrations");
        let mut planned = false;
        let result = revert_user_migrations_locked(
            "postgres://invalid:invalid@0.0.0.0:1/invalid_db",
            dir,
            None,
            |_applied| {
                planned = true;
                Ok(Vec::new())
            },
            |_| {},
        );
        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), MigrationError::Connection(_)),
            "unreachable host must produce Connection error"
        );
        assert!(
            !planned,
            "plan closure must not run when the connection fails"
        );
    }

    #[test]
    fn record_checksums_from_dir_locked_fails_with_connection_error_on_bad_url() {
        // `autumn migrate baseline` primitive: it reads the migrations dir first,
        // then establishes a connection to take the advisory lock before the
        // read+write. An unreachable host must surface as a Connection error
        // (never a panic or a silently-held lock).
        let dir = std::path::Path::new("../examples/todo-app/migrations");
        let result = record_checksums_from_dir_locked(
            "postgres://invalid:invalid@0.0.0.0:1/invalid_db",
            dir,
            None,
        );
        assert!(
            matches!(result.unwrap_err(), MigrationError::Connection(_)),
            "unreachable host must produce Connection error"
        );
    }

    #[test]
    fn rebaseline_checksum_from_dir_locked_fails_with_connection_error_on_bad_url() {
        // `autumn migrate baseline --force <version>` primitive. `00000000000000`
        // exists on disk, so the up.sql lookup succeeds and the code proceeds to
        // connect for the advisory lock — an unreachable host must produce a
        // Connection error.
        let dir = std::path::Path::new("../examples/todo-app/migrations");
        let result = rebaseline_checksum_from_dir_locked(
            "postgres://invalid:invalid@0.0.0.0:1/invalid_db",
            dir,
            "00000000000000",
            None,
        );
        assert!(
            matches!(result.unwrap_err(), MigrationError::Connection(_)),
            "unreachable host must produce Connection error"
        );
    }

    #[test]
    fn rebaseline_checksum_from_dir_locked_errors_before_connecting_on_unknown_version() {
        // The up.sql lookup is a pure disk read that runs BEFORE any connection
        // or lock acquisition, so an unknown version fails fast with a Migration
        // error and never opens a session (nothing to serialize).
        let dir = std::path::Path::new("../examples/todo-app/migrations");
        let result = rebaseline_checksum_from_dir_locked(
            "postgres://invalid:invalid@0.0.0.0:1/invalid_db",
            dir,
            "99999999999999",
            None,
        );
        assert!(
            matches!(result.unwrap_err(), MigrationError::Migration(m) if m.contains("99999999999999")),
            "an unknown version must fail fast with a Migration error naming it"
        );
    }

    #[test]
    fn applied_user_migration_resolves_dir_field() {
        let m = AppliedUserMigration {
            version: "20260101000000".to_string(),
            name: "20260101000000_create_posts".to_string(),
            dir: Some(std::path::PathBuf::from(
                "migrations/20260101000000_create_posts",
            )),
        };
        let s = format!("{m:?}");
        assert!(s.contains("create_posts"));
        assert!(m.dir.is_some());
    }

    fn version_map(pairs: &[(&str, &str)]) -> std::collections::BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(v, n)| ((*v).to_string(), (*n).to_string()))
            .collect()
    }

    fn version_set(versions: &[&str]) -> std::collections::BTreeSet<String> {
        versions.iter().map(|v| (*v).to_string()).collect()
    }

    #[test]
    fn classify_excludes_framework_versions_absent_locally() {
        let applied = vec!["00000000000000".to_string(), "20260101000000".to_string()];
        let by_version = version_map(&[("20260101000000", "20260101000000_create_posts")]);
        let framework = version_set(&["00000000000000"]);

        let user = classify_applied_user_migrations(
            &applied,
            &by_version,
            &framework,
            Path::new("migrations"),
        );

        // The framework version (absent locally) is dropped; the user migration remains.
        assert_eq!(user.len(), 1);
        assert_eq!(user[0].version, "20260101000000");
        assert_eq!(
            user[0].dir,
            Some(Path::new("migrations").join("20260101000000_create_posts"))
        );
    }

    #[test]
    fn classify_keeps_user_migration_colliding_with_framework_version() {
        // A local user migration whose version equals a framework shim version
        // (the placeholder `00000000000000`) must be kept — local presence wins.
        let applied = vec!["00000000000000".to_string()];
        let by_version = version_map(&[("00000000000000", "00000000000000_create_todos")]);
        let framework = version_set(&["00000000000000"]);

        let user = classify_applied_user_migrations(
            &applied,
            &by_version,
            &framework,
            Path::new("migrations"),
        );

        assert_eq!(
            user.len(),
            1,
            "user migration sharing a framework version must not be dropped"
        );
        assert_eq!(user[0].name, "00000000000000_create_todos");
        assert!(user[0].dir.is_some());
    }

    #[test]
    fn classify_surfaces_applied_migration_missing_locally() {
        // Applied, not framework-owned, but absent from the local dir: keep it
        // with dir = None so callers can refuse rather than silently drop it.
        let applied = vec!["20260101000000".to_string()];
        let by_version = version_map(&[]);
        let framework = version_set(&["00000000000000"]);

        let user = classify_applied_user_migrations(
            &applied,
            &by_version,
            &framework,
            Path::new("migrations"),
        );

        assert_eq!(user.len(), 1);
        assert_eq!(user[0].version, "20260101000000");
        assert!(
            user[0].dir.is_none(),
            "missing-locally migration must have dir = None"
        );
    }

    #[test]
    fn classify_sorts_ascending_and_resolves_hyphenated_dirs() {
        // `by_version` keys are Diesel-normalised (hyphens stripped); the dir
        // name can be the raw hyphenated form, and it must still resolve.
        let applied = vec!["20260102000000".to_string(), "20260101000000".to_string()];
        let by_version = version_map(&[
            ("20260101000000", "2026-01-01-000000_create_posts"),
            ("20260102000000", "20260102000000_add_body"),
        ]);
        let framework = version_set(&[]);

        let user = classify_applied_user_migrations(
            &applied,
            &by_version,
            &framework,
            Path::new("migrations"),
        );

        assert_eq!(user.len(), 2);
        // Ascending by version regardless of input order.
        assert_eq!(user[0].version, "20260101000000");
        assert_eq!(user[1].version, "20260102000000");
        // Hyphenated dir resolved via the normalised version key.
        assert_eq!(
            user[0].dir,
            Some(Path::new("migrations").join("2026-01-01-000000_create_posts"))
        );
    }

    #[test]
    fn reverted_migration_debug_includes_name() {
        let r = RevertedMigration {
            version: "20260101000000".to_string(),
            name: "20260101000000_create_posts".to_string(),
            duration: std::time::Duration::from_millis(42),
        };
        let s = format!("{r:?}");
        assert!(s.contains("create_posts"));
        assert!(s.contains("20260101000000"));
    }

    #[test]
    fn migration_result_debug() {
        let result = MigrationResult {
            applied: vec!["00000000000001".to_string()],
        };
        let debug = format!("{result:?}");
        assert!(debug.contains("00000000000001"));
    }

    #[test]
    fn migration_error_display_connection() {
        let err = MigrationError::Connection("refused".to_string());
        let msg = err.to_string();
        assert!(msg.contains("connect"));
        assert!(msg.contains("refused"));
    }

    #[test]
    fn migration_error_display_migration() {
        let err = MigrationError::Migration("syntax error".to_string());
        let msg = err.to_string();
        assert!(msg.contains("migration failed"));
        assert!(msg.contains("syntax error"));
    }

    #[test]
    fn replica_migration_comparison_detects_stale_replica() {
        let primary = vec!["00000000000001".to_owned(), "00000000000002".to_owned()];
        let replica = vec!["00000000000001".to_owned()];

        let readiness = compare_replica_migration_versions(&primary, &replica);

        assert!(!readiness.is_ready());
        assert!(
            readiness
                .detail()
                .expect("stale detail")
                .contains("00000000000002")
        );
    }

    #[test]
    fn profile_aliases_are_recognized() {
        // `auto_migrate` unset (None) — decision falls to convention + alias.
        assert!(should_auto_apply(Some("dev"), None, false));
        assert!(should_auto_apply(Some("development"), None, false));
        assert!(!should_auto_apply(Some("prod"), None, false));
        assert!(!should_auto_apply(Some("production"), None, false));
        assert!(should_auto_apply(Some("prod"), None, true));
        assert!(should_auto_apply(Some("production"), None, true));
    }

    /// Issue #1903: a CUSTOM profile (`fly`, `staging`, …) that opts in via the
    /// `auto_migrate_in_production` alias must auto-apply — the old name-gated
    /// check only honored `prod`/`production`, silently skipping custom profiles
    /// (the reported bug). And a custom profile with nothing set stays off.
    #[test]
    fn should_auto_apply_is_profile_agnostic_for_the_alias() {
        // THE bug: custom profile + alias opt-in => auto-apply.
        assert!(should_auto_apply(Some("fly"), None, true));
        assert!(should_auto_apply(Some("staging"), None, true));
        // Custom profile, nothing set => opt-in only, stays off.
        assert!(!should_auto_apply(Some("fly"), None, false));
        assert!(!should_auto_apply(Some("staging"), None, false));
    }

    /// Issue #1903: the profile-agnostic `auto_migrate` override wins on ANY
    /// profile — `Some(true)` forces apply, `Some(false)` forces report-only,
    /// even on dev.
    #[test]
    fn explicit_auto_migrate_overrides_convention_on_any_profile() {
        // dev/development => auto-apply by default (alias irrelevant).
        assert!(should_auto_apply(Some("dev"), None, false));
        assert!(should_auto_apply(Some("development"), None, false));
        // Explicit false on dev => report-only (override beats convention).
        assert!(!should_auto_apply(Some("dev"), Some(false), false));
        assert!(!should_auto_apply(Some("development"), Some(false), true));
        // Explicit true forces apply on a custom/prod profile with no alias.
        assert!(should_auto_apply(Some("fly"), Some(true), false));
        assert!(should_auto_apply(Some("prod"), Some(true), false));
        // Explicit `auto_migrate` wins even when the alias disagrees.
        assert!(should_auto_apply(Some("prod"), Some(true), false));
        assert!(!should_auto_apply(Some("prod"), Some(false), true));
    }

    #[test]
    fn run_pending_connection_error() {
        const MIGRATIONS: EmbeddedMigrations =
            diesel_migrations::embed_migrations!("../examples/todo-app/migrations");
        let url = "postgres://invalid_user:invalid_password@0.0.0.0:1/invalid_db";
        let result = run_pending(url, MIGRATIONS);

        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), MigrationError::Connection(_)));
    }

    #[test]
    fn replica_migration_readiness_ready_is_ready_and_has_no_detail() {
        assert!(ReplicaMigrationReadiness::Ready.is_ready());
        assert_eq!(ReplicaMigrationReadiness::Ready.detail(), None);
    }

    #[test]
    fn replica_migration_readiness_unknown_is_not_ready_and_has_detail() {
        let r = ReplicaMigrationReadiness::Unknown("db error xyz".to_string());
        assert!(!r.is_ready());
        let detail = r.detail().expect("Unknown must have detail");
        assert!(
            detail.contains("db error xyz"),
            "detail must contain the error: {detail}"
        );
    }

    #[test]
    fn compare_migration_versions_equal_returns_ready() {
        let versions = vec!["00000000000001".to_owned(), "00000000000002".to_owned()];
        let readiness = compare_replica_migration_versions(&versions, &versions.clone());
        assert!(readiness.is_ready());
        assert_eq!(readiness.detail(), None);
    }

    #[test]
    fn hold_migration_lock_fails_with_connection_error_on_bad_url() {
        let result = hold_migration_lock(
            "postgres://invalid_user:invalid_password@0.0.0.0:1/invalid_db",
            DEFAULT_LOCK_WAIT_TIMEOUT,
        );
        assert!(
            matches!(result.unwrap_err(), MigrationError::Connection(_)),
            "unreachable host must produce Connection error"
        );
    }

    #[test]
    fn pending_migrations_fails_with_connection_error_on_bad_url() {
        const MIGRATIONS: EmbeddedMigrations =
            diesel_migrations::embed_migrations!("../examples/todo-app/migrations");
        let url = "postgres://invalid_user:invalid_password@0.0.0.0:1/invalid_db";
        let result = pending_migrations(url, MIGRATIONS);
        assert!(matches!(result.unwrap_err(), MigrationError::Connection(_)));
    }

    #[test]
    fn stale_detail_uses_none_placeholder_when_primary_is_empty() {
        let empty: Vec<String> = vec![];
        let replica = vec!["00000000000001".to_owned()];
        let r = compare_replica_migration_versions(&empty, &replica);
        assert!(!r.is_ready());
        let detail = r.detail().expect("stale must have detail");
        assert!(
            detail.contains("<none>"),
            "empty primary must use <none>: {detail}"
        );
        assert!(detail.contains("00000000000001"));
    }

    #[test]
    fn should_auto_apply_returns_false_for_none_profile() {
        // No profile is treated as a non-dev, opt-in profile: off unless an
        // explicit override or the alias enables it.
        assert!(!should_auto_apply(None, None, false));
        assert!(should_auto_apply(None, None, true));
        assert!(should_auto_apply(None, Some(true), false));
        assert!(!should_auto_apply(None, Some(false), true));
    }

    // ── startup wait-for-DB (red phase) ───────────────────────────────────────

    #[test]
    fn is_retryable_connection_refused() {
        assert!(is_retryable_connection_error("connection refused"));
        assert!(is_retryable_connection_error(
            "FATAL: connection refused (os error 111)"
        ));
    }

    #[test]
    fn is_retryable_server_starting_up() {
        assert!(is_retryable_connection_error(
            "the database system is starting up"
        ));
    }

    #[test]
    fn is_retryable_timed_out() {
        assert!(is_retryable_connection_error("connection timed out"));
        assert!(is_retryable_connection_error(
            "timed out waiting for server"
        ));
        // libpq reports connect_timeout expiry as "timeout expired"
        assert!(is_retryable_connection_error("timeout expired"));
        assert!(is_retryable_connection_error(
            "ERROR: SSL connection: timeout expired"
        ));
    }

    #[test]
    fn is_retryable_could_not_connect() {
        assert!(is_retryable_connection_error(
            "could not connect to server: Connection refused"
        ));
    }

    #[test]
    fn is_not_retryable_auth_failure() {
        assert!(!is_retryable_connection_error(
            "password authentication failed for user \"app\""
        ));
    }

    #[test]
    fn is_not_retryable_database_does_not_exist() {
        assert!(!is_retryable_connection_error(
            "database \"mydb\" does not exist"
        ));
    }

    #[test]
    fn is_not_retryable_invalid_url() {
        assert!(!is_retryable_connection_error(
            "invalid connection string syntax"
        ));
    }

    #[test]
    fn is_not_retryable_role_does_not_exist() {
        assert!(!is_retryable_connection_error(
            "role \"app\" does not exist"
        ));
    }

    #[test]
    fn is_retryable_dns_linux() {
        assert!(is_retryable_connection_error(
            "could not translate host name \"db\" to address: \
             Name or service not known"
        ));
    }

    #[test]
    fn is_retryable_dns_macos() {
        assert!(is_retryable_connection_error(
            "could not translate host name \"db\" to address: \
             nodename nor servname provided, or not known"
        ));
    }

    #[test]
    fn is_retryable_dns_temporary_failure() {
        assert!(is_retryable_connection_error(
            "Temporary failure in name resolution"
        ));
    }

    #[test]
    fn backoff_delay_grows_and_caps() {
        let d = |n| backoff_delay(n).as_millis();
        assert_eq!(d(1), 500);
        assert_eq!(d(2), 1000);
        assert_eq!(d(3), 2000);
        assert_eq!(d(4), 4000);
        assert_eq!(d(5), 5000); // capped
        assert_eq!(d(10), 5000); // still capped
    }

    #[test]
    fn redact_removes_password_from_url() {
        let msg = "failed: postgres://user:secret@host:5432/db";
        let out = redact_db_url_credentials(msg);
        assert!(
            !out.contains("secret"),
            "password must not appear in output: {out}"
        );
    }

    #[test]
    fn redact_no_creds_url_unchanged_format() {
        let msg = "connection refused at postgres://host:5432/db";
        let out = redact_db_url_credentials(msg);
        assert!(
            !out.contains("****"),
            "no-cred url should not be mangled: {out}"
        );
        assert!(
            out.contains("host:5432"),
            "host should still be present: {out}"
        );
    }

    #[test]
    fn redact_removes_password_from_postgresql_scheme() {
        let msg = "failed: postgresql://user:s3cret@host:5432/db";
        let out = redact_db_url_credentials(msg);
        assert!(
            !out.contains("s3cret"),
            "password must not appear when scheme is postgresql://: {out}"
        );
        assert!(out.contains("****"), "masked marker missing: {out}");
    }

    #[test]
    fn redact_masks_whole_token_on_parse_failure() {
        // A URL with a bare @ in the password fails url::Url::parse; the whole
        // token must be replaced with **** rather than passed through unredacted.
        let msg = "error: postgres://user:p@ss@host/db";
        let out = redact_db_url_credentials(msg);
        assert!(
            !out.contains("p@ss"),
            "unparseable password must not leak: {out}"
        );
    }

    #[test]
    fn wait_for_database_inner_success_first_attempt() {
        use std::cell::Cell;
        let attempts = Cell::new(0u32);
        let sleeps = Cell::new(0u32);

        let result = wait_for_database_inner(
            std::time::Duration::from_secs(30),
            || {
                attempts.set(attempts.get() + 1);
                Ok(())
            },
            |_| sleeps.set(sleeps.get() + 1),
            || std::time::Duration::ZERO,
            |_, _| {},
        );
        assert!(result.is_ok());
        assert_eq!(attempts.get(), 1);
        assert_eq!(sleeps.get(), 0);
    }

    #[test]
    fn wait_for_database_inner_fatal_error_no_retry() {
        use std::cell::Cell;
        let attempts = Cell::new(0u32);
        let retried = Cell::new(false);

        let result = wait_for_database_inner(
            std::time::Duration::from_secs(30),
            || {
                attempts.set(attempts.get() + 1);
                Err(AttemptError::Fatal(
                    "password authentication failed".to_string(),
                ))
            },
            |_| {},
            || std::time::Duration::ZERO,
            |_, _| retried.set(true),
        );
        assert!(result.is_err());
        assert_eq!(attempts.get(), 1, "fatal error must not retry");
        assert!(!retried.get(), "on_retry must not fire for fatal errors");
    }

    #[test]
    fn wait_for_database_inner_success_on_third_attempt() {
        use std::cell::Cell;
        let attempts = Cell::new(0u32);
        let mut sleep_delays = Vec::new();

        let result = wait_for_database_inner(
            std::time::Duration::from_secs(30),
            || {
                let n = attempts.get() + 1;
                attempts.set(n);
                if n < 3 {
                    Err(AttemptError::Retryable("connection refused".to_string()))
                } else {
                    Ok(())
                }
            },
            |d| sleep_delays.push(d),
            || std::time::Duration::ZERO, // always within budget
            |_, _| {},
        );
        assert!(result.is_ok());
        assert_eq!(attempts.get(), 3);
        assert_eq!(sleep_delays.len(), 2);
        // Delays grow with capped exponential backoff
        assert_eq!(sleep_delays[0], std::time::Duration::from_millis(500));
        assert_eq!(sleep_delays[1], std::time::Duration::from_secs(1));
    }

    #[test]
    fn wait_for_database_inner_timeout_returns_error() {
        use std::cell::Cell;
        let attempts = Cell::new(0u32);

        let result = wait_for_database_inner(
            std::time::Duration::from_secs(5),
            || {
                attempts.set(attempts.get() + 1);
                Err(AttemptError::Retryable("connection refused".to_string()))
            },
            |_| {},
            || std::time::Duration::from_secs(10), // fake: already past the budget
            |_, _| {},
        );
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.to_lowercase().contains("wait")
                || msg.to_lowercase().contains("timeout")
                || msg.to_lowercase().contains("timed out"),
            "error must describe the timeout: {msg}"
        );
    }

    // ── Migration content checksums (issue #1203) ────────────────────────────

    #[test]
    fn migration_checksum_lf_and_crlf_hash_equally() {
        // Line-ending normalization: CRLF, CR, and LF variants of the same
        // logical source must produce the same checksum so a Windows checkout
        // does not spuriously trip the mismatch guard.
        let lf = "CREATE TABLE t (id INT);\nCREATE INDEX i ON t (id);\n";
        let crlf = "CREATE TABLE t (id INT);\r\nCREATE INDEX i ON t (id);\r\n";
        let cr = "CREATE TABLE t (id INT);\rCREATE INDEX i ON t (id);\r";
        assert_eq!(migration_checksum(lf), migration_checksum(crlf));
        assert_eq!(migration_checksum(lf), migration_checksum(cr));
    }

    #[test]
    fn migration_checksum_ignores_trailing_whitespace() {
        // trim_end strips trailing whitespace so an editor that appends /
        // strips a final newline does not spuriously trip the mismatch guard.
        let a = "SELECT 1;\n";
        let b = "SELECT 1;";
        let c = "SELECT 1;\n\n\n   \n";
        assert_eq!(migration_checksum(a), migration_checksum(b));
        assert_eq!(migration_checksum(a), migration_checksum(c));
    }

    #[test]
    fn migration_checksum_differs_on_semantic_edit() {
        // A real content change must change the checksum.
        let orig = "CREATE TABLE t (id INT);\n";
        let edited = "CREATE TABLE t (id BIGINT);\n";
        assert_ne!(migration_checksum(orig), migration_checksum(edited));
    }

    #[test]
    fn migration_checksum_is_hex_sha256() {
        // Known SHA-256 vector: sha256("abc") is the standard test vector.
        // Our normaliser leaves "abc" unchanged (no CRLF, no trailing ws),
        // so the checksum equals the canonical sha256("abc") hex.
        let expected = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(migration_checksum("abc"), expected);
    }

    #[test]
    fn migration_checksum_bytes_matches_string_form() {
        // The bytes and string forms must agree for valid UTF-8 so the
        // startup (embedded bytes) and CLI (on-disk string) paths hash
        // identically.
        let sql = "CREATE TABLE t (id INT);\r\n";
        assert_eq!(
            migration_checksum(sql),
            migration_checksum_bytes(sql.as_bytes())
        );
    }

    fn checksum_map(pairs: &[(&str, &str)]) -> std::collections::HashMap<String, String> {
        pairs
            .iter()
            .map(|(v, s)| ((*v).to_string(), (*s).to_string()))
            .collect()
    }

    #[test]
    fn classify_marks_matching_hash_ok() {
        let up = "SELECT 1;\n";
        let applied = vec!["20260101000000".to_string()];
        let up_map = checksum_map(&[("20260101000000", up)]);
        let recorded = checksum_map(&[("20260101000000", &migration_checksum(up))]);

        let result = classify(&applied, &up_map, &recorded);
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0].1, ChecksumState::Ok));
    }

    #[test]
    fn classify_flags_edited_migration_as_changed() {
        let original = "SELECT 1;\n";
        let edited = "SELECT 2;\n";
        let applied = vec!["20260101000000".to_string()];
        let up_map = checksum_map(&[("20260101000000", edited)]);
        let recorded = checksum_map(&[("20260101000000", &migration_checksum(original))]);

        let result = classify(&applied, &up_map, &recorded);
        assert_eq!(result.len(), 1);
        match &result[0].1 {
            ChecksumState::Changed { recorded, actual } => {
                assert_eq!(recorded, &migration_checksum(original));
                assert_eq!(actual, &migration_checksum(edited));
            }
            other => panic!("expected Changed, got {other:?}"),
        }
    }

    #[test]
    fn classify_marks_unrecorded_when_missing_from_recorded_map() {
        // Legacy migrations applied before the checksum table existed have no
        // recorded hash; they must show up as Unrecorded (never Changed).
        let up = "SELECT 1;\n";
        let applied = vec!["20260101000000".to_string()];
        let up_map = checksum_map(&[("20260101000000", up)]);
        let recorded = std::collections::HashMap::new();

        let result = classify(&applied, &up_map, &recorded);
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0].1, ChecksumState::Unrecorded));
    }

    #[test]
    fn classify_marks_unrecorded_when_up_sql_unresolvable() {
        // Applied migration whose local up.sql isn't available: treat as
        // Unrecorded rather than a hard error so status/validation still runs.
        let applied = vec!["20260101000000".to_string()];
        let up_map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let recorded = std::collections::HashMap::new();
        let result = classify(&applied, &up_map, &recorded);
        assert_eq!(result.len(), 1);
        assert!(matches!(result[0].1, ChecksumState::Unrecorded));
    }

    #[test]
    fn validate_checksums_returns_err_on_first_changed() {
        let orig = "SELECT 1;\n";
        let edited = "SELECT 2;\n";
        let applied = vec!["20260101000000".to_string()];
        let up_map = checksum_map(&[("20260101000000", edited)]);
        let recorded = checksum_map(&[("20260101000000", &migration_checksum(orig))]);

        let err = validate_checksums(&applied, &up_map, &recorded).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("checksum mismatch"),
            "message must name the failure mode: {msg}"
        );
        assert!(
            msg.contains("20260101000000"),
            "message must include the version: {msg}"
        );
        assert!(
            msg.contains(&migration_checksum(orig)),
            "message must include the recorded hex: {msg}"
        );
        assert!(
            msg.contains(&migration_checksum(edited)),
            "message must include the actual hex: {msg}"
        );
    }

    #[test]
    fn validate_checksums_tolerates_unrecorded() {
        let up = "SELECT 1;\n";
        let applied = vec!["20260101000000".to_string()];
        let up_map = checksum_map(&[("20260101000000", up)]);
        let recorded = std::collections::HashMap::new();
        assert!(validate_checksums(&applied, &up_map, &recorded).is_ok());
    }

    #[test]
    fn classify_marks_missing_when_recorded_but_up_sql_gone() {
        // A migration that WAS recorded (so it once belonged to this dir) but
        // whose on-disk up.sql is now gone (deleted or renamed) must classify
        // as Missing — genuine drift, NOT the tolerated Unrecorded legacy case.
        let original = "SELECT 1;\n";
        let applied = vec!["20260101000000".to_string()];
        let up_map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let recorded = checksum_map(&[("20260101000000", &migration_checksum(original))]);

        let result = classify(&applied, &up_map, &recorded);
        assert_eq!(result.len(), 1);
        match &result[0].1 {
            ChecksumState::Missing { recorded } => {
                assert_eq!(recorded, &migration_checksum(original));
            }
            other => panic!("expected Missing, got {other:?}"),
        }
    }

    #[test]
    fn validate_checksums_fails_on_missing() {
        // The Missing state must hard-fail validation (drift), with a message
        // that names the version, the recorded hex, and the remedy.
        let original = "SELECT 1;\n";
        let version = "20260101000000";
        let applied = vec![version.to_string()];
        let up_map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let recorded = checksum_map(&[(version, &migration_checksum(original))]);

        let err = validate_checksums(&applied, &up_map, &recorded)
            .expect_err("a recorded migration whose up.sql is gone must fail validation");
        let msg = err.to_string();
        assert!(
            msg.contains(version),
            "message must name the version: {msg}"
        );
        assert!(
            msg.contains(&migration_checksum(original)),
            "message must include the recorded hex: {msg}"
        );
        assert!(
            msg.contains("up.sql is missing from the source tree"),
            "message must name the failure mode: {msg}"
        );
        // The auto_migrate startup guard matches on this exact substring to
        // decide to hard-exit; keep them in lockstep.
        assert!(
            msg.contains("must never be deleted or renamed"),
            "message must state the remedy: {msg}"
        );
    }

    #[test]
    fn validate_checksums_tolerates_absent_up_sql_without_recorded_checksum() {
        // NEGATIVE / no-false-positive test: a version that is applied but has
        // NO recorded checksum and NO on-disk up.sql — e.g. an embedded or
        // framework migration whose SQL never lived in the validated dir, or a
        // truly legacy migration — must classify Unrecorded and NOT hard-fail,
        // even alongside a normal recorded+present migration. This proves the
        // `recorded.is_some()` scoping keeps `Missing` from firing on
        // migrations that never belonged to this dir.
        let ok_up = "SELECT 1;\n";
        let framework_version = "00000000000000".to_string();
        let user_version = "20260101000000".to_string();
        let applied = vec![framework_version.clone(), user_version];
        // Only the user migration is present on disk / recorded; the framework
        // version is applied but has neither a file nor a recorded checksum.
        let up_map = checksum_map(&[("20260101000000", ok_up)]);
        let recorded = checksum_map(&[("20260101000000", &migration_checksum(ok_up))]);

        let states = classify(&applied, &up_map, &recorded);
        assert_eq!(
            states
                .iter()
                .find(|(v, _)| v == &framework_version)
                .map(|(_, s)| s),
            Some(&ChecksumState::Unrecorded),
            "an applied version with no recorded checksum and no file must be Unrecorded"
        );
        validate_checksums(&applied, &up_map, &recorded)
            .expect("an unrecorded absent migration must not hard-fail validation");
    }

    #[test]
    fn checksum_status_excludes_framework_but_keeps_user_unrecorded() {
        // Reproduces the status bug: an applied FRAMEWORK version (no local
        // up.sql, no recorded checksum) must NOT be classified Unrecorded and
        // prompt `baseline` (which cannot record it — its up.sql isn't in the
        // user dir). A genuinely user-owned applied-but-unrecorded version must
        // STILL classify Unrecorded. The status path filters framework versions
        // via `user_applied_versions` BEFORE `classify`, so exercise that first.
        let framework_version = "00000000000000".to_string();
        let user_recorded_version = "20260101000000".to_string();
        let user_unrecorded_version = "20260102000000".to_string();

        let applied = vec![
            framework_version.clone(),
            user_recorded_version.clone(),
            user_unrecorded_version.clone(),
        ];

        let ok_up = "SELECT 1;\n";
        let pending_up = "SELECT 2;\n";
        // The framework version has no on-disk up.sql; both user versions do.
        let up_map = checksum_map(&[("20260101000000", ok_up), ("20260102000000", pending_up)]);
        // Only the first user migration has a recorded checksum. The framework
        // version is (correctly) never recorded against the user dir.
        let recorded = checksum_map(&[("20260101000000", &migration_checksum(ok_up))]);
        let framework = version_set(&["00000000000000"]);

        let user_applied = user_applied_versions(&applied, &up_map, &framework);
        // The framework version is filtered out entirely before classification.
        assert!(
            !user_applied.contains(&framework_version),
            "applied framework version must be excluded from checksum status"
        );

        let states = classify(&user_applied, &up_map, &recorded);
        // The framework version produces no status entry, so it can never
        // recommend `baseline`.
        assert!(
            states.iter().all(|(v, _)| v != &framework_version),
            "framework version must not appear in checksum status output"
        );
        // The recorded user migration is Ok.
        assert_eq!(
            states
                .iter()
                .find(|(v, _)| v == &user_recorded_version)
                .map(|(_, s)| s),
            Some(&ChecksumState::Ok),
        );
        // The user-owned applied-but-unrecorded migration STILL surfaces as
        // Unrecorded — the filter must not over-reach.
        assert_eq!(
            states
                .iter()
                .find(|(v, _)| v == &user_unrecorded_version)
                .map(|(_, s)| s),
            Some(&ChecksumState::Unrecorded),
            "a user-owned unrecorded migration must still classify Unrecorded"
        );
    }

    #[test]
    fn user_applied_versions_keeps_disk_missing_user_migration() {
        // A user-owned applied version absent from disk AND not framework-owned
        // must still be kept (it is a real problem to surface), while a
        // framework version absent from disk is dropped. The filter keys on
        // framework-set membership, never on "absent from the user dir".
        let framework_version = "00000000000000".to_string();
        let missing_user_version = "20260101000000".to_string();
        let applied = vec![framework_version.clone(), missing_user_version.clone()];
        // Neither version is present on disk.
        let up_map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let framework = version_set(&["00000000000000"]);

        let user_applied = user_applied_versions(&applied, &up_map, &framework);

        assert_eq!(user_applied, vec![missing_user_version]);
        assert!(
            !user_applied.contains(&framework_version),
            "framework version absent from disk must be dropped"
        );
    }
}
