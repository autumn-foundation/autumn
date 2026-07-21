//! Per-sim `SQLite` database substrate (sim-testing W4, issue #1797).
//!
//! This is the **DB lane** the sim builds its app on: a fresh, migrated,
//! in-process `SQLite` database, unique per simulation, ready to be handed to a
//! [`Sim`](crate::sim::Sim)-mounted app. It is deliberately **self-contained**
//! and additive — W2 owns `Sim::build(TestApp)` / the `SimApp` mount and will
//! consume this substrate there in a follow-up; nothing here defines or pre-empts
//! that public API.
//!
//! # Consumption shape (W2 seam)
//!
//! W2's `Sim::build` takes a [`crate::test::TestApp`] (not a raw `AppBuilder`),
//! and the endorsed pattern attaches the DB to the `TestApp` *before* build. So
//! the substrate hands its pool to a `TestApp` via
//! [`TestApp::with_db`](crate::test::TestApp::with_db):
//! `TestApp::new()…with_db(substrate.pool())`, and W2 then calls `sim.build(app)`.
//! [`SqliteSubstrate::pool`] returns exactly the [`crate::db::RuntimeConnection`]
//! pool that seam consumes; the substrate value itself must outlive the app,
//! because it holds the kept-alive guard connection.
//!
//! # Why shared-cache in-memory + a kept-alive guard
//!
//! A pure `:memory:` / `file::memory:` `SQLite` database is **private per
//! connection** and is destroyed the moment its *last* connection closes. So the
//! naive "migrate on a transient connection, then build a pool" sequence loses
//! the schema before the pool's first checkout, and every DB-backed request then
//! 500s with "no such table". That is exactly why the framework's startup
//! migration path *rejects* any in-memory target with registered migrations (see
//! [`crate::migrate::reject_in_memory_migrations`] /
//! [`crate::db::sqlite_target_is_any_in_memory`]).
//!
//! The substrate honours the literal "in-memory" DoD without hitting that
//! failure by using a **named shared-cache** in-memory database
//! (`file:<unique>?mode=memory&cache=shared`) plus a **kept-alive guard
//! connection** held for the whole sim lifetime:
//!
//! 1. Open the guard connection *first*, so the shared in-memory database exists
//!    and cannot be reclaimed.
//! 2. Apply the caller's migration set directly through diesel's
//!    `MigrationHarness` on that same guard connection. This deliberately
//!    bypasses [`crate::migrate::run_pending_sqlite`]'s in-memory reject — that
//!    reject is a *conservative* guard for the "no one is keeping the DB alive"
//!    case, and here the guard connection is precisely what makes applying
//!    migrations to a shared in-memory DB safe.
//! 3. Build the async runtime pool ([`crate::db::create_pool`]) over the *same*
//!    URL. Because the guard connection stays open, the shared in-memory database
//!    (and the migrated schema) survives for every pooled checkout.
//!
//! The unique database name (a fresh UUID per substrate) guarantees two sims
//! never share state: `SQLite` scopes a shared-cache in-memory database by its
//! name within the process, so distinct names are fully isolated databases.
//!
//! The framework's `SQLite` **repository-commit-hook** migration set
//! ([`crate::repository_commit_hooks::REPOSITORY_COMMIT_HOOK_MIGRATIONS`]) is
//! applied *first*, before any caller-registered migrations, so the
//! `autumn_repository_commit_hooks` control-plane table always exists on the
//! substrate. This is load-bearing: an app mounted on a substrate that has a DB
//! pool is drained by [`Sim::run_to_idle`](crate::sim::Sim::run_to_idle), whose
//! drain does an unconditional `COUNT(*)` on that table
//! ([`crate::test::drain_ready_repository_commit_hooks`]) — so without the table
//! `run_to_idle` panics with "no such table". Because `substrate.rs` lives inside
//! the `autumn` crate it can reference that `pub(crate)`-reachable migration set
//! directly; a sim author outside the crate cannot, which is exactly why the
//! substrate provisions it rather than leaving it to the caller. Other caller
//! migrations then apply on top.
//!
//! The Postgres control-plane [`crate::migrate::FRAMEWORK_MIGRATIONS`] are **not**
//! applied here: they are Postgres DDL, and the sim's representative scheduler +
//! job paths (below) need no other DB-side control tables.
//!
//! # Feature-unification hazard resolution (the representative path)
//!
//! Under `--features sqlite` two Postgres-only orchestration backends are
//! compiled **out**, so the sim exercises their local, in-process substitutes —
//! which are the real, default-configured paths a single-node `SQLite` app runs:
//!
//! * **Scheduler** — the sim runs the **`InProcessSchedulerCoordinator`**
//!   ([`crate::scheduler::coordinator_from_config`], the
//!   [`SchedulerBackend::InProcess`](crate::config::SchedulerBackend::InProcess)
//!   `#[default]`). `coordinator_from_config` *rejects*
//!   `scheduler.backend = "postgres"` under the `sqlite` feature, because the
//!   Postgres advisory-lock coordinator leases via `pg_advisory_lock`, which
//!   `SQLite` has no primitive for.
//! * **Jobs** — the sim runs the **local `JobAdminMemoryBackend`**
//!   ([`crate::job::start_runtime`] with the `"local"` backend default);
//!   `jobs.backend = "postgres"` is a hard-error stub under the `sqlite` feature
//!   because `SQLite` has no `LISTEN`/`NOTIFY` + `SKIP LOCKED` durable queue.
//!
//! **Documented divergence** (consistent with the RFC §12 scope): a green sim
//! proves the *orchestration, timing, and ordering* of the local scheduler + job
//! paths. It does **not** validate the Postgres advisory-lock scheduler leasing
//! or the durable Postgres `LISTEN`/`NOTIFY` + `SKIP LOCKED` job-queue
//! claim/lock semantics — those are compiled out under `sqlite` and remain the
//! province of the Postgres-backed integration tests. The sim is a determinism /
//! orchestration harness, not a Postgres queue-semantics conformance suite.

use std::sync::atomic::{AtomicU64, Ordering};

use diesel_migrations::{EmbeddedMigrations, HarnessWithOutput, MigrationHarness};

use crate::config::DatabaseConfig;
use crate::db::RuntimeConnection;
use crate::migrate::EmbeddedMigrationsRef;

use diesel_async::pooled_connection::deadpool::Pool;

/// A monotonic counter folded into each substrate's database name so that, even
/// within a single process and a single wall-clock instant, two substrates never
/// collide on a name (belt-and-suspenders alongside the per-substrate UUID).
static SUBSTRATE_SEQ: AtomicU64 = AtomicU64::new(0);

/// An error building the `SQLite` sim substrate.
///
/// Deliberately small and string-backed: the substrate is a test/sim seam, so a
/// self-describing message is more useful to a sim author than a typed error
/// tree, and it keeps the type free of a `diesel`/`deadpool` dependency in its
/// public shape.
#[derive(Debug)]
pub enum SubstrateError {
    /// The guard connection to the in-memory database could not be opened.
    Connection(String),
    /// A registered migration failed to apply.
    Migration(String),
    /// The async runtime pool could not be built over the substrate database.
    Pool(String),
}

impl std::fmt::Display for SubstrateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connection(msg) => write!(f, "sim substrate connection error: {msg}"),
            Self::Migration(msg) => write!(f, "sim substrate migration error: {msg}"),
            Self::Pool(msg) => write!(f, "sim substrate pool error: {msg}"),
        }
    }
}

impl std::error::Error for SubstrateError {}

/// Holds the kept-alive connection that anchors the shared-cache in-memory
/// database for the substrate's lifetime.
///
/// `SQLite` reclaims a shared-cache in-memory database when its **last** connection
/// closes; this guard is that last connection. It is never used for queries after
/// migrations — it exists purely so the async pool's checkouts keep seeing the
/// migrated schema. Dropping the [`SqliteSubstrate`] drops this guard, releasing
/// the in-memory database.
struct SubstrateGuard {
    _conn: diesel::SqliteConnection,
}

/// A fresh, migrated, in-process `SQLite` database for one simulation.
///
/// Construct one per sim via [`SqliteSubstrate::new`] (no migrations) or
/// [`SqliteSubstrate::with_migrations`] (apply a caller-supplied set), then hand
/// [`SqliteSubstrate::pool`] to the app the sim builds. The substrate owns the
/// kept-alive guard connection, so it must outlive any app that uses its pool.
pub struct SqliteSubstrate {
    /// The normalized `file:<unique>?mode=memory&cache=shared` URL both the guard
    /// connection and the pool were opened against.
    url: String,
    /// The async runtime pool over the substrate database. Cloneable and handed
    /// to the app the sim mounts.
    pool: Pool<RuntimeConnection>,
    /// Anchors the shared-cache in-memory database for the substrate's lifetime.
    _guard: SubstrateGuard,
}

impl SqliteSubstrate {
    /// Build a fresh, empty (un-migrated) in-memory `SQLite` substrate.
    ///
    /// Equivalent to [`with_migrations`](Self::with_migrations) with no migration
    /// sets — a legitimate configuration (an app with no registered migrations),
    /// and the cheapest substrate.
    ///
    /// # Errors
    ///
    /// Returns [`SubstrateError`] if the guard connection cannot be opened or the
    /// pool cannot be built.
    pub fn new() -> Result<Self, SubstrateError> {
        Self::with_migrations(&[])
    }

    /// Build a fresh in-memory `SQLite` substrate and apply `migrations` (in order)
    /// against it, so the returned [`pool`](Self::pool) serves a fully-migrated
    /// schema.
    ///
    /// The framework's `SQLite` repository-commit-hook migration set is applied
    /// first (so `Sim::run_to_idle`'s drain never hits a missing
    /// `autumn_repository_commit_hooks` table — see the module docs), then each
    /// caller entry, all through diesel's `MigrationHarness`; the kept-alive guard
    /// connection (opened first) keeps the shared in-memory database — and thus the
    /// applied schema — alive for every later pooled checkout. See the module docs
    /// for why this is safe against a shared-cache in-memory target even though the
    /// framework's own startup path rejects it.
    ///
    /// # Errors
    ///
    /// Returns [`SubstrateError::Connection`] if the guard connection cannot be
    /// opened, [`SubstrateError::Migration`] if a migration fails, or
    /// [`SubstrateError::Pool`] if the runtime pool cannot be built.
    pub fn with_migrations(migrations: &[&EmbeddedMigrations]) -> Result<Self, SubstrateError> {
        let url = unique_sim_db_url();

        // 1. Open the guard connection FIRST so the shared-cache in-memory
        //    database exists and cannot be reclaimed while we migrate + build the
        //    pool. This connection is reused to apply the migrations and then held
        //    for the substrate's lifetime.
        let mut guard_conn = crate::db::establish_sqlite_migration_connection(&url)
            .map_err(|e| SubstrateError::Connection(e.to_string()))?;

        // 2. Apply the framework's SQLite repository-commit-hook migration set
        //    FIRST, so the `autumn_repository_commit_hooks` control-plane table
        //    always exists on the substrate. An app mounted here (which has a DB
        //    pool) is drained by `Sim::run_to_idle`, whose drain does an
        //    unconditional COUNT(*) on that table — without it, `run_to_idle`
        //    panics with "no such table". Because this file lives in the `autumn`
        //    crate it can reference the `pub(crate)`-reachable migration symbol
        //    directly; the whole substrate is `#[cfg(feature = "sqlite")]`, so
        //    `REPOSITORY_COMMIT_HOOK_MIGRATIONS` here is the SQLite variant.
        {
            let mut harness = HarnessWithOutput::write_to_stdout(&mut guard_conn);
            harness
                .run_pending_migrations(EmbeddedMigrationsRef(
                    &crate::repository_commit_hooks::REPOSITORY_COMMIT_HOOK_MIGRATIONS,
                ))
                .map_err(|e| SubstrateError::Migration(e.to_string()))?;
        }

        // 3. Apply each caller-registered migration set directly via the harness.
        //    We bypass `run_pending_sqlite` deliberately: its in-memory reject is a
        //    conservative guard for the "no one is keeping the DB alive" case,
        //    which does not hold here — the guard connection above is exactly what
        //    makes migrating a shared in-memory database safe.
        for set in migrations {
            let mut harness = HarnessWithOutput::write_to_stdout(&mut guard_conn);
            harness
                .run_pending_migrations(EmbeddedMigrationsRef(set))
                .map_err(|e| SubstrateError::Migration(e.to_string()))?;
        }

        // 4. Build the async runtime pool over the SAME shared-cache database. A
        //    single slot keeps the sim deterministic (`SQLite` is single-writer),
        //    and every checkout attaches to the guard-anchored in-memory DB and
        //    sees the migrated schema.
        let config = DatabaseConfig {
            url: Some(url.clone()),
            primary_pool_size: Some(1),
            ..Default::default()
        };
        let pool = crate::db::create_pool(&config)
            .map_err(|e| SubstrateError::Pool(e.to_string()))?
            .ok_or_else(|| SubstrateError::Pool("substrate URL yielded no pool".to_owned()))?;

        Ok(Self {
            url,
            pool,
            _guard: SubstrateGuard { _conn: guard_conn },
        })
    }

    /// Clone the async runtime pool over the substrate database.
    ///
    /// This is what the sim hands to the app it builds (via the
    /// [`TestApp::with_db`](crate::test::TestApp::with_db) DB seam W2's
    /// `Sim::build(TestApp)` consumes). The pool is cheap to clone (an `Arc`
    /// internally); the substrate must outlive every clone, because dropping it
    /// releases the kept-alive guard connection and the in-memory database with
    /// it.
    #[must_use]
    pub fn pool(&self) -> Pool<RuntimeConnection> {
        self.pool.clone()
    }

    /// The `file:<unique>?mode=memory&cache=shared` URL this substrate's database
    /// lives at.
    ///
    /// Useful for wiring the same database into config-driven code paths (e.g. a
    /// `DatabaseConfig.url`) that resolve the pool themselves rather than taking a
    /// pre-built one.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }
}

/// Mint a process-unique shared-cache in-memory `SQLite` URL for one substrate.
///
/// The name combines a fresh UUID with a monotonic sequence number so two
/// substrates never collide, and `mode=memory&cache=shared` makes it an in-memory
/// database shareable across the guard connection and the pool's connections
/// within this process. A distinct name is a fully isolated database, which is
/// what keeps two sims from ever sharing state.
fn unique_sim_db_url() -> String {
    let seq = SUBSTRATE_SEQ.fetch_add(1, Ordering::Relaxed);
    let id = uuid::Uuid::new_v4().simple();
    format!("file:autumn_sim_{id}_{seq}?mode=memory&cache=shared")
}

#[cfg(test)]
mod tests {
    use super::{SqliteSubstrate, unique_sim_db_url};

    // Each minted substrate URL is unique (UUID + sequence), so two substrates
    // are fully isolated databases — the core "two sims never share state"
    // guarantee, verified at the URL level without opening a database.
    #[test]
    fn minted_urls_are_unique_and_shared_cache_in_memory() {
        let a = unique_sim_db_url();
        let b = unique_sim_db_url();
        assert_ne!(a, b, "each substrate must get a distinct database name");
        for url in [&a, &b] {
            assert!(url.starts_with("file:autumn_sim_"), "unexpected url: {url}");
            assert!(url.contains("mode=memory"), "must be in-memory: {url}");
            assert!(url.contains("cache=shared"), "must be shared-cache: {url}");
        }
    }

    // A bare substrate (no migrations) builds and hands out a live pool. This is
    // the boot-free DB lane the sim consumes; it needs a running tokio runtime
    // only to check the pool out, so it is a plain async test.
    #[tokio::test]
    async fn empty_substrate_builds_and_serves_a_pool() {
        use diesel_async::RunQueryDsl as _;

        let substrate = SqliteSubstrate::new().expect("empty substrate builds");
        let pool = substrate.pool();
        let mut conn = pool.get().await.expect("checkout a substrate connection");
        // A trivial query proves the pool is live against the guard-anchored
        // in-memory database.
        diesel::sql_query("SELECT 1")
            .execute(&mut *conn)
            .await
            .expect("substrate pool serves a query");
    }
}
