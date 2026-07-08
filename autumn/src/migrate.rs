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

use diesel::migration::{Migration, MigrationSource};
use diesel::pg::Pg;
use diesel::{Connection, RunQueryDsl};
use diesel_migrations::{FileBasedMigrations, HarnessWithOutput, MigrationHarness};
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
macro_rules! with_migration_connection {
    ($url:expr, |$conn:ident| $body:expr) => {
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
                // The runtime (when owned) drives the connection's tokio
                // driver task: it must outlive every use of `conn`.
                drop(conn);
                drop(runtime);
                result
            }
        }
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
    migrations: impl diesel::migration::MigrationSource<diesel::pg::Pg>,
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
    migrations: impl diesel::migration::MigrationSource<diesel::pg::Pg>,
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
    conn: diesel::PgConnection,
}

impl std::fmt::Debug for MigrationLockGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MigrationLockGuard").finish_non_exhaustive()
    }
}

impl Drop for MigrationLockGuard {
    fn drop(&mut self) {
        release_migration_lock(&mut self.conn);
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
/// [`FRAMEWORK_MIGRATIONS`] plus the shard-required version-history and
/// commit-hook queue migrations.
///
/// Used to exclude framework-owned migrations from user rollback planning so
/// the forward-only contract is preserved regardless of which migrations are
/// applied locally. The shard-required sets must be included too: on a shard
/// target they are recorded in `__diesel_schema_migrations` but have no user
/// `down.sql`, so without this exclusion `autumn migrate down --shard` would
/// plan one of them as a user migration and fail.
fn framework_migration_versions() -> Result<std::collections::BTreeSet<String>, MigrationError> {
    let mut versions = std::collections::BTreeSet::new();
    for migrations in [
        MigrationSource::<Pg>::migrations(&FRAMEWORK_MIGRATIONS),
        MigrationSource::<Pg>::migrations(&crate::version_history::VERSION_HISTORY_MIGRATIONS),
        MigrationSource::<Pg>::migrations(
            &crate::repository_commit_hooks::REPOSITORY_COMMIT_HOOK_MIGRATIONS,
        ),
    ] {
        let migrations = migrations.map_err(|e| MigrationError::Migration(e.to_string()))?;
        versions.extend(migrations.iter().map(|m| m.name().version().to_string()));
    }
    Ok(versions)
}

/// Classify the database's applied migrations into user migrations (ascending
/// by version), excluding framework-owned ones and resolving each to its local
/// directory via Diesel's `name()`/`version()` metadata.
fn resolve_applied_user_migrations(
    conn: &mut diesel::PgConnection,
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
    let mut conn = diesel::PgConnection::establish(database_url)
        .map_err(|e| MigrationError::Connection(e.to_string()))?;

    let source = FileBasedMigrations::from_path(migrations_dir)
        .map_err(|e| MigrationError::Migration(format!("failed to read migrations dir: {e}")))?;
    let all_migrations: Vec<Box<dyn Migration<Pg>>> = source
        .migrations()
        .map_err(|e| MigrationError::Migration(e.to_string()))?;

    resolve_applied_user_migrations(&mut conn, &all_migrations, migrations_dir)
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
    P: FnOnce(&[AppliedUserMigration]) -> Result<Vec<String>, MigrationError>,
    F: FnMut(&RevertedMigration),
{
    let timeout = wait_timeout.unwrap_or(DEFAULT_LOCK_WAIT_TIMEOUT);

    let mut conn = diesel::PgConnection::establish(database_url)
        .map_err(|e| MigrationError::Connection(e.to_string()))?;

    let source = FileBasedMigrations::from_path(migrations_dir)
        .map_err(|e| MigrationError::Migration(format!("failed to read migrations dir: {e}")))?;
    let all_migrations: Vec<Box<dyn Migration<Pg>>> = source
        .migrations()
        .map_err(|e| MigrationError::Migration(e.to_string()))?;

    acquire_migration_lock(&mut conn, timeout)?;

    let result: Result<usize, MigrationError> = (|| {
        let applied_user =
            resolve_applied_user_migrations(&mut conn, &all_migrations, migrations_dir)?;
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

            on_reverted(&RevertedMigration {
                version: version.clone(),
                name: migration.name().to_string(),
                duration,
            });
            count += 1;
        }
        Ok(count)
    })();

    release_migration_lock(&mut conn);

    result
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
    // Splice `connect_timeout` directly into the raw percent-encoded query
    // string so existing parameters (e.g. `options=-c%20search_path%3Dapp`)
    // are preserved byte-for-byte.  Using query_pairs() / append_pair() would
    // decode and re-encode them via form encoding (spaces → `+`), which libpq
    // does not accept.
    url::Url::parse(url).map_or_else(
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
    )
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
    let mut conn = diesel::PgConnection::establish(database_url)
        .map_err(|e| MigrationError::Connection(e.to_string()))?;

    acquire_migration_lock(&mut conn, wait_timeout)?;

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
    migrations: impl diesel::migration::MigrationSource<diesel::pg::Pg>,
    wait_timeout: Option<std::time::Duration>,
) -> Result<MigrationResult, MigrationError> {
    let timeout = wait_timeout.unwrap_or(DEFAULT_LOCK_WAIT_TIMEOUT);

    with_migration_connection!(database_url, |conn| {
        acquire_migration_lock_on(conn, timeout)?;

        // Collect migration names eagerly so the harness borrow on `conn` is
        // dropped before we call release_migration_lock on the same connection.
        let migration_result: Result<Vec<String>, MigrationError> = {
            let mut harness = HarnessWithOutput::write_to_stdout(&mut *conn);
            harness
                .run_pending_migrations(migrations)
                .map(|applied| applied.iter().map(|m| format!("{m}")).collect())
                .map_err(|e| MigrationError::Migration(e.to_string()))
        };

        release_migration_lock_on(conn);

        Ok(MigrationResult {
            applied: migration_result?,
        })
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

fn should_auto_apply(profile: Option<&str>, allow_auto_migrate_in_production: bool) -> bool {
    let profile_name = profile.unwrap_or("none");
    matches!(profile_name, "dev" | "development")
        || (matches!(profile_name, "prod" | "production") && allow_auto_migrate_in_production)
}

/// Run migrations according to the active profile and migration policy.
///
/// - **dev/development**: runs all pending migrations automatically and logs each one.
/// - **prod/production**: logs pending migrations unless
///   `allow_auto_migrate_in_production` is enabled.
/// - **other profiles**: logs pending migrations without auto-applying.
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
    allow_auto_migrate_in_production: bool,
    migrations: &EmbeddedMigrations,
    target: &str,
) {
    let profile_name = profile.unwrap_or("none");
    let is_dev = matches!(profile_name, "dev" | "development");
    let is_prod = matches!(profile_name, "prod" | "production");
    let should_auto_apply = should_auto_apply(profile, allow_auto_migrate_in_production);

    if should_auto_apply {
        if is_dev {
            tracing::info!(target = %target, "Development profile: running pending database migrations...");
        } else {
            tracing::warn!(
                profile = profile_name,
                target = %target,
                "Production auto-migration is enabled; running pending database migrations"
            );
        }
        match run_pending_locked(database_url, EmbeddedMigrationsRef(migrations), None) {
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
        match pending_migrations(database_url, EmbeddedMigrationsRef(migrations)) {
            Ok(pending) if pending.is_empty() => {
                tracing::info!(target = %target, "Database migrations are up to date");
            }
            Ok(pending) => {
                if is_prod {
                    tracing::warn!(
                        "Production profile detected: automatic migrations are disabled by default. \
                         Run `autumn migrate check` to review safety before applying, then \
                         `autumn migrate` in your deployment job. \
                         Set database.auto_migrate_in_production=true only for single-process \
                         deployments after confirming all pending migrations are safe for a \
                         rolling deploy (expand/contract pattern)."
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(should_auto_apply(Some("dev"), false));
        assert!(should_auto_apply(Some("development"), false));
        assert!(!should_auto_apply(Some("prod"), false));
        assert!(!should_auto_apply(Some("production"), false));
        assert!(should_auto_apply(Some("prod"), true));
        assert!(should_auto_apply(Some("production"), true));
        assert!(!should_auto_apply(Some("staging"), true));
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
        assert!(!should_auto_apply(None, false));
        assert!(!should_auto_apply(None, true));
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
}
