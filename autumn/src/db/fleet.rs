//! A fleet of `SQLite` databases: one file per tenant, or one per routing slot
//! (ADR 0019).
//!
//! `SQLite` has one writer per database. One database per tenant (or per slot)
//! turns that single process-wide writer into one writer per tenant, gives each
//! tenant a file that can be exported, restored or deleted on its own, and
//! keeps everything on the one volume a host can attach.
//!
//! A [`DatabaseFleet`] opens databases on first use and closes idle ones:
//!
//! ```text
//! open(key)
//!   wait while the key is draining or being deleted
//!   single-flight per key ──► file exists?  no ─► create_on_demand? no ─► 404
//!                              build pool (WAL, busy_timeout, foreign_keys)
//!                              check the identity row (a moved file is refused)
//!                              apply pending migrations (or refuse, see below)
//!                              run on_open hooks (commit-hook worker, replicator)
//!   ◄── FleetDatabase (cheap clone of the pool)
//!
//! close(key)   — LRU past max_open, idle past idle_close, delete, shutdown
//!   remove from the map  ──► stop workers (wait, bounded)
//!   pool.close() (new checkouts fail fast)
//!   wait for in-flight connections ──► on_close hooks (final replication ship)
//! ```
//!
//! # Migrations
//!
//! Every database gets the app's registered migrations and the shard
//! framework sets (version history, commit hooks, derivations) — never the
//! control-plane set, which stays on the control database. A database is
//! migrated when it is created. An existing database is migrated when it is
//! opened if the boot's migration policy auto-applies
//! (`database.auto_migrate` / `auto_migrate_in_production`); otherwise opening
//! one with pending migrations is refused with `503` until `AUTUMN_MIGRATE=1`
//! (which calls [`DatabaseFleet::migrate_all`]) has run. Each database's
//! sequence runs under one `BEGIN IMMEDIATE`, so it is atomic and two
//! processes cannot interleave it.
//!
//! The fleet is therefore mixed during a rollout: code must work against the
//! schema before and after each migration (expand, then contract).
//!
//! # Eviction safety
//!
//! A database is closed only when none of its connections is checked out.
//! Closing calls [`Pool::close`], so a stale clone of the pool can no longer
//! hand out connections, and the `on_close` hooks run only after the last
//! in-flight connection has gone back. A request for a database that is
//! draining waits for the drain, then opens it fresh.
//!
//! # Cancellation
//!
//! `open`, `close`, `delete`, `restore` and `provision` do their work in owned
//! tasks. If the caller is dropped, the work still finishes: a drain always
//! ends, and a started open stays tracked. See ADR 0019, section 3.

// autumn-determinism-gate: production code in this module must read time and
// mint identifiers through the framework's injected seams (ClockSource /
// Entropy), never `Instant::now()` / `Utc::now()` / `SystemTime::now()` /
// `Uuid::new_v4()` directly. See CONTRIBUTING.md "Determinism seam gate"
// (issue #1797). Justify exceptions with
// #[allow(clippy::disallowed_methods, reason = "…")] at the narrowest scope.
#![cfg_attr(not(test), deny(clippy::disallowed_methods))]

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use diesel_async::pooled_connection::deadpool::Pool;
use tokio::sync::{Notify, OnceCell};
use tokio_util::sync::CancellationToken;

use crate::config::DatabaseFleetConfig;
use crate::db::RuntimeConnection;
use crate::error::AutumnError;
use crate::fleet_layout::{FleetDbKey, FleetLayoutError, FleetMode, FleetPathTemplate, TenantDbId};
use crate::migrate::EmbeddedMigrations;

/// How many databases [`DatabaseFleet::each`] and [`DatabaseFleet::migrate_all`]
/// work on at once by default.
pub const DEFAULT_FLEET_CONCURRENCY: usize = 8;

/// How often the idle sweeper looks for databases to close, at most.
const MAX_SWEEP_INTERVAL: Duration = Duration::from_secs(30);

/// How long a database stays open after its last use before LRU may close it.
///
/// A request resolves its database, then checks a connection out a moment
/// later (a generated repository even later, lazily); without this grace a
/// burst of opens could close a database between the two and fail the
/// request with a closed pool.
pub const DEFAULT_MIN_RESIDENCY: Duration = Duration::from_secs(2);

/// How often a drain checks whether the last in-flight connection is back.
const DRAIN_POLL: Duration = Duration::from_millis(20);

/// How long a close waits for the workers of one database to stop. Workers
/// of different databases stop together, so this also bounds a shutdown.
pub const WORKER_QUIESCE_TIMEOUT: Duration = Duration::from_secs(10);

/// The table that records which database a file is.
const IDENTITY_TABLE: &str = "_autumn_fleet_identity";

// ── Errors ───────────────────────────────────────────────────────────────────

/// Why a fleet operation failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum FleetError {
    /// The routing key cannot name a database (`400`).
    #[error(transparent)]
    InvalidKey(#[from] FleetLayoutError),
    /// The database does not exist and is not created on demand (`404`).
    #[error("fleet database {name} does not exist; provision it first")]
    NotFound {
        /// The database name.
        name: String,
    },
    /// [`DatabaseFleet::provision`] was asked for a database that exists (`409`).
    #[error("fleet database {name} already exists")]
    AlreadyExists {
        /// The database name.
        name: String,
    },
    /// The database is being deleted (`410`).
    #[error("fleet database {name} is being deleted")]
    Deleting {
        /// The database name.
        name: String,
    },
    /// The database has migrations this boot will not apply (`503`).
    #[error(
        "fleet database {name} has {count} pending migration(s) and this boot does not \
         auto-apply them; run the app once with AUTUMN_MIGRATE=1"
    )]
    PendingMigrations {
        /// The database name.
        name: String,
        /// Number of pending migrations.
        count: usize,
    },
    /// The file at this key's path belongs to another key (`500`): it was
    /// copied or moved by hand. Serving it would show one tenant another's data.
    #[error(
        "fleet database file for {name} identifies itself as {found}; refusing to serve it \
         (was the file copied or renamed?)"
    )]
    Misplaced {
        /// The key that was opened.
        name: String,
        /// The name recorded inside the file.
        found: String,
    },
    /// A migration failed (`503`).
    #[error("fleet database {name}: {detail}")]
    Migration {
        /// The database name.
        name: String,
        /// Redacted detail.
        detail: String,
    },
    /// The pool could not be built or a connection could not be made (`503`).
    #[error("fleet database {name}: {detail}")]
    Unavailable {
        /// The database name.
        name: String,
        /// Detail.
        detail: String,
    },
    /// Another process sharing the fleet root has the database open, so it
    /// cannot be deleted or replaced from here (`409`).
    #[error(
        "fleet database {name} is open in another process; close it there (or stop that \
         process) before deleting or restoring it"
    )]
    InUseElsewhere {
        /// The database name.
        name: String,
    },
    /// Restoring the database from its replica failed (`503`).
    #[error("fleet database {name}: restore failed: {detail}")]
    Restore {
        /// The database name.
        name: String,
        /// Detail.
        detail: String,
    },
    /// The fleet does not replicate, so there is nothing to restore from (`503`).
    #[error("fleet database {name}: the fleet is not replicated ([replication] is off)")]
    NotReplicated {
        /// The database name.
        name: String,
    },
    /// This host does not own the key's slot at the current epoch (`421`).
    /// Another host serves it, or this host waits for the slot's lease.
    #[error("fleet database {name}: this host does not own slot {slot}")]
    NotOwner {
        /// The database name.
        name: String,
        /// The routing slot.
        slot: u16,
    },
    /// A file operation failed (`500`).
    #[error("fleet {op}: {detail}")]
    Io {
        /// What was being attempted.
        op: &'static str,
        /// I/O detail.
        detail: String,
    },
}

impl FleetError {
    /// The HTTP status a request that hit this error answers with. Applied
    /// by [`AutumnError`]'s `From` impl, so `?` on a fleet call in a handler
    /// produces it.
    #[must_use]
    pub const fn http_status(&self) -> http::StatusCode {
        use http::StatusCode;
        match self {
            Self::InvalidKey(_) => StatusCode::BAD_REQUEST,
            Self::NotFound { .. } => StatusCode::NOT_FOUND,
            Self::AlreadyExists { .. } | Self::InUseElsewhere { .. } => StatusCode::CONFLICT,
            Self::Deleting { .. } => StatusCode::GONE,
            Self::NotOwner { .. } => StatusCode::MISDIRECTED_REQUEST,
            Self::PendingMigrations { .. }
            | Self::Migration { .. }
            | Self::Unavailable { .. }
            | Self::Restore { .. }
            | Self::NotReplicated { .. } => StatusCode::SERVICE_UNAVAILABLE,
            Self::Misplaced { .. } | Self::Io { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

fn io_error(op: &'static str) -> impl FnOnce(std::io::Error) -> FleetError {
    move |e| FleetError::Io {
        op,
        detail: e.to_string(),
    }
}

// ── Public handle types ──────────────────────────────────────────────────────

/// One open database in the fleet. Cheap to clone.
///
/// Every live handle (and every clone) is a lease: while any exists, idle
/// and `max_open` eviction skip the database, so a pool in use is never
/// closed under its holder. Hold a handle for a request or a task, never in
/// a long-lived cache: each retained handle pins one open database, and
/// enough of them defeat `max_open` and `idle_close_secs`. Call
/// [`DatabaseFleet::open`] again instead; it is cheap while the database is
/// open.
///
/// An explicit [`DatabaseFleet::close`], [`DatabaseFleet::delete`] or
/// [`DatabaseFleet::restore`] does not wait for leases. [`DatabaseFleet::delete`]
/// and [`DatabaseFleet::restore`] stop the workers and close the pool at once.
/// [`DatabaseFleet::close`] first lets the workers finish their current hook
/// (for at most [`WORKER_QUIESCE_TIMEOUT`]), then closes the pool. After
/// that the pool refuses new checkouts and the next [`DatabaseFleet::open`]
/// opens the database fresh.
#[derive(Clone)]
pub struct FleetDatabase {
    key: FleetDbKey,
    path: Arc<Path>,
    pool: Pool<RuntimeConnection>,
    closed: CancellationToken,
    /// Counts the holders of this database. The fleet's own copy is one; any
    /// more and the database is in use, so it is never closed.
    lease: Arc<()>,
    /// The ownership epoch this database opened under. `None` without
    /// ownership.
    epoch: Option<crate::lock::FencingToken>,
    /// Background work to stop before the pool closes. `None` once the stop
    /// has begun: a later registration is stopped at once.
    workers: Arc<Mutex<Option<Vec<Worker>>>>,
}

/// A task registered with [`FleetDatabase::register_worker`].
struct Worker {
    stop: CancellationToken,
    handle: tokio::task::JoinHandle<()>,
}

impl FleetDatabase {
    /// Which database this is.
    #[must_use]
    pub const fn key(&self) -> &FleetDbKey {
        &self.key
    }

    /// The database file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The ownership epoch this database opened under (issue #3233). `None`
    /// when the fleet has no ownership.
    #[must_use]
    pub const fn epoch(&self) -> Option<crate::lock::FencingToken> {
        self.epoch
    }

    /// The database's connection pool.
    #[must_use]
    pub const fn pool(&self) -> &Pool<RuntimeConnection> {
        &self.pool
    }

    /// A lease that keeps this database open while it is held. See
    /// [`ShardLease`](crate::sharding::ShardLease).
    #[must_use]
    pub fn lease(&self) -> crate::sharding::ShardLease {
        Arc::clone(&self.lease) as crate::sharding::ShardLease
    }

    /// Cancelled when the fleet closes this database. Background work tied to
    /// one database (its commit-hook worker) stops on it.
    #[must_use]
    pub const fn closed(&self) -> &CancellationToken {
        &self.closed
    }

    /// Register background work that uses this database's pool. Before the
    /// pool closes, the fleet cancels `stop` and awaits `handle` (for at most
    /// [`WORKER_QUIESCE_TIMEOUT`]), so the work can finish its current step
    /// while the pool still hands out connections.
    ///
    /// Keep `stop` apart from [`closed`](Self::closed): that token means the
    /// pool is closed.
    pub fn register_worker(&self, stop: CancellationToken, handle: tokio::task::JoinHandle<()>) {
        let mut workers = self
            .workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(workers) = workers.as_mut() {
            workers.push(Worker { stop, handle });
        } else {
            // Too late to settle: the pool is about to close.
            stop.cancel();
            handle.abort();
        }
    }

    /// Stop the registered workers. With `settle`, wait for them within the
    /// timeout; without it, abort them now.
    async fn quiesce_workers(&self, settle: bool) {
        let workers = self
            .workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .unwrap_or_default();
        for worker in &workers {
            worker.stop.cancel();
        }
        if !settle {
            for worker in &workers {
                worker.handle.abort();
            }
            return;
        }
        let key = &self.key;
        let aborts: Vec<_> = workers.iter().map(|w| w.handle.abort_handle()).collect();
        let settle = futures::future::join_all(workers.into_iter().map(|w| w.handle));
        if tokio::time::timeout(WORKER_QUIESCE_TIMEOUT, settle)
            .await
            .is_err()
        {
            tracing::warn!(database = %key, "a fleet worker did not stop in time; aborting it");
            // A detached task would ack against a closed pool.
            for abort in aborts {
                abort.abort();
            }
        }
    }
}

impl fmt::Debug for FleetDatabase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FleetDatabase")
            .field("key", &self.key)
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

/// Work that follows a fleet database's lifecycle. Installed with
/// [`DatabaseFleet::add_lifecycle`].
pub trait FleetLifecycle: Send + Sync + 'static {
    /// Before opening a database whose file is missing. Return `Ok(true)`
    /// when the hook produced the file (a restore from a replica); the open
    /// then treats it as an existing database. Runs on a blocking thread. An
    /// error refuses the open.
    ///
    /// The fleet holds the path lock while the hook runs, and has cleared the
    /// stray sidecars of the absent database first. Other keys that share the
    /// lock file wait, so do the slow work (a download) only as needed.
    ///
    /// # Errors
    ///
    /// A message that becomes [`FleetError::Restore`].
    fn restore_missing(&self, key: &FleetDbKey, path: &Path) -> Result<bool, String> {
        let _ = (key, path);
        Ok(false)
    }

    /// After the database is migrated, before its first use. Runs on a
    /// blocking thread. An error refuses the open.
    ///
    /// # Errors
    ///
    /// A message that becomes [`FleetError::Unavailable`].
    fn on_open(&self, db: &FleetDatabase) -> Result<(), String> {
        let _ = db;
        Ok(())
    }

    /// After every connection of the database has been closed (`db`'s pool
    /// is closed and empty). Runs on a blocking thread. A replicator ships
    /// its last frames here.
    fn on_close(&self, db: &FleetDatabase) {
        let _ = db;
    }

    /// Before a delete or a restore touches the closed database's files.
    /// Let go of anything that holds them open (a parked replicator does).
    fn release(&self, key: &FleetDbKey) {
        let _ = key;
    }

    /// After a delete has removed the database's files. Not called when the
    /// delete was refused or found nothing to delete.
    fn on_delete(&self, key: &FleetDbKey) {
        let _ = key;
    }
}

/// Counters for health and metrics. See [`DatabaseFleet::stats`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct FleetStats {
    /// Databases open now.
    pub open: usize,
    /// Databases being closed now.
    pub draining: usize,
    /// The configured cap.
    pub max_open: usize,
    /// Databases opened since boot.
    pub opens_total: u64,
    /// Databases closed since boot (LRU, idle, delete).
    pub closes_total: u64,
    /// Opens that failed since boot.
    pub open_failures_total: u64,
    /// Migrations applied by this process since boot.
    pub migrations_applied_total: u64,
    /// Databases created since boot.
    pub created_total: u64,
    /// Databases deleted since boot.
    pub deleted_total: u64,
}

/// What [`DatabaseFleet::inspect`] reads from a database file.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct FleetDbInfo {
    /// The name the file records. `None` until the fleet first opens it.
    pub identity: Option<String>,
    /// The applied migration versions, sorted. Empty before any migration.
    pub applied_migrations: Vec<String>,
}

/// The outcome of [`DatabaseFleet::migrate_all`].
#[derive(Debug, Default)]
pub struct FleetMigrationReport {
    /// Databases visited.
    pub databases: usize,
    /// Migrations applied, summed over the fleet.
    pub applied: usize,
    /// Databases that failed, with the reason. The others were migrated.
    pub failed: Vec<(FleetDbKey, FleetError)>,
}

// ── Migrations ───────────────────────────────────────────────────────────────

/// A migration source a fleet database can receive: an embedded set, or the
/// app's `migrations/` directory read from disk (the CLI).
type SqliteSource = dyn diesel::migration::MigrationSource<diesel::sqlite::Sqlite> + Send + Sync;

/// One migration set of a fleet. Cheap to clone.
#[derive(Clone)]
struct FleetSet(Arc<SqliteSource>);

impl diesel::migration::MigrationSource<diesel::sqlite::Sqlite> for FleetSet {
    fn migrations(
        &self,
    ) -> diesel::migration::Result<Vec<Box<dyn diesel::migration::Migration<diesel::sqlite::Sqlite>>>>
    {
        self.0.migrations()
    }
}

/// One embedded set of the app's shared list.
struct SharedEmbedded {
    all: Arc<Vec<(&'static str, EmbeddedMigrations)>>,
    idx: usize,
}

impl diesel::migration::MigrationSource<diesel::sqlite::Sqlite> for SharedEmbedded {
    fn migrations(
        &self,
    ) -> diesel::migration::Result<Vec<Box<dyn diesel::migration::Migration<diesel::sqlite::Sqlite>>>>
    {
        diesel::migration::MigrationSource::<diesel::sqlite::Sqlite>::migrations(
            &self.all[self.idx].1,
        )
    }
}

/// The migration sets every fleet database receives, with the collision map
/// computed once over them.
pub(crate) struct FleetMigrations {
    sets: Vec<FleetSet>,
    disambiguated: HashMap<String, String>,
    history: Vec<Vec<(String, String)>>,
    /// Whether opening an existing database applies its pending migrations.
    apply_on_open: bool,
    /// Whether a database that records a version none of the sets has is
    /// refused. See [`DatabaseFleetBuilder::refuse_unknown_migrations`].
    refuse_unknown: bool,
}

impl FleetMigrations {
    /// The sets of `all` that `keep` accepts. The app passes a filter that
    /// drops the control-plane framework set.
    pub(crate) fn new(
        all: &Arc<Vec<(&'static str, EmbeddedMigrations)>>,
        keep: impl Fn(&EmbeddedMigrations) -> bool,
        apply_on_open: bool,
    ) -> Self {
        let sets = all
            .iter()
            .enumerate()
            .filter(|(_, (_, set))| keep(set))
            .map(|(idx, _)| {
                FleetSet(Arc::new(SharedEmbedded {
                    all: Arc::clone(all),
                    idx,
                }))
            })
            .collect();
        Self::from_sets(sets, apply_on_open)
    }

    /// Use `sets` unchanged. The collision map uses only migration names, so
    /// an on-disk set and the same embedded set get the same map.
    fn from_sets(sets: Vec<FleetSet>, apply_on_open: bool) -> Self {
        let history: Vec<Vec<(String, String)>> = sets
            .iter()
            .filter_map(|set| {
                crate::migrate::migration_versions_and_names::<diesel::sqlite::Sqlite, _>(set).ok()
            })
            .collect();
        let disambiguated =
            crate::migrate::compute_migration_disambiguation_from_names(history.clone());
        Self {
            sets,
            disambiguated,
            history,
            apply_on_open,
            refuse_unknown: false,
        }
    }

    /// The versions the sets record, substitutes included. Sorted.
    fn known_versions(&self) -> Vec<String> {
        let mut versions: Vec<String> = self
            .history
            .iter()
            .flatten()
            .map(|(version, name)| {
                self.disambiguated
                    .get(name)
                    .cloned()
                    .unwrap_or_else(|| version.clone())
            })
            .collect();
        versions.sort();
        versions.dedup();
        versions
    }

    /// Migrations in the selected sets, which a new database receives.
    fn count(&self) -> usize {
        use diesel::migration::MigrationSource;
        self.sets
            .iter()
            .map(|set| {
                MigrationSource::<diesel::sqlite::Sqlite>::migrations(set).map_or(0, |m| m.len())
            })
            .sum()
    }

    /// Apply every pending migration. Blocking.
    fn apply(&self, url: &str, name: &str) -> Result<usize, FleetError> {
        let fail = |e: crate::migrate::MigrationError| FleetError::Migration {
            name: name.to_owned(),
            detail: crate::db_url::redact_driver_error(&e.to_string(), url),
        };
        crate::migrate::adopt_sqlite_collision_history(url, &self.history, &self.disambiguated)
            .map_err(fail)?;
        // Every set in one run, under one `BEGIN IMMEDIATE`: a failure in a
        // later set rolls back the earlier ones too, so a database is never
        // left part-way through an upgrade.
        let known = self.refuse_unknown.then(|| self.known_versions());
        // The check reads the file the run migrates, under its write lock.
        let guard = |recorded: &[String]| {
            let unknown: Vec<&str> = known.as_ref().map_or_else(Vec::new, |known| {
                recorded
                    .iter()
                    .filter(|v| known.binary_search(v).is_err())
                    .map(String::as_str)
                    .collect()
            });
            if unknown.is_empty() {
                return Ok(());
            }
            Err(format!(
                "records migrations that none of the sets has: {}",
                unknown.join(", ")
            ))
        };
        let result =
            crate::migrate::run_pending_sqlite_guarded(url, self.chained(), guard).map_err(fail)?;
        for migration in &result.applied {
            tracing::info!(database = %name, migration = %migration, "fleet migration applied");
        }
        Ok(result.applied.len())
    }

    /// The selected sets as one migration source.
    fn chained(&self) -> ChainedMigrations<'_> {
        ChainedMigrations(
            self.sets
                .iter()
                .map(|set| crate::migrate::DisambiguatedMigrations::new(set, &self.disambiguated))
                .collect(),
        )
    }

    /// Count pending migrations without applying them. Blocking.
    fn pending(&self, url: &str, name: &str) -> Result<usize, FleetError> {
        crate::migrate::pending_migrations_sqlite(url, self.chained())
            .map(|pending| pending.len())
            .map_err(|e| FleetError::Migration {
                name: name.to_owned(),
                detail: crate::db_url::redact_driver_error(&e.to_string(), url),
            })
    }
}

/// Several migration sets read as one source, so diesel runs them in one
/// pass (in version order) under one transaction.
struct ChainedMigrations<'a>(Vec<crate::migrate::DisambiguatedMigrations<'a, FleetSet>>);

impl diesel::migration::MigrationSource<diesel::sqlite::Sqlite> for ChainedMigrations<'_> {
    fn migrations(
        &self,
    ) -> diesel::migration::Result<Vec<Box<dyn diesel::migration::Migration<diesel::sqlite::Sqlite>>>>
    {
        let mut all = Vec::new();
        for source in &self.0 {
            all.extend(
                diesel::migration::MigrationSource::<diesel::sqlite::Sqlite>::migrations(source)?,
            );
        }
        Ok(all)
    }
}

// ── The fleet ────────────────────────────────────────────────────────────────

/// One entry of the open map. `cell` is filled once by the entry's single
/// owned opener task, which sets `finished` when it ends.
struct Entry {
    cell: OnceCell<FleetDatabase>,
    finished: AtomicBool,
    done: Notify,
    last_use_seq: AtomicU64,
    last_use_at: Mutex<std::time::Instant>,
}

/// The use mark of an entry only background opens used. Foreground uses
/// count from 1, so such an entry is the first LRU victim.
const BACKGROUND_SEQ: u64 = 0;

/// A database [`DatabaseFleet::open_background`] returned.
#[derive(Debug)]
pub struct BackgroundOpen {
    /// The database. Holding it is a lease.
    pub db: FleetDatabase,
    /// Whether this call opened it (it was not open before).
    pub opened: bool,
}

/// A database leaving the open map: its key, its handle, its drain marker.
type Draining = (FleetDbKey, Option<FleetDatabase>, Arc<Drain>);

/// A key taken out of service: its open entry, if any, and its drain marker.
type Exclusive = (FleetDbKey, Option<Arc<Entry>>, Arc<Drain>);

/// What an opener does next: wait for a drain, or open through an entry.
enum Next {
    Wait(Arc<Drain>),
    /// A background open found the fleet full.
    NoRoom,
    /// The entry, and whether this caller made it and so must start its opener.
    Open(Arc<Entry>, bool),
}

/// Counts an owned mutation until it ends, so `close_all` can wait for it.
/// Made before the task is spawned: no window exists where it is unseen.
struct MutationGuard(DatabaseFleet);

impl MutationGuard {
    fn new(fleet: &DatabaseFleet) -> Self {
        fleet.inner.mutations.fetch_add(1, Ordering::AcqRel);
        Self(fleet.clone())
    }
}

impl Drop for MutationGuard {
    fn drop(&mut self) {
        self.0.inner.mutations.fetch_sub(1, Ordering::AcqRel);
        self.0.inner.mutations_done.notify_waiters();
    }
}

/// Ends an open: forgets the entry if it holds no database, then wakes the
/// waiters. A dropped guard (a panic) does the same.
struct OpenGuard {
    fleet: DatabaseFleet,
    key: FleetDbKey,
    entry: Arc<Entry>,
}

impl Drop for OpenGuard {
    fn drop(&mut self) {
        if self.entry.cell.get().is_none() {
            self.fleet.forget_entry(&self.key, &self.entry);
        }
        self.entry.finished.store(true, Ordering::Release);
        self.entry.done.notify_waiters();
    }
}

/// Finishes a drain when dropped, so a panic cannot strand the key.
struct DrainGuard<'a> {
    fleet: &'a DatabaseFleet,
    key: FleetDbKey,
    drain: Arc<Drain>,
}

impl Drop for DrainGuard<'_> {
    fn drop(&mut self) {
        self.fleet.finish_drain(&self.key, &self.drain);
    }
}

/// A key that is closing or being deleted. Openers wait on `done`.
struct Drain {
    deleting: bool,
    finished: AtomicBool,
    done: Notify,
}

impl Drain {
    async fn wait(&self) {
        loop {
            let notified = self.done.notified();
            if self.finished.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }
}

#[derive(Default)]
struct State {
    open: HashMap<FleetDbKey, Arc<Entry>>,
    draining: HashMap<FleetDbKey, Arc<Drain>>,
    /// Serializes the in-process provisions of one key.
    provisioning: HashMap<FleetDbKey, Arc<tokio::sync::Mutex<()>>>,
    /// Keys a provision created but no caller has seen succeed yet.
    unacknowledged: HashSet<FleetDbKey>,
    seq: u64,
    /// Set by `close_all`. No entry is claimed after it.
    shutting_down: bool,
}

#[derive(Default)]
struct Counters {
    opens: AtomicU64,
    closes: AtomicU64,
    open_failures: AtomicU64,
    migrations_applied: AtomicU64,
    created: AtomicU64,
    deleted: AtomicU64,
}

struct Inner {
    root: PathBuf,
    template: FleetPathTemplate,
    max_open: usize,
    pool_size: usize,
    create_on_demand: bool,
    idle_close: Option<Duration>,
    min_residency: Duration,
    connect_timeout_secs: u64,
    migrations: FleetMigrations,
    replicating: AtomicBool,
    replication: std::sync::OnceLock<Arc<super::fleet_replication::FleetReplication>>,
    ownership: std::sync::OnceLock<Arc<super::fleet_ownership::FleetOwnership>>,
    /// The shared volume lock held while ownership is installed.
    volume_lock: Mutex<Option<VolumeLock>>,
    state: Mutex<State>,
    lifecycle: std::sync::RwLock<Vec<Arc<dyn FleetLifecycle>>>,
    counters: Counters,
    in_flight_drains: AtomicUsize,
    /// Owned `delete`, `restore` and `provision` tasks that have not ended.
    mutations: AtomicUsize,
    mutations_done: Notify,
}

/// A fleet of `SQLite` databases. Cheap to clone. See the module docs.
#[derive(Clone)]
pub struct DatabaseFleet {
    inner: Arc<Inner>,
}

impl fmt::Debug for DatabaseFleet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DatabaseFleet")
            .field("mode", &self.mode())
            .field("root", &self.inner.root)
            .field("path", &self.inner.template.as_str())
            .finish_non_exhaustive()
    }
}

/// Builder for a [`DatabaseFleet`] outside app boot (tests, tools).
pub struct DatabaseFleetBuilder {
    config: DatabaseFleetConfig,
    min_residency: Duration,
    connect_timeout_secs: u64,
    migrations: Vec<FleetSet>,
    apply_on_open: bool,
    refuse_unknown: bool,
}

impl DatabaseFleetBuilder {
    /// Add a migration set every database receives.
    #[must_use]
    pub fn migrations(self, name: &'static str, set: EmbeddedMigrations) -> Self {
        self.migration_source(name, set)
    }

    /// Add a migration source every database receives, for example the app's
    /// `migrations/` directory as `FileBasedMigrations` (`autumn fleet
    /// migrate` does this). `name` is not used.
    #[must_use]
    pub fn migration_source(
        mut self,
        _name: &'static str,
        source: impl diesel::migration::MigrationSource<diesel::sqlite::Sqlite> + Send + Sync + 'static,
    ) -> Self {
        self.migrations.push(FleetSet(Arc::new(source)));
        self
    }

    /// Add the framework sets a fleet database receives at boot: version
    /// history, the commit-hook queue and derivations. Never the
    /// control-plane set.
    #[must_use]
    pub fn framework_migrations(mut self) -> Self {
        for set in crate::migrate::shard_framework_migration_sets() {
            self.migrations
                .push(FleetSet(Arc::new(crate::migrate::EmbeddedMigrationsRef(
                    set,
                ))));
        }
        self
    }

    /// Whether opening an existing database applies its pending migrations.
    /// Default: `true`.
    #[must_use]
    pub const fn apply_migrations_on_open(mut self, apply: bool) -> Self {
        self.apply_on_open = apply;
        self
    }

    /// Refuse to migrate a database that records a version none of the sets
    /// has (a plugin's set, say). The check reads the migrated file under its
    /// write lock. For a tool with only some of the app's sets (`autumn fleet
    /// migrate`). Default: `false`.
    #[must_use]
    pub const fn refuse_unknown_migrations(mut self, refuse: bool) -> Self {
        self.refuse_unknown = refuse;
        self
    }

    /// How long a database stays open after its last use before LRU may
    /// close it. Default: [`DEFAULT_MIN_RESIDENCY`].
    #[must_use]
    pub const fn min_residency(mut self, residency: Duration) -> Self {
        self.min_residency = residency;
        self
    }

    /// Connection timeout of each database's pool. Default: 5 s.
    #[must_use]
    pub const fn connect_timeout_secs(mut self, secs: u64) -> Self {
        self.connect_timeout_secs = secs;
        self
    }

    /// Build the fleet, creating the root directory.
    ///
    /// # Errors
    ///
    /// [`FleetError::InvalidKey`] for a bad template, [`FleetError::Io`] when
    /// the root cannot be created.
    pub fn build(self) -> Result<DatabaseFleet, FleetError> {
        let mut migrations = FleetMigrations::from_sets(self.migrations, self.apply_on_open);
        migrations.refuse_unknown = self.refuse_unknown;
        DatabaseFleet::from_parts(
            &self.config,
            self.connect_timeout_secs,
            migrations,
            self.min_residency,
        )
    }
}

impl DatabaseFleet {
    /// Start building a fleet from its configuration section.
    #[must_use]
    pub const fn builder(config: DatabaseFleetConfig) -> DatabaseFleetBuilder {
        DatabaseFleetBuilder {
            config,
            min_residency: DEFAULT_MIN_RESIDENCY,
            connect_timeout_secs: 5,
            migrations: Vec::new(),
            apply_on_open: true,
            refuse_unknown: false,
        }
    }

    pub(crate) fn from_parts(
        config: &DatabaseFleetConfig,
        connect_timeout_secs: u64,
        migrations: FleetMigrations,
        min_residency: Duration,
    ) -> Result<Self, FleetError> {
        let template = FleetPathTemplate::parse(config.path_template(), config.mode)?;
        std::fs::create_dir_all(&config.root).map_err(io_error("create root directory"))?;
        // Canonical, so the containment check in `path_of` compares like with
        // like and a relative root does not move with the working directory.
        let root = std::fs::canonicalize(&config.root).map_err(io_error("resolve root"))?;
        Ok(Self {
            inner: Arc::new(Inner {
                root,
                template,
                max_open: config.max_open.max(1),
                pool_size: config.pool_size.max(1),
                create_on_demand: config.effective_create_on_demand(),
                idle_close: (config.idle_close_secs > 0)
                    .then(|| Duration::from_secs(config.idle_close_secs)),
                min_residency,
                connect_timeout_secs,
                migrations,
                replicating: AtomicBool::new(false),
                replication: std::sync::OnceLock::new(),
                ownership: std::sync::OnceLock::new(),
                volume_lock: Mutex::new(None),
                state: Mutex::new(State::default()),
                lifecycle: std::sync::RwLock::new(Vec::new()),
                counters: Counters::default(),
                in_flight_drains: AtomicUsize::new(0),
                mutations: AtomicUsize::new(0),
                mutations_done: Notify::new(),
            }),
        })
    }

    /// What one database holds.
    #[must_use]
    pub fn mode(&self) -> FleetMode {
        self.inner.template.mode()
    }

    /// The canonical root directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    /// The configured cap on open databases.
    #[must_use]
    pub fn max_open(&self) -> usize {
        self.inner.max_open
    }

    /// Connections per open database.
    #[must_use]
    pub fn pool_size(&self) -> usize {
        self.inner.pool_size
    }

    /// Install lifecycle hooks. Install before the first open: a database
    /// already open does not see `on_open` again.
    pub fn add_lifecycle(&self, hooks: Arc<dyn FleetLifecycle>) {
        self.inner
            .lifecycle
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(hooks);
    }

    fn hooks(&self) -> Vec<Arc<dyn FleetLifecycle>> {
        self.inner
            .lifecycle
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Replicate every database this fleet opens from now on (ADR 0019 §5).
    ///
    /// Databases open with `wal_autocheckpoint = 0` and `replication` is
    /// their only checkpointer; it registers on open and ships the last frames
    /// on close. Install before the first open, once.
    ///
    /// # Errors
    ///
    /// The replication that is already installed, when called twice.
    pub fn install_replication(
        &self,
        replication: Arc<super::fleet_replication::FleetReplication>,
    ) -> Result<(), Arc<super::fleet_replication::FleetReplication>> {
        self.inner.replication.set(Arc::clone(&replication))?;
        self.inner.replicating.store(true, Ordering::Release);
        self.add_lifecycle(replication);
        self.wire_fence_hook();
        Ok(())
    }

    /// The installed replication, if any.
    #[must_use]
    pub fn replication(&self) -> Option<&Arc<super::fleet_replication::FleetReplication>> {
        self.inner.replication.get()
    }

    /// Whether databases are opened for an external checkpointer.
    #[must_use]
    pub fn replicating(&self) -> bool {
        self.inner.replicating.load(Ordering::Acquire)
    }

    /// Serve only the slots `ownership` holds (issue #3233). Install before
    /// the first open, once, after a first
    /// [`refresh`](super::fleet_ownership::FleetOwnership::refresh).
    ///
    /// The process also takes a shared OS lock on the root, so that
    /// [`release_ownership`](Self::release_ownership) knows when it is the
    /// last process on the volume.
    ///
    /// # Errors
    ///
    /// The ownership that is already installed, when called twice.
    pub fn install_ownership(
        &self,
        ownership: Arc<super::fleet_ownership::FleetOwnership>,
    ) -> Result<(), Arc<super::fleet_ownership::FleetOwnership>> {
        self.inner.ownership.set(ownership)?;
        match VolumeLock::shared(self.root()) {
            Ok(lock) => {
                *self
                    .inner
                    .volume_lock
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(lock);
            }
            Err(error) => {
                tracing::warn!(%error, "fleet ownership: no volume lock; ranges lapse instead of being released");
            }
        }
        self.wire_fence_hook();
        Ok(())
    }

    /// Release this host's ranges at shutdown, after the databases closed,
    /// so the next owner can claim them at once. Returns whether it released.
    ///
    /// It releases only when this is the last process on the volume (the
    /// processes share the owner id and the epoch) and nothing committed is
    /// left unshipped. Otherwise the leases lapse.
    pub async fn release_ownership(&self) -> bool {
        let Some(ownership) = self.ownership() else {
            return false;
        };
        if self.replication().is_some_and(|r| r.closing() > 0) {
            tracing::warn!("fleet ownership: unshipped frames; the ranges lapse instead");
            return false;
        }
        let shared = self
            .inner
            .volume_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if shared.is_none() {
            return false;
        }
        drop(shared);
        let root = self.root().to_path_buf();
        let last = crate::time::spawn_blocking(move || VolumeLock::exclusive(&root))
            .await
            .ok()
            .and_then(Result::ok)
            .flatten();
        let Some(_last) = last else {
            tracing::info!("fleet ownership: another process on this volume serves; not released");
            return false;
        };
        ownership.release_all().await;
        true
    }

    /// With both ownership and replication: a replicator that sees a newer
    /// epoch drops this host's range, and the ownership loop closes it.
    fn wire_fence_hook(&self) {
        let (Some(replication), Some(ownership)) = (self.replication(), self.ownership()) else {
            return;
        };
        let ownership = Arc::downgrade(ownership);
        replication.set_fence_hook(Arc::new(move |key: &FleetDbKey, newest: u64| {
            let newer = i64::try_from(newest)
                .ok()
                .and_then(|n| crate::lock::FencingToken::try_from(n).ok());
            if let (Some(ownership), Some(newer)) = (ownership.upgrade(), newer) {
                ownership.lose(key.routing_slot(), newer);
            }
        }));
    }

    /// The epoch to open `key` under: `None` without ownership.
    fn owned_epoch(
        &self,
        key: &FleetDbKey,
    ) -> Result<Option<crate::lock::FencingToken>, FleetError> {
        let Some(ownership) = self.ownership() else {
            return Ok(None);
        };
        let slot = key.routing_slot();
        ownership
            .epoch_for(slot)
            .map(Some)
            .ok_or_else(|| FleetError::NotOwner {
                name: key.name(),
                slot,
            })
    }

    /// The installed ownership, if any.
    #[must_use]
    pub fn ownership(&self) -> Option<&Arc<super::fleet_ownership::FleetOwnership>> {
        self.inner.ownership.get()
    }

    /// Whether this host may serve `key` now. Always `true` without
    /// ownership.
    #[must_use]
    pub fn owns(&self, key: &FleetDbKey) -> bool {
        self.owned_epoch(key).is_ok()
    }

    /// Begin to close each open database whose slot this host no longer
    /// holds at the epoch it opened under. Returns how many began closing.
    #[must_use = "the count is how many databases began closing"]
    pub fn fence_unowned(&self) -> usize {
        let Some(ownership) = self.ownership() else {
            return 0;
        };
        let victims = self.begin_drains(|state| {
            state
                .open
                .iter()
                .filter(|(key, entry)| {
                    entry
                        .cell
                        .get()
                        .is_some_and(|db| ownership.epoch_for(key.routing_slot()) != db.epoch)
                })
                .map(|(key, _)| key.clone())
                .collect()
        });
        let fenced = victims.len();
        for (key, db, drain) in victims {
            tracing::warn!(database = %key, "fleet database closed: this host lost its slot");
            // No settle: the data is not this host's any more.
            self.spawn_drain(key, db, drain, false);
        }
        fenced
    }

    /// Renew this host's ranges and close what it lost, until `shutdown`
    /// (issue #3233). It wakes every renew interval, when a local lease runs
    /// out, and when a replicator sees a newer epoch. A no-op without
    /// ownership.
    #[must_use = "await the handle at shutdown"]
    pub fn spawn_ownership(&self, shutdown: CancellationToken) -> tokio::task::JoinHandle<()> {
        let fleet = self.clone();
        tokio::spawn(async move {
            let Some(ownership) = fleet.ownership().cloned() else {
                return;
            };
            loop {
                let changed = ownership.changed().notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                // Close a lapsed range first: the store may hang (a real
                // partition), and the local lease must still end on time.
                let _ = fleet.fence_unowned();
                let refresh = ownership.refresh();
                tokio::pin!(refresh);
                loop {
                    let lapse = ownership.next_lapse();
                    tokio::select! {
                        _ = &mut refresh => break,
                        () = sleep_until_some(lapse) => {
                            let _ = fleet.fence_unowned();
                        }
                    }
                }
                let _ = fleet.fence_unowned();
                let wake = ownership.next_wake();
                tokio::select! {
                    () = shutdown.cancelled() => break,
                    () = tokio::time::sleep_until(wake) => {}
                    () = &mut changed => {}
                }
            }
        })
    }

    // ── Keys ─────────────────────────────────────────────────────────────

    /// The database key for a routing key (a tenant id), per the fleet mode.
    ///
    /// # Errors
    ///
    /// [`FleetError::InvalidKey`] when a tenant fleet is given an id that
    /// cannot name a file.
    pub fn key_for(&self, routing_key: &str) -> Result<FleetDbKey, FleetError> {
        let slot = crate::sharding::slot_for_key(routing_key.into()).0;
        Ok(match self.mode() {
            FleetMode::Tenant => FleetDbKey::Tenant {
                id: TenantDbId::parse(routing_key)?,
                slot,
            },
            FleetMode::Slot => FleetDbKey::Slot(slot),
        })
    }

    /// The database key for a [`ShardKey`](crate::sharding::ShardKey).
    ///
    /// A slot fleet hashes the key exactly as configured shards do. A tenant
    /// fleet needs a textual id: an integer key is its decimal form and a
    /// byte key (a UUID) its lowercase hex.
    ///
    /// # Errors
    ///
    /// [`FleetError::InvalidKey`] when the id cannot name a file.
    pub fn key_for_shard_key(
        &self,
        key: crate::sharding::ShardKey<'_>,
    ) -> Result<FleetDbKey, FleetError> {
        use crate::sharding::ShardKey;
        let slot = crate::sharding::slot_for_key(key).0;
        Ok(match self.mode() {
            FleetMode::Slot => FleetDbKey::Slot(slot),
            FleetMode::Tenant => {
                let id = match key {
                    ShardKey::Str(id) => TenantDbId::parse(id)?,
                    ShardKey::Int(id) => TenantDbId::parse(&id.to_string())?,
                    ShardKey::Bytes(bytes) => {
                        use std::fmt::Write as _;
                        let mut hex = String::with_capacity(bytes.len() * 2);
                        for byte in bytes {
                            let _ = write!(hex, "{byte:02x}");
                        }
                        TenantDbId::parse(&hex)?
                    }
                };
                // The tenant's slot is the hash of its id string, which is
                // what `key_for` and the on-disk layout use.
                let slot = crate::sharding::slot_for_key(id.as_str().into()).0;
                FleetDbKey::Tenant { id, slot }
            }
        })
    }

    /// The key for a name produced by [`FleetDbKey::name`].
    ///
    /// # Errors
    ///
    /// [`FleetError::InvalidKey`] for a malformed name or one of the other mode.
    pub fn key_for_name(&self, name: &str) -> Result<FleetDbKey, FleetError> {
        let key = match crate::fleet_layout::parse_db_name(name)? {
            crate::fleet_layout::ParsedDbName::Tenant(id) => {
                let slot = crate::sharding::slot_for_key(id.as_str().into()).0;
                FleetDbKey::Tenant { id, slot }
            }
            crate::fleet_layout::ParsedDbName::Slot(slot) => FleetDbKey::Slot(slot),
        };
        if key.mode() != self.mode() {
            return Err(FleetLayoutError::InvalidName(name.to_owned()).into());
        }
        Ok(key)
    }

    /// Refuse a key this fleet cannot hold: another mode's, a slot out of
    /// range, or a tenant key whose slot is not its id's (all constructible
    /// by hand, since `FleetDbKey`'s variants are public).
    fn check_key(&self, key: &FleetDbKey) -> Result<(), FleetError> {
        let consistent = match key {
            FleetDbKey::Slot(slot) => *slot < crate::config::SLOT_COUNT,
            FleetDbKey::Tenant { id, slot } => {
                *slot == crate::sharding::slot_for_key(id.as_str().into()).0
            }
        };
        if key.mode() != self.mode() || !consistent {
            return Err(FleetLayoutError::InvalidName(key.name()).into());
        }
        Ok(())
    }

    /// The file of `key`, under the root.
    #[must_use]
    pub fn path_of(&self, key: &FleetDbKey) -> PathBuf {
        self.inner.root.join(self.inner.template.render(key))
    }

    fn url_of(path: &Path) -> String {
        format!("sqlite://{}", path.display())
    }

    // ── Open ─────────────────────────────────────────────────────────────

    /// Open the database for a routing key (see [`key_for`](Self::key_for)).
    ///
    /// # Errors
    ///
    /// See [`open`](Self::open).
    pub async fn open_for(&self, routing_key: &str) -> Result<FleetDatabase, FleetError> {
        let key = self.key_for(routing_key)?;
        self.open(&key).await
    }

    /// Open a database, creating it when the fleet creates on demand.
    ///
    /// # Errors
    ///
    /// [`FleetError::NotFound`], [`FleetError::Deleting`],
    /// [`FleetError::PendingMigrations`], [`FleetError::Misplaced`], or a
    /// migration / pool / I/O failure.
    pub async fn open(&self, key: &FleetDbKey) -> Result<FleetDatabase, FleetError> {
        self.open_with(key, self.inner.create_on_demand).await
    }

    /// Create a database and apply its migrations, whatever
    /// `create_on_demand` says. The way to admit a new tenant to a tenant
    /// fleet.
    ///
    /// Cancellation-safe: the publish runs in an owned task. If the caller is
    /// dropped after the file is published, the next `provision` of the key
    /// reports the creation. Exactly one call does, but it is not always the
    /// call that made the file. The record is in memory: a restart loses it.
    ///
    /// # Errors
    ///
    /// [`FleetError::AlreadyExists`] when the file is already there, or
    /// another call already reported the creation. [`FleetError::Unavailable`]
    /// when the create task fails. Or an [`open`](Self::open) error.
    pub async fn provision(&self, key: &FleetDbKey) -> Result<FleetDatabase, FleetError> {
        self.check_key(key)?;
        self.owned_epoch(key)?;
        // Held to the end, so `close_all` also sees the open that follows.
        let _open_counted = self.admit_mutation(key)?;
        let (fleet, task_key, counted) = (self.clone(), key.clone(), MutationGuard::new(self));
        tokio::spawn(async move {
            let _counted = counted;
            fleet.publish_owned(&task_key).await
        })
        .await
        .map_err(|e| FleetError::Unavailable {
            name: key.name(),
            detail: format!("create task failed: {e}"),
        })??;
        if !self.lock_state().unacknowledged.contains(key) {
            return Err(FleetError::AlreadyExists { name: key.name() });
        }
        let db = self.open_with(key, false).await?;
        // Of the callers that got this far, one removes the marker.
        if self.lock_state().unacknowledged.remove(key) {
            Ok(db)
        } else {
            Err(FleetError::AlreadyExists { name: key.name() })
        }
    }

    /// Publish the file for [`provision`](Self::provision), one task per key
    /// at a time. Records the creation until a caller claims it.
    async fn publish_owned(&self, key: &FleetDbKey) -> Result<(), FleetError> {
        self.with_provision_turn(key, self.publish(key)).await
    }

    /// Run `work` while no other provision or delete of `key` runs its file
    /// step. A delete then cannot clear the marker of a creation that
    /// follows its removal.
    async fn with_provision_turn<T>(&self, key: &FleetDbKey, work: impl Future<Output = T>) -> T {
        let lock = Arc::clone(
            self.lock_state()
                .provisioning
                .entry(key.clone())
                .or_default(),
        );
        let result = {
            let _turn = lock.lock().await;
            work.await
        };
        drop(lock);
        let mut state = self.lock_state();
        if state
            .provisioning
            .get(key)
            .is_some_and(|lock| Arc::strong_count(lock) == 1)
        {
            state.provisioning.remove(key);
        }
        drop(state);
        result
    }

    async fn publish(&self, key: &FleetDbKey) -> Result<(), FleetError> {
        let path = self.path_of(key);
        if path.exists() {
            return Ok(());
        }
        // The publish result decides: of two concurrent provisions (in this
        // process or another) only the one whose link created the file
        // records a creation, and its caller alone runs the signup work.
        let fleet = self.clone();
        let name = key.name();
        let created = crate::time::spawn_blocking(move || {
            // Before any file work: staging and publish would otherwise follow
            // a bucket symlink and leave a database outside the root.
            check_contained(fleet.root(), &path)?;
            create_database_file(fleet.root(), &path, &name, &fleet.inner.migrations)
        })
        .await
        .map_err(|e| FleetError::Unavailable {
            name: key.name(),
            detail: format!("create task failed: {e}"),
        })??;
        if created {
            let counters = &self.inner.counters;
            counters.created.fetch_add(1, Ordering::Relaxed);
            counters
                .migrations_applied
                .fetch_add(self.inner.migrations.count() as u64, Ordering::Relaxed);
            self.lock_state().unacknowledged.insert(key.clone());
        }
        Ok(())
    }

    /// Whether the database's file exists.
    #[must_use]
    pub fn exists(&self, key: &FleetDbKey) -> bool {
        self.path_of(key).is_file()
    }

    async fn open_with(&self, key: &FleetDbKey, create: bool) -> Result<FleetDatabase, FleetError> {
        // Only a background claim reports no room.
        self.open_entry(key, create, false)
            .await?
            .map(|opened| opened.db)
            .ok_or_else(|| FleetError::Unavailable {
                name: key.name(),
                detail: "no room".to_owned(),
            })
    }

    /// Open `key` for background work (ADR 0019 §6). It never creates a
    /// database, never restores a missing one from its replica, and never
    /// makes the fleet hold more than `max_open`: with no room it returns
    /// `Ok(None)`. It does not count as a use, so the
    /// database stays the first LRU victim, and
    /// [`close_if_unused`](Self::close_if_unused) closes it again when no
    /// request used it meanwhile.
    ///
    /// # Errors
    ///
    /// [`FleetError::NotFound`] when the file is missing (and no restore
    /// produced it), or another [`open`](Self::open) error.
    pub async fn open_background(
        &self,
        key: &FleetDbKey,
    ) -> Result<Option<BackgroundOpen>, FleetError> {
        self.open_entry(key, false, true).await
    }

    /// Close `key` if a background open opened it, nothing else used it since,
    /// and nobody holds it (no lease, no checked-out connection). Returns
    /// whether the close began.
    #[must_use = "whether the close began"]
    pub fn close_if_unused(&self, key: &FleetDbKey) -> bool {
        let begun = self.begin_drains(|state| {
            state
                .open
                .get(key)
                .filter(|entry| {
                    entry.last_use_seq.load(Ordering::Relaxed) == BACKGROUND_SEQ
                        && entry.cell.get().is_some_and(is_idle)
                })
                .map(|_| vec![key.clone()])
                .unwrap_or_default()
        });
        let closing = !begun.is_empty();
        for (key, db, drain) in begun {
            self.spawn_drain(key, db, drain, true);
        }
        closing
    }

    /// Open through the key's entry. A `background` open is refused for room
    /// (`Ok(None)`) and does not mark the entry used.
    async fn open_entry(
        &self,
        key: &FleetDbKey,
        create: bool,
        background: bool,
    ) -> Result<Option<BackgroundOpen>, FleetError> {
        self.check_key(key)?;
        loop {
            // Before anything touches the disk: `restore_missing` runs only
            // for a key this host owns (issue #3233).
            let epoch = self.owned_epoch(key)?;
            let next = self.claim_entry(key, background)?;
            let (entry, elected) = match next {
                Next::Wait(drain) => {
                    drain.wait().await;
                    continue;
                }
                Next::NoRoom => return Ok(None),
                Next::Open(entry, elected) => (entry, elected),
            };
            // No await between the claim and the spawn, so a cancelled caller
            // cannot leave an entry without an opener.
            let opener =
                elected.then(|| self.spawn_opener(key, create, !background, epoch, &entry));
            let result = match (entry.cell.get(), opener) {
                (Some(db), _) => Ok(db.clone()),
                (None, Some(opener)) => opener.await.unwrap_or_else(|error| {
                    Err(FleetError::Unavailable {
                        name: key.name(),
                        detail: format!("open task failed: {error}"),
                    })
                }),
                (None, None) => {
                    entry.wait().await;
                    match entry.cell.get() {
                        Some(db) => Ok(db.clone()),
                        // The opener failed and its entry is gone: open afresh.
                        None => continue,
                    }
                }
            };
            match result {
                Ok(db) => {
                    // The pool is closed when the database was drained between
                    // our lookup and now; go round and open it fresh.
                    if db.pool.is_closed() {
                        continue;
                    }
                    // Read the epoch again: the open can take long (a
                    // restore), and the range can be lost meanwhile. Never
                    // serve a database under an epoch this host does not hold
                    // now. The fence closes it; it is not awaited here, as
                    // the caller may hold one of its connections.
                    match self.owned_epoch(key) {
                        Ok(now) if now == db.epoch => {}
                        Ok(_) => {
                            let _ = self.fence_unowned();
                            return Err(FleetError::Unavailable {
                                name: key.name(),
                                detail: "the database reopens under a new ownership epoch; retry"
                                    .to_owned(),
                            });
                        }
                        Err(error) => {
                            let _ = self.fence_unowned();
                            return Err(error);
                        }
                    }
                    if !background {
                        // The claim stamped the entry before the file was
                        // created and migrated. A slow open must not count as
                        // idle time.
                        entry.mark_used_now();
                        self.enforce_capacity(Some(key));
                    }
                    return Ok(Some(BackgroundOpen {
                        db,
                        opened: elected,
                    }));
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Start the entry's one opener as an owned task. A cancelled caller only
    /// stops waiting: the open (file, migrations, `on_open` hooks) still ends
    /// in the entry, where a later open, close or delete finds it.
    fn spawn_opener(
        &self,
        key: &FleetDbKey,
        create: bool,
        restore: bool,
        epoch: Option<crate::lock::FencingToken>,
        entry: &Arc<Entry>,
    ) -> tokio::task::JoinHandle<Result<FleetDatabase, FleetError>> {
        let guard = OpenGuard {
            fleet: self.clone(),
            key: key.clone(),
            entry: Arc::clone(entry),
        };
        tokio::spawn(async move {
            match guard
                .fleet
                .open_now(guard.key.clone(), create, restore, epoch)
                .await
            {
                Ok(db) => {
                    let _ = guard.entry.cell.set(db.clone());
                    // The caller may be gone; the cap is the opener's to keep.
                    guard.fleet.enforce_capacity(Some(&guard.key));
                    Ok(db)
                }
                Err(error) => {
                    let counters = &guard.fleet.inner.counters;
                    counters.open_failures.fetch_add(1, Ordering::Relaxed);
                    Err(error)
                }
            }
        })
    }

    /// Under the lock: wait for a drain of `key`, or take (or make) its entry
    /// and mark it used. A `background` claim marks nothing, and makes no
    /// entry when the fleet already holds `max_open`.
    fn claim_entry(&self, key: &FleetDbKey, background: bool) -> Result<Next, FleetError> {
        let mut state = self.lock_state();
        if state.shutting_down {
            return Err(FleetError::Unavailable {
                name: key.name(),
                detail: "the fleet is shutting down".to_owned(),
            });
        }
        if let Some(drain) = state.draining.get(key).cloned() {
            drop(state);
            if drain.deleting {
                return Err(FleetError::Deleting { name: key.name() });
            }
            return Ok(Next::Wait(drain));
        }
        if background {
            if let Some(entry) = state.open.get(key) {
                return Ok(Next::Open(Arc::clone(entry), false));
            }
            if state.open.len() >= self.inner.max_open {
                return Ok(Next::NoRoom);
            }
            let entry = Arc::new(Entry::new(BACKGROUND_SEQ));
            state.open.insert(key.clone(), Arc::clone(&entry));
            return Ok(Next::Open(entry, true));
        }
        state.seq += 1;
        let seq = state.seq;
        let mut elected = false;
        let entry = Arc::clone(state.open.entry(key.clone()).or_insert_with(|| {
            elected = true;
            Arc::new(Entry::new(seq))
        }));
        // Under the lock: `close_if_unused` reads the mark under it too.
        entry.touch(seq);
        drop(state);
        Ok(Next::Open(entry, elected))
    }

    /// Remove `entry` from the open map, if it is still the one there.
    fn forget_entry(&self, key: &FleetDbKey, entry: &Arc<Entry>) {
        let mut state = self.lock_state();
        if state
            .open
            .get(key)
            .is_some_and(|current| Arc::ptr_eq(current, entry))
        {
            state.open.remove(key);
        }
    }

    /// `restore`: a missing file may come back from its replica. A background
    /// open passes `false`, so it never brings back a deleted database.
    async fn open_now(
        &self,
        key: FleetDbKey,
        create: bool,
        restore: bool,
        epoch: Option<crate::lock::FencingToken>,
    ) -> Result<FleetDatabase, FleetError> {
        let path = self.path_of(&key);
        let name = key.name();
        check_contained(self.root(), &path)?;
        let mut existed = path.is_file();
        if !existed && restore {
            existed = self.restore_missing(&key, &path).await?;
        }
        // Whether this open published the file (another process may have won
        // the race to create it; then we open theirs).
        let created = if existed {
            false
        } else {
            if !create {
                return Err(FleetError::NotFound { name });
            }
            let fleet = self.clone();
            let (create_path, create_name) = (path.clone(), name.clone());
            crate::time::spawn_blocking(move || {
                create_database_file(
                    fleet.root(),
                    &create_path,
                    &create_name,
                    &fleet.inner.migrations,
                )
            })
            .await
            .map_err(|e| FleetError::Unavailable {
                name: name.clone(),
                detail: format!("create task failed: {e}"),
            })??
        };
        let url = Self::url_of(&path);
        let pool = crate::db::build_sqlite_pool_with(
            &url,
            self.inner.pool_size,
            self.inner.connect_timeout_secs,
            crate::db::SqliteCheckpointOwner::Fleet(self.replicating()),
        )
        .map_err(|e| FleetError::Unavailable {
            name: name.clone(),
            detail: e.to_string(),
        })?;
        let db = FleetDatabase {
            key,
            path: Arc::from(path.as_path()),
            pool,
            closed: CancellationToken::new(),
            lease: Arc::new(()),
            epoch,
            workers: Arc::new(Mutex::new(Some(Vec::new()))),
        };
        // The file is complete here — published fully migrated, restored, or
        // already there — so a failure below closes the pool and never
        // removes the file another opener (or process) may be serving.
        let applied = match self.prepare(&db).await {
            Ok(applied) => applied,
            Err(error) => {
                db.quiesce_workers(true).await;
                db.closed.cancel();
                let closer = db.pool.clone();
                let _ = crate::time::spawn_blocking(move || closer.close()).await;
                return Err(error);
            }
        };
        let applied = applied + usize::from(created) * self.inner.migrations.count();
        let existed = !created;
        let counters = &self.inner.counters;
        counters.opens.fetch_add(1, Ordering::Relaxed);
        counters
            .migrations_applied
            .fetch_add(applied as u64, Ordering::Relaxed);
        if !existed {
            counters.created.fetch_add(1, Ordering::Relaxed);
        }
        tracing::debug!(database = %name, created = !existed, migrations = applied, "fleet database opened");
        Ok(db)
    }

    /// Identity check, migrations and `on_open` hooks for a freshly built
    /// pool. Returns the number of migrations applied.
    async fn prepare(&self, db: &FleetDatabase) -> Result<usize, FleetError> {
        let name = db.key.name();
        // The first pooled connection sets WAL and checks (or, for a file
        // that predates identity rows, records) which database it is.
        check_identity(&db.pool, &name).await?;

        let fleet = self.clone();
        let migrate_name = name.clone();
        let migrate_url = Self::url_of(&db.path);
        let applied = crate::time::spawn_blocking(move || {
            let migrations = &fleet.inner.migrations;
            if migrations.apply_on_open {
                migrations.apply(&migrate_url, &migrate_name)
            } else {
                match migrations.pending(&migrate_url, &migrate_name)? {
                    0 => Ok(0),
                    count => Err(FleetError::PendingMigrations {
                        name: migrate_name,
                        count,
                    }),
                }
            }
        })
        .await
        .map_err(|e| FleetError::Unavailable {
            name: name.clone(),
            detail: format!("migration task failed: {e}"),
        })??;

        let hooks = self.hooks();
        if !hooks.is_empty() {
            let opened = db.clone();
            crate::time::spawn_blocking(move || {
                let mut started: Vec<&Arc<dyn FleetLifecycle>> = Vec::new();
                for hook in &hooks {
                    if let Err(detail) = hook.on_open(&opened) {
                        // Undo the hooks that did start (a replicator that
                        // registered) before refusing the open.
                        // Stop the workers first, as `drain` does.
                        tokio::runtime::Handle::current().block_on(opened.quiesce_workers(true));
                        for hook in started {
                            hook.on_close(&opened);
                        }
                        return Err(detail);
                    }
                    started.push(hook);
                }
                Ok(())
            })
            .await
            .map_err(|e| e.to_string())
            .and_then(|r| r)
            .map_err(|detail| FleetError::Unavailable { name, detail })?;
        }
        Ok(applied)
    }

    /// Ask the lifecycle hooks to produce a missing file (a restore).
    async fn restore_missing(&self, key: &FleetDbKey, path: &Path) -> Result<bool, FleetError> {
        let hooks = self.hooks();
        if hooks.is_empty() {
            return Ok(false);
        }
        let (hook_key, hook_path) = (key.clone(), path.to_path_buf());
        let root = self.inner.root.clone();
        let name = key.name();
        crate::time::spawn_blocking(move || {
            // The hook publishes the restored file. Hold the path lock so no
            // other process publishes meanwhile, and clear the stale sidecars
            // of the absent database first: they would replay onto it.
            with_path_lock(&root, &name, || {
                if !hook_path.exists() {
                    unlink_database_set(
                        &hook_path,
                        "clear stray sidecars",
                        &name,
                        false,
                        |file| std::fs::remove_file(file),
                    )?;
                }
                for hook in hooks {
                    let restored =
                        hook.restore_missing(&hook_key, &hook_path)
                            .map_err(|detail| FleetError::Restore {
                                name: name.clone(),
                                detail,
                            })?;
                    if restored {
                        return Ok(true);
                    }
                }
                Ok(false)
            })
        })
        .await
        .map_err(|error| FleetError::Restore {
            name: key.name(),
            detail: error.to_string(),
        })?
    }

    // ── Close ────────────────────────────────────────────────────────────

    fn lock_state(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Close least recently used idle databases until at most `max_open`
    /// are open. A database with a checked-out connection is never chosen,
    /// so the cap is soft while every open database is busy.
    fn enforce_capacity(&self, keep: Option<&FleetDbKey>) {
        let max_open = self.inner.max_open;
        let min_residency = self.inner.min_residency;
        let now = crate::time::ambient_instant();
        let victims = self.begin_drains(|state| {
            let excess = state.open.len().saturating_sub(max_open);
            if excess == 0 {
                return Vec::new();
            }
            let mut idle: Vec<(u64, FleetDbKey)> = state
                .open
                .iter()
                .filter(|(key, _)| Some(*key) != keep)
                // The residency grace covers a request between its resolve
                // and its checkout. A background entry has no request.
                .filter(|(_, entry)| {
                    entry.last_use_seq.load(Ordering::Relaxed) == BACKGROUND_SEQ
                        || now.saturating_duration_since(entry.last_used()) >= min_residency
                })
                .filter_map(|(key, entry)| {
                    let db = entry.cell.get()?;
                    is_idle(db).then(|| (entry.last_use_seq.load(Ordering::Relaxed), key.clone()))
                })
                .collect();
            idle.sort_unstable();
            idle.truncate(excess);
            idle.into_iter().map(|(_, key)| key).collect()
        });
        for (key, db, drain) in victims {
            self.spawn_drain(key, db, drain, true);
        }
    }

    /// Close every database idle for longer than `idle_close`, returning how
    /// many began closing.
    ///
    /// Called by the sweeper [`spawn_maintenance`](Self::spawn_maintenance)
    /// starts; public for tests and tools that drive time by hand.
    #[must_use = "the count is how many databases began closing"]
    pub fn close_idle(&self) -> usize {
        let Some(idle_close) = self.inner.idle_close else {
            return 0;
        };
        let now = crate::time::ambient_instant();
        let victims = self.begin_drains(|state| {
            state
                .open
                .iter()
                .filter(|(_, entry)| {
                    now.saturating_duration_since(entry.last_used()) >= idle_close
                        && entry.cell.get().is_some_and(is_idle)
                })
                .map(|(key, _)| key.clone())
                .collect()
        });
        let closed = victims.len();
        for (key, db, drain) in victims {
            self.spawn_drain(key, db, drain, true);
        }
        closed
    }

    /// Under one lock: pick keys with `pick`, then move each from open to
    /// draining.
    fn begin_drains(&self, pick: impl FnOnce(&State) -> Vec<FleetDbKey>) -> Vec<Draining> {
        let mut state = self.lock_state();
        let keys = pick(&state);
        let draining = keys
            .iter()
            .filter_map(|key| self.begin_drain_locked(&mut state, key, false))
            .collect();
        drop(state);
        draining
    }

    /// Move `key` from open to draining. `None` when it is not open (or is
    /// still opening, which is never interrupted).
    fn begin_drain_locked(
        &self,
        state: &mut State,
        key: &FleetDbKey,
        deleting: bool,
    ) -> Option<Draining> {
        let db = state.open.get(key)?.cell.get()?.clone();
        state.open.remove(key);
        let drain = Arc::new(Drain {
            deleting,
            finished: AtomicBool::new(false),
            done: Notify::new(),
        });
        state.draining.insert(key.clone(), Arc::clone(&drain));
        self.inner.in_flight_drains.fetch_add(1, Ordering::Relaxed);
        Some((key.clone(), Some(db), drain))
    }

    fn spawn_drain(
        &self,
        key: FleetDbKey,
        db: Option<FleetDatabase>,
        drain: Arc<Drain>,
        settle: bool,
    ) {
        let fleet = self.clone();
        tokio::spawn(async move {
            let _finish = DrainGuard {
                fleet: &fleet,
                key: key.clone(),
                drain,
            };
            fleet.drain(&key, db, settle).await;
        });
    }

    /// Close one database: refuse new checkouts, wait for the in-flight ones,
    /// then run the `on_close` hooks.
    ///
    /// `settle` lets the database's workers finish their step first, so a
    /// hook is acked. A delete or restore passes `false`: the data goes away,
    /// and the pool must stop stale holders at once.
    async fn drain(&self, key: &FleetDbKey, db: Option<FleetDatabase>, settle: bool) {
        let Some(db) = db else {
            return;
        };
        // Workers settle first: their last ack needs a live pool.
        db.quiesce_workers(settle).await;
        db.closed.cancel();
        // `close` drops the idle connections, and dropping a `SQLite`
        // connection can checkpoint: keep that I/O off the async workers.
        let closer = db.pool.clone();
        let _ = crate::time::spawn_blocking(move || closer.close()).await;
        while db.pool.status().size > 0 {
            tokio::time::sleep(DRAIN_POLL).await;
        }
        let hooks = self.hooks();
        if !hooks.is_empty() {
            let _ = crate::time::spawn_blocking(move || {
                for hook in hooks {
                    hook.on_close(&db);
                }
            })
            .await;
        }
        self.inner.counters.closes.fetch_add(1, Ordering::Relaxed);
        tracing::debug!(database = %key, "fleet database closed");
    }

    fn finish_drain(&self, key: &FleetDbKey, drain: &Arc<Drain>) {
        {
            let mut state = self.lock_state();
            if state
                .draining
                .get(key)
                .is_some_and(|current| Arc::ptr_eq(current, drain))
            {
                state.draining.remove(key);
            }
        }
        self.inner.in_flight_drains.fetch_sub(1, Ordering::Relaxed);
        drain.finished.store(true, Ordering::Release);
        drain.done.notify_waiters();
    }

    /// Close one database now (waiting for its in-flight connections). A
    /// no-op when it is not open.
    ///
    /// Cancellation-safe: the work runs in an owned task, so a dropped caller
    /// only stops waiting.
    pub async fn close(&self, key: &FleetDbKey) {
        let (fleet, key) = (self.clone(), key.clone());
        // A task panic has nothing to report to: `close` returns no result.
        let _ = tokio::spawn(async move { fleet.close_owned(&key).await }).await;
    }

    async fn close_owned(&self, key: &FleetDbKey) {
        // A cold open is never interrupted: let it end, then close it.
        self.settle_opening(key).await;
        let begun = self.begin_drains(|_| vec![key.clone()]).pop();
        if let Some((key, db, drain)) = begun {
            let _finish = DrainGuard {
                fleet: self,
                key: key.clone(),
                drain,
            };
            self.drain(&key, db, true).await;
        } else {
            let pending = self.lock_state().draining.get(key).cloned();
            if let Some(drain) = pending {
                drain.wait().await;
            }
        }
    }

    /// Set `shutting_down` if no counted work runs. Only then: a provision
    /// counted before this must finish its own open.
    fn stop_admitting_if_idle(&self) -> bool {
        let mut state = self.lock_state();
        let idle = self.inner.mutations.load(Ordering::Acquire) == 0;
        if idle {
            state.shutting_down = true;
        }
        drop(state);
        idle
    }

    /// Count a new `provision`, `delete` or `restore` unless shutdown has
    /// begun. The check and the
    /// count share one lock hold, and `close_all` sets the flag under the same
    /// lock. So a call is either counted before `close_all` looks, or refused.
    fn admit_mutation(&self, key: &FleetDbKey) -> Result<MutationGuard, FleetError> {
        let state = self.lock_state();
        if state.shutting_down {
            return Err(FleetError::Unavailable {
                name: key.name(),
                detail: "the fleet is shutting down".to_owned(),
            });
        }
        let counted = MutationGuard::new(self);
        drop(state);
        Ok(counted)
    }

    /// Close every open database and wait for all drains, for shutdown.
    pub async fn close_all(&self) {
        // Mutations first: one that just started may open or drain a key.
        loop {
            let ended = self.inner.mutations_done.notified();
            // Under the lock `admit_mutation` takes: no call slips in between
            // the count and the flag.
            if self.stop_admitting_if_idle() {
                break;
            }
            ended.await;
        }
        let keys: Vec<FleetDbKey> = self.lock_state().open.keys().cloned().collect();
        // Together, so the worker timeout is paid once, not per database.
        futures::future::join_all(keys.iter().map(|key| self.close(key))).await;
        loop {
            let pending: Vec<Arc<Drain>> = self.lock_state().draining.values().cloned().collect();
            if pending.is_empty() {
                return;
            }
            for drain in pending {
                drain.wait().await;
            }
        }
    }

    /// How often the sweeper runs: often enough to close idle databases on
    /// time, and to retry the `max_open` cap once the residency grace that
    /// blocked it has passed (a burst of opens inside one grace window would
    /// otherwise stay over the cap until the next open).
    fn sweep_interval(&self) -> Duration {
        let idle = self
            .inner
            .idle_close
            .map_or(MAX_SWEEP_INTERVAL, |idle| idle / 4);
        idle.min(self.inner.min_residency.max(Duration::from_millis(100)))
            .clamp(Duration::from_millis(100), MAX_SWEEP_INTERVAL)
    }

    /// Start the sweeper: it closes idle databases and enforces `max_open`.
    /// It stops when `shutdown` is cancelled, after closing every database so
    /// replicators ship their last frames.
    #[must_use = "await the handle at shutdown so every database closes (and ships its last frames)"]
    pub fn spawn_maintenance(&self, shutdown: CancellationToken) -> tokio::task::JoinHandle<()> {
        let fleet = self.clone();
        let interval = self.sweep_interval();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = shutdown.cancelled() => break,
                    () = tokio::time::sleep(interval) => {
                        let _ = fleet.close_idle();
                        fleet.enforce_capacity(None);
                    }
                }
            }
            // Real time on purpose: the flush budget covers blocking I/O.
            #[allow(
                clippy::disallowed_methods,
                reason = "real time: the shutdown bound covers blocking I/O"
            )]
            let shutdown_began = std::time::Instant::now();
            if let Some(replication) = fleet.replication() {
                // Closing ships each database's last frames. Bound that too.
                // Keep a cutoff the caller set before it woke this task.
                if replication.cutoff_left().is_none() {
                    replication
                        .set_shutdown_cutoff(shutdown_began.checked_add(replication.final_flush()));
                }
            }
            fleet.close_all().await;
            // Every database is closed. Keep shipping parked replicators until
            // they catch up or the flush deadline passes, then stop the loop
            // thread so it does not outlive the app (an embedded or restarted
            // server). Closing the databases used part of the deadline.
            if let Some(replication) = fleet.replication().cloned() {
                let _ = crate::time::spawn_blocking(move || {
                    let left = replication.cutoff_left().unwrap_or_else(|| {
                        replication
                            .final_flush()
                            .saturating_sub(shutdown_began.elapsed())
                    });
                    replication.flush_closing(left);
                    replication.stop();
                })
                .await;
            }
        })
    }

    // ── Delete, backup ──────────────────────────────────────────────────

    /// Delete a database and its sidecar files. Requests for it get `410`
    /// while this runs, and `404` (or a fresh database, when created on
    /// demand) after.
    ///
    /// The call holds the path lock, which every process that shares the
    /// root takes to publish or remove a database at that path. So no other
    /// process publishes a database there while this call runs. One that does
    /// so afterwards keeps its files: this call touches nothing after the
    /// unlink.
    ///
    /// The database's replica, if any, is not deleted here: a lifecycle hook
    /// (`on_delete`) decides.
    ///
    /// Cancellation-safe: the work runs in an owned task, so a dropped caller
    /// only stops waiting.
    ///
    /// # Errors
    ///
    /// [`FleetError::Deleting`] when a delete is already running,
    /// [`FleetError::Unavailable`] once a shutdown has begun, or
    /// [`FleetError::Io`].
    pub async fn delete(&self, key: &FleetDbKey) -> Result<bool, FleetError> {
        self.check_key(key)?;
        let counted = self.admit_mutation(key)?;
        let (fleet, key) = (self.clone(), key.clone());
        tokio::spawn(async move {
            let _counted = counted;
            fleet.delete_owned(&key).await
        })
        .await
        .map_err(|error| FleetError::Io {
            op: "delete database",
            detail: error.to_string(),
        })?
    }

    async fn delete_owned(&self, key: &FleetDbKey) -> Result<bool, FleetError> {
        let begun = self.begin_exclusive(key, true)?;
        let Some((key, entry, drain)) = begun else {
            // A plain close is running; wait, then delete.
            let pending = self.lock_state().draining.get(key).cloned();
            if let Some(pending) = pending {
                pending.wait().await;
            }
            return Box::pin(self.delete_owned(key)).await;
        };
        let _finish = DrainGuard {
            fleet: self,
            key: key.clone(),
            drain,
        };
        let db = self.take_database(entry).await;
        self.drain(&key, db, false).await;
        // Hooks let go of the files first (a parked replicator holds one open).
        for hook in self.hooks() {
            hook.release(&key);
        }
        let path = self.path_of(&key);
        let root = self.inner.root.clone();
        let name = key.name();
        let removed = self
            .with_provision_turn(&key, async {
                let removed = crate::time::spawn_blocking(move || {
                    // Another process sharing the root may still serve the file;
                    // unlinking it under that process would split the tenant's data
                    // between the old inode and a new file. The guard refuses then.
                    // On Unix it also blocks new openers while the files go. On
                    // Windows it closes first.
                    check_contained(&root, &path)?;
                    with_path_lock(&root, &name, || {
                        with_exclusive_access(&path, &name, HOLD_GUARD_WHILE_REMOVING, |present| {
                            remove_database_files(&path, &root, &name, present)
                        })
                    })
                })
                .await
                .map_err(|e| FleetError::Io {
                    op: "delete database",
                    detail: e.to_string(),
                })
                .and_then(|r| r);
                if removed.is_ok() {
                    self.lock_state().unacknowledged.remove(&key);
                }
                removed
            })
            .await;
        if matches!(removed, Ok(true)) {
            self.inner.counters.deleted.fetch_add(1, Ordering::Relaxed);
            // Only now: a refused delete (another process has the file open)
            // or a missing file must not trigger, say, replica cleanup.
            for hook in self.hooks() {
                hook.on_delete(&key);
            }
        }
        removed
    }

    /// Wait until no open of `key` is in progress. An open is never
    /// interrupted: this waits for the entry's opener task to end.
    async fn settle_opening(&self, key: &FleetDbKey) {
        loop {
            let opening = self
                .lock_state()
                .open
                .get(key)
                .filter(|entry| entry.cell.get().is_none())
                .cloned();
            let Some(entry) = opening else {
                break;
            };
            entry.wait().await;
        }
    }

    /// Under the lock: take `key` out of service for a delete (openers get
    /// `410`) or a restore (openers wait). The drain goes in first, so no new
    /// entry can appear. The caller settles the returned entry with
    /// [`take_database`](Self::take_database). `Ok(None)` when a plain close
    /// is running: the caller waits for it, then retries.
    fn begin_exclusive(
        &self,
        key: &FleetDbKey,
        deleting: bool,
    ) -> Result<Option<Exclusive>, FleetError> {
        let mut state = self.lock_state();
        if let Some(drain) = state.draining.get(key) {
            return if drain.deleting {
                Err(FleetError::Deleting { name: key.name() })
            } else {
                Ok(None)
            };
        }
        let entry = state.open.remove(key);
        let drain = Arc::new(Drain {
            deleting,
            finished: AtomicBool::new(false),
            done: Notify::new(),
        });
        state.draining.insert(key.clone(), Arc::clone(&drain));
        drop(state);
        self.inner.in_flight_drains.fetch_add(1, Ordering::Relaxed);
        Ok(Some((key.clone(), entry, drain)))
    }

    /// The database behind an entry that `begin_exclusive` took out of the
    /// open map. An open that is still running ends first: it is never
    /// interrupted, and a late opener cannot start one, as the drain is in.
    async fn take_database(&self, entry: Option<Arc<Entry>>) -> Option<FleetDatabase> {
        let entry = entry?;
        entry.wait().await;
        entry.cell.get().cloned()
    }

    /// Write a consistent copy of a database to `dest` with `VACUUM INTO`.
    /// `dest` must not exist. Readers and writers keep running meanwhile.
    ///
    /// # Errors
    ///
    /// An [`open`](Self::open) error, [`FleetError::AlreadyExists`] for an
    /// existing `dest`, or [`FleetError::Io`] when the copy fails.
    pub async fn backup(&self, key: &FleetDbKey, dest: &Path) -> Result<(), FleetError> {
        use diesel_async::RunQueryDsl as _;
        if dest.exists() {
            return Err(FleetError::AlreadyExists {
                name: dest.display().to_string(),
            });
        }
        let dest = dest.to_str().ok_or_else(|| FleetError::Io {
            op: "backup",
            detail: "destination path is not UTF-8".to_owned(),
        })?;
        let db = self.open_with(key, false).await?;
        let mut conn = db.pool.get().await.map_err(|e| FleetError::Unavailable {
            name: key.name(),
            detail: e.to_string(),
        })?;
        diesel::sql_query("VACUUM INTO ?")
            .bind::<diesel::sql_types::Text, _>(dest)
            .execute(&mut *conn)
            .await
            .map_err(|e| FleetError::Io {
                op: "backup",
                detail: e.to_string(),
            })?;
        Ok(())
    }

    /// Rebuild a database from its replica, as of `target` (latest when
    /// `None`), over whatever is on disk. Openers wait until it is done, then
    /// open the restored file. Needs [`install_replication`](Self::install_replication).
    ///
    /// Cancellation-safe: the work runs in an owned task, so a dropped caller
    /// only stops waiting.
    ///
    /// # Errors
    ///
    /// [`FleetError::NotReplicated`] without replication,
    /// [`FleetError::NotFound`] when nothing was ever shipped for the key,
    /// [`FleetError::Deleting`] during a delete,
    /// [`FleetError::Unavailable`] once a shutdown has begun, or
    /// [`FleetError::Restore`].
    pub async fn restore(
        &self,
        key: &FleetDbKey,
        target: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<crate::replication::RestoreOutcome, FleetError> {
        self.check_key(key)?;
        let (fleet, key, name) = (self.clone(), key.clone(), key.name());
        let counted = self.admit_mutation(&key)?;
        tokio::spawn(async move {
            let _counted = counted;
            fleet.restore_owned(&key, target).await
        })
        .await
        .map_err(|error| FleetError::Restore {
            name,
            detail: error.to_string(),
        })?
    }

    async fn restore_owned(
        &self,
        key: &FleetDbKey,
        target: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<crate::replication::RestoreOutcome, FleetError> {
        let replication = self
            .inner
            .replication
            .get()
            .cloned()
            .ok_or_else(|| FleetError::NotReplicated { name: key.name() })?;
        loop {
            if let Some((key, entry, drain)) = self.begin_exclusive(key, false)? {
                let _finish = DrainGuard {
                    fleet: self,
                    key: key.clone(),
                    drain,
                };
                let db = self.take_database(entry).await;
                self.drain(&key, db, false).await;
                for hook in self.hooks() {
                    hook.release(&key);
                }
                let path = self.path_of(&key);
                let root = self.inner.root.clone();
                let restore_key = key.clone();
                // The guard finishes the drain whatever fails: an error that
                // skipped it would strand every later opener of this key.
                let restored = crate::time::spawn_blocking(move || {
                    check_contained(&root, &path)?;
                    restore_in_place(&root, &replication, &restore_key, target, &path)
                })
                .await
                .map_err(|error| FleetError::Restore {
                    name: key.name(),
                    detail: error.to_string(),
                })
                .and_then(|r| r);
                return restored;
            }
            let pending = self.lock_state().draining.get(key).cloned();
            if let Some(pending) = pending {
                pending.wait().await;
            }
        }
    }

    // ── Enumeration and fan-out ─────────────────────────────────────────

    /// Every database on disk, sorted by key. Files that do not match the
    /// template (sidecars, strays, a tenant file in the wrong bucket) and
    /// symbolic links are skipped.
    ///
    /// # Errors
    ///
    /// [`FleetError::Io`] when the root cannot be read.
    pub async fn list(&self) -> Result<Vec<FleetDbKey>, FleetError> {
        let fleet = self.clone();
        crate::time::spawn_blocking(move || fleet.list_blocking())
            .await
            .map_err(|e| FleetError::Io {
                op: "list",
                detail: e.to_string(),
            })?
    }

    fn list_blocking(&self) -> Result<Vec<FleetDbKey>, FleetError> {
        let template = &self.inner.template;
        let slot_of = |id: &str| crate::sharding::slot_for_key(id.into()).0;
        let mut found = Vec::new();
        let mut frontier = vec![(PathBuf::new(), 0_usize)];
        while let Some((relative, depth)) = frontier.pop() {
            let last = depth + 1 == template.depth();
            if let Some(literal) = template.literal_segment(depth) {
                let next = relative.join(literal);
                let meta = std::fs::symlink_metadata(self.inner.root.join(&next));
                match meta {
                    Ok(meta) if last && meta.is_file() => {
                        if let Some(key) = template.key_for_path(&next, &slot_of) {
                            found.push(key);
                        }
                    }
                    Ok(meta) if !last && meta.is_dir() => frontier.push((next, depth + 1)),
                    _ => {}
                }
                continue;
            }
            let dir = self.inner.root.join(&relative);
            let entries = match std::fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && depth > 0 => continue,
                Err(e) => return Err(io_error("list")(e)),
            };
            for entry in entries {
                let entry = entry.map_err(io_error("list"))?;
                let Ok(name) = entry.file_name().into_string() else {
                    continue;
                };
                if !template.segment_matches(depth, &name) {
                    continue;
                }
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                let next = relative.join(&name);
                if last && file_type.is_file() {
                    if let Some(key) = template.key_for_path(&next, &slot_of) {
                        found.push(key);
                    }
                } else if !last && file_type.is_dir() {
                    frontier.push((next, depth + 1));
                }
            }
        }
        found.sort();
        Ok(found)
    }

    /// Run `f` on every database on disk, at most `concurrency` at once,
    /// opening each (never creating). Results come back in key order; a
    /// failure on one database does not stop the others.
    ///
    /// This opens every database, which churns the open set past `max_open`;
    /// use it for admin work, not on a hot path. With ownership, it skips the
    /// files of slots this host does not own.
    ///
    /// # Errors
    ///
    /// [`FleetError::Io`] when the fleet cannot be listed.
    pub async fn each<T, Fut, F>(
        &self,
        concurrency: usize,
        f: F,
    ) -> Result<Vec<(FleetDbKey, Result<T, AutumnError>)>, FleetError>
    where
        T: Send,
        Fut: std::future::Future<Output = Result<T, AutumnError>> + Send,
        F: Fn(FleetDatabase) -> Fut + Send + Sync,
    {
        use futures::StreamExt as _;
        // With ownership, only the slots this host serves (issue #3233).
        let keys = self.list().await?.into_iter().filter(|key| self.owns(key));
        let f = &f;
        let results = futures::stream::iter(keys.map(|key| async move {
            let result = match self.open_with(&key, false).await {
                Ok(db) => f(db).await,
                Err(error) => Err(error.into()),
            };
            (key, result)
        }))
        .buffered(concurrency.max(1))
        .collect::<Vec<_>>()
        .await;
        Ok(results)
    }

    /// Apply pending migrations to every database on disk, at most
    /// `concurrency` at once, without opening pools. For `AUTUMN_MIGRATE=1`
    /// and deploy hooks. A failing database is reported, not fatal.
    ///
    /// # Errors
    ///
    /// [`FleetError::Io`] when the fleet cannot be listed.
    pub async fn migrate_all(
        &self,
        concurrency: usize,
    ) -> Result<FleetMigrationReport, FleetError> {
        let keys = self.list().await?;
        Ok(self.migrate_keys(keys, concurrency).await)
    }

    /// [`migrate_all`](Self::migrate_all) on exactly `keys`, for a caller
    /// that checked each database first: a database created since then is
    /// not visited. A key whose file is gone is skipped, never created.
    pub async fn migrate_keys(
        &self,
        keys: Vec<FleetDbKey>,
        concurrency: usize,
    ) -> FleetMigrationReport {
        use futures::StreamExt as _;
        let mut report = FleetMigrationReport {
            databases: keys.len(),
            ..FleetMigrationReport::default()
        };
        let outcomes = futures::stream::iter(keys.into_iter().map(|key| async move {
            let outcome = match self.check_key(&key) {
                // Never opened: on Windows the open would create the file.
                Ok(()) if !self.exists(&key) => Ok(0),
                Ok(()) => self.migrate_existing(&key).await,
                Err(error) => Err(error),
            };
            (key, outcome)
        }))
        .buffer_unordered(concurrency.max(1))
        .collect::<Vec<_>>()
        .await;
        for (key, outcome) in outcomes {
            match outcome {
                Ok(applied) => report.applied += applied,
                Err(error) => report.failed.push((key, error)),
            }
        }
        report.failed.sort_by(|a, b| a.0.cmp(&b.0));
        self.inner
            .counters
            .migrations_applied
            .fetch_add(report.applied as u64, Ordering::Relaxed);
        report
    }

    /// Migrations this fleet records under a substitute version, because two
    /// of its sets share a version. Full names, sorted. Empty in the usual
    /// case.
    ///
    /// The substitute depends on every set of the list. A tool that has only
    /// some of the app's sets (`autumn fleet migrate` reads `migrations/`
    /// only) must not migrate when this is not empty: boot can record other
    /// versions.
    #[must_use]
    pub fn remapped_migrations(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .inner
            .migrations
            .disambiguated
            .keys()
            .cloned()
            .collect();
        names.sort();
        names
    }

    /// Refuse a control database (one of `control_urls`) inside the root: a
    /// tenant or slot path could open it. Boot and `autumn fleet` call this.
    ///
    /// Config validation compares paths lexically, so a symlinked root can
    /// still reach the control database. `root()` is canonical, so this
    /// compares the control file's canonical path.
    ///
    /// # Errors
    ///
    /// The control database that resolves inside the root.
    pub fn check_control_outside_root(&self, control_urls: &[&str]) -> Result<(), String> {
        let inside = control_urls
            .iter()
            .filter_map(|url| crate::config::sqlite_url_file(url))
            .filter_map(|path| canonical_target(&path))
            .find(|control| control.starts_with(self.root()));
        inside.map_or(Ok(()), |control| {
            Err(format!(
                "the control database {} resolves inside database.fleet.root {}; a tenant or \
                 slot path could open it. Put the control database outside the fleet root",
                control.display(),
                self.root().display()
            ))
        })
    }

    /// The versions this fleet's sets record, substitutes included. Sorted.
    ///
    /// A database that records another version was migrated by a set this
    /// fleet does not have (a plugin's, say).
    #[must_use]
    pub fn known_migration_versions(&self) -> Vec<String> {
        self.inner.migrations.known_versions()
    }

    /// Apply the pending migrations of one database on disk, without opening
    /// a pool. Returns the number of applied migrations. It does not create
    /// the database.
    ///
    /// # Errors
    ///
    /// [`FleetError::InvalidKey`], [`FleetError::NotFound`] when the file is
    /// missing, [`FleetError::Misplaced`] for a file copied from another key,
    /// or a migration failure.
    pub async fn migrate(&self, key: &FleetDbKey) -> Result<usize, FleetError> {
        self.check_key(key)?;
        if !self.exists(key) {
            return Err(FleetError::NotFound { name: key.name() });
        }
        let applied = self.migrate_existing(key).await?;
        // Deleted since the check above: `migrate_existing` skipped it.
        if applied == 0 && !self.exists(key) {
            return Err(FleetError::NotFound { name: key.name() });
        }
        self.inner
            .counters
            .migrations_applied
            .fetch_add(applied as u64, Ordering::Relaxed);
        Ok(applied)
    }

    async fn migrate_existing(&self, key: &FleetDbKey) -> Result<usize, FleetError> {
        let (fleet, task_key) = (self.clone(), key.clone());
        crate::time::spawn_blocking(move || migrate_existing_blocking(&fleet, &task_key))
            .await
            .map_err(|e| FleetError::Migration {
                name: key.name(),
                detail: e.to_string(),
            })
            .and_then(|r| r)
    }

    /// Read the identity row and the applied migration versions of one
    /// database. It does not claim, migrate or create the file.
    ///
    /// # Errors
    ///
    /// [`FleetError::InvalidKey`], [`FleetError::NotFound`] when the file is
    /// missing, [`FleetError::Io`] for a path out of the root, or
    /// [`FleetError::Unavailable`] when the file cannot be read.
    pub async fn inspect(&self, key: &FleetDbKey) -> Result<FleetDbInfo, FleetError> {
        self.check_key(key)?;
        let (path, root, name) = (self.path_of(key), self.inner.root.clone(), key.name());
        crate::time::spawn_blocking(move || {
            check_contained(&root, &path)?;
            if !path.is_file() {
                return Err(FleetError::NotFound { name });
            }
            inspect_blocking(&path, &name)
        })
        .await
        .map_err(|e| FleetError::Unavailable {
            name: key.name(),
            detail: e.to_string(),
        })?
    }

    /// Current counters.
    #[must_use]
    pub fn stats(&self) -> FleetStats {
        let open = self.lock_state().open.len();
        let c = &self.inner.counters;
        FleetStats {
            open,
            draining: self.inner.in_flight_drains.load(Ordering::Relaxed),
            max_open: self.inner.max_open,
            opens_total: c.opens.load(Ordering::Relaxed),
            closes_total: c.closes.load(Ordering::Relaxed),
            open_failures_total: c.open_failures.load(Ordering::Relaxed),
            migrations_applied_total: c.migrations_applied.load(Ordering::Relaxed),
            created_total: c.created.load(Ordering::Relaxed),
            deleted_total: c.deleted.load(Ordering::Relaxed),
        }
    }

    /// Keys of the databases open now, sorted.
    #[must_use]
    pub fn open_keys(&self) -> Vec<FleetDbKey> {
        let mut keys: Vec<FleetDbKey> = self.lock_state().open.keys().cloned().collect();
        keys.sort();
        keys
    }

    /// The database of `key` when it is open now. It never opens one, and it
    /// does not count as a use. Holding the result is a lease.
    #[must_use]
    pub fn get_open(&self, key: &FleetDbKey) -> Option<FleetDatabase> {
        let state = self.lock_state();
        let db = state.open.get(key)?.cell.get()?.clone();
        drop(state);
        (!db.pool.is_closed()).then_some(db)
    }
}

impl Entry {
    fn new(seq: u64) -> Self {
        Self {
            cell: OnceCell::new(),
            finished: AtomicBool::new(false),
            done: Notify::new(),
            last_use_seq: AtomicU64::new(seq),
            last_use_at: Mutex::new(crate::time::ambient_instant()),
        }
    }

    /// Wait until the opener task has ended.
    async fn wait(&self) {
        loop {
            let notified = self.done.notified();
            if self.finished.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }

    fn last_used(&self) -> std::time::Instant {
        *self
            .last_use_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn mark_used_now(&self) {
        *self
            .last_use_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = crate::time::ambient_instant();
    }

    fn touch(&self, seq: u64) {
        self.last_use_seq.store(seq, Ordering::Relaxed);
        *self
            .last_use_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = crate::time::ambient_instant();
    }
}

/// Nobody holds a lease beyond the fleet's own, no connection is checked
/// out, and nobody is waiting for one.
fn is_idle(db: &FleetDatabase) -> bool {
    let status = db.pool.status();
    Arc::strong_count(&db.lease) == 1 && status.available >= status.size && status.waiting == 0
}

/// Record the database's name in the file on first use, and refuse a file
/// that records another name.
async fn check_identity(pool: &Pool<RuntimeConnection>, name: &str) -> Result<(), FleetError> {
    use diesel_async::{RunQueryDsl as _, SimpleAsyncConnection as _};

    #[derive(diesel::QueryableByName)]
    struct Identity {
        #[diesel(sql_type = diesel::sql_types::Text)]
        name: String,
    }

    let unavailable = |detail: String| FleetError::Unavailable {
        name: name.to_owned(),
        detail,
    };
    let mut conn = pool.get().await.map_err(|e| unavailable(e.to_string()))?;
    conn.batch_execute(&format!(
        "CREATE TABLE IF NOT EXISTS {IDENTITY_TABLE} (\
             id INTEGER PRIMARY KEY CHECK (id = 1), \
             name TEXT NOT NULL)"
    ))
    .await
    .map_err(|e| unavailable(e.to_string()))?;
    diesel::sql_query(format!(
        "INSERT OR IGNORE INTO {IDENTITY_TABLE} (id, name) VALUES (1, ?)"
    ))
    .bind::<diesel::sql_types::Text, _>(name)
    .execute(&mut *conn)
    .await
    .map_err(|e| unavailable(e.to_string()))?;
    let found: Identity =
        diesel::sql_query(format!("SELECT name FROM {IDENTITY_TABLE} WHERE id = 1"))
            .get_result(&mut *conn)
            .await
            .map_err(|e| unavailable(e.to_string()))?;
    if found.name != name {
        return Err(FleetError::Misplaced {
            name: name.to_owned(),
            found: found.name,
        });
    }
    Ok(())
}

/// Migrate one database `migrate_all` found on disk. Blocking.
///
/// Same rules as `open`: the path must stay inside the root, and a file
/// copied or renamed onto this key's path (another database's data) is
/// reported, never migrated. A file deleted since it was listed (a live
/// process sharing the root deleted the tenant) is skipped: it is opened
/// without create, so migrating it cannot bring back an empty database.
fn migrate_existing_blocking(fleet: &DatabaseFleet, key: &FleetDbKey) -> Result<usize, FleetError> {
    let path = fleet.path_of(key);
    let name = key.name();
    check_contained(fleet.root(), &path)?;
    let Some(_pin) = pin_existing(&path).map_err(|e| FleetError::Unavailable {
        name: name.clone(),
        detail: e.to_string(),
    })?
    else {
        return Ok(0);
    };
    let url = existing_only_url(&path);
    match check_identity_blocking(&url, &name) {
        Ok(()) => fleet.inner.migrations.apply(&url, &name),
        Err(_) if !path.exists() => Ok(0),
        Err(error) => Err(error),
    }
}

/// [`DatabaseFleet::inspect`] on a blocking, query-only connection.
fn inspect_blocking(path: &Path, name: &str) -> Result<FleetDbInfo, FleetError> {
    use diesel::connection::SimpleConnection as _;
    use diesel::{OptionalExtension as _, RunQueryDsl as _};

    #[derive(diesel::QueryableByName)]
    struct Value {
        #[diesel(sql_type = diesel::sql_types::Text)]
        value: String,
    }

    let unavailable = |detail: String| FleetError::Unavailable {
        name: name.to_owned(),
        detail,
    };
    let Some(_pin) = pin_existing(path).map_err(|e| unavailable(e.to_string()))? else {
        return Err(FleetError::NotFound {
            name: name.to_owned(),
        });
    };
    let mut conn = crate::db::establish_sqlite_migration_connection(&existing_only_url(path))
        .map_err(|e| unavailable(e.to_string()))?;
    conn.batch_execute("PRAGMA query_only = ON;")
        .map_err(|e| unavailable(e.to_string()))?;
    // Rows of `sql`, or none when `table` does not exist.
    let mut read = |table: &str, sql: &str| -> Result<Vec<String>, FleetError> {
        let exists = diesel::sql_query(
            "SELECT name AS value FROM sqlite_master WHERE type = 'table' AND name = ?",
        )
        .bind::<diesel::sql_types::Text, _>(table)
        .get_result::<Value>(&mut conn)
        .optional()
        .map_err(|e| unavailable(e.to_string()))?
        .is_some();
        if !exists {
            return Ok(Vec::new());
        }
        Ok(diesel::sql_query(sql)
            .load::<Value>(&mut conn)
            .map_err(|e| unavailable(e.to_string()))?
            .into_iter()
            .map(|row| row.value)
            .collect())
    };
    Ok(FleetDbInfo {
        identity: read(
            IDENTITY_TABLE,
            &format!("SELECT name AS value FROM {IDENTITY_TABLE} WHERE id = 1"),
        )?
        .pop(),
        applied_migrations: read(
            "__diesel_schema_migrations",
            "SELECT version AS value FROM __diesel_schema_migrations ORDER BY version",
        )?,
    })
}

/// A connection string that opens `path` only if the file exists. On Unix a
/// `file:` URI with `mode=rw`, which `SQLite` refuses to create through
/// (`%`, `?` and `#` are escaped, as the URI grammar needs). Elsewhere the
/// plain path, whose canonical Windows spelling (`\\?\C:\…`) has no URI
/// form; there [`pin_existing`] keeps the file in place while it is opened.
fn existing_only_url(path: &Path) -> String {
    #[cfg(unix)]
    {
        let text = path.to_string_lossy();
        let escaped = text
            .replace('%', "%25")
            .replace('?', "%3F")
            .replace('#', "%23");
        format!("file:{escaped}?mode=rw")
    }
    #[cfg(not(unix))]
    {
        DatabaseFleet::url_of(path)
    }
}

/// Keep `path` from being removed or replaced while the caller opens it, on
/// Windows, where [`existing_only_url`] cannot refuse to create the file.
/// The handle shares read and write but not delete, so a removal or rename
/// fails while it lives; `SQLite` also opens without `FILE_SHARE_DELETE`, so
/// the file stays put for as long as the caller's connections hold it.
/// `None` when the file does not exist.
#[cfg(windows)]
fn pin_existing(path: &Path) -> std::io::Result<Option<std::fs::File>> {
    use std::os::windows::fs::OpenOptionsExt as _;
    // FILE_SHARE_READ | FILE_SHARE_WRITE
    match std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0x1 | 0x2)
        .open(path)
    {
        Ok(file) => Ok(Some(file)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Nothing to hold where [`existing_only_url`] refuses to create the file.
#[cfg(not(windows))]
#[allow(clippy::unnecessary_wraps)]
const fn pin_existing(_path: &Path) -> std::io::Result<Option<()>> {
    Ok(Some(()))
}

/// [`check_identity`] on a blocking connection, for paths that migrate a file
/// without opening a pool ([`DatabaseFleet::migrate_all`]).
fn check_identity_blocking(url: &str, name: &str) -> Result<(), FleetError> {
    use diesel::RunQueryDsl as _;
    use diesel::connection::SimpleConnection as _;

    #[derive(diesel::QueryableByName)]
    struct Identity {
        #[diesel(sql_type = diesel::sql_types::Text)]
        name: String,
    }

    let unavailable = |detail: String| FleetError::Unavailable {
        name: name.to_owned(),
        detail,
    };
    let mut conn = crate::db::establish_sqlite_migration_connection(url)
        .map_err(|e| unavailable(e.to_string()))?;
    conn.batch_execute(&format!(
        "CREATE TABLE IF NOT EXISTS {IDENTITY_TABLE} (\
             id INTEGER PRIMARY KEY CHECK (id = 1), \
             name TEXT NOT NULL)"
    ))
    .map_err(|e| unavailable(e.to_string()))?;
    diesel::sql_query(format!(
        "INSERT OR IGNORE INTO {IDENTITY_TABLE} (id, name) VALUES (1, ?)"
    ))
    .bind::<diesel::sql_types::Text, _>(name)
    .execute(&mut conn)
    .map_err(|e| unavailable(e.to_string()))?;
    let found: Identity =
        diesel::sql_query(format!("SELECT name FROM {IDENTITY_TABLE} WHERE id = 1"))
            .get_result(&mut conn)
            .map_err(|e| unavailable(e.to_string()))?;
    if found.name != name {
        return Err(FleetError::Misplaced {
            name: name.to_owned(),
            found: found.name,
        });
    }
    Ok(())
}

/// The next candidate staging name in this process; see [`staging_path`].
static STAGING_SEQ: AtomicU64 = AtomicU64::new(0);

/// A private path beside `path` for building a database before publishing it:
/// `<dir>/<file>`, where `<dir>` is `.<file>.<tag>-<pid>-<n>`, a directory no
/// template matches (it starts with a dot). The directory is created
/// exclusively, so the name is this caller's alone even across processes that
/// share the volume but not a PID namespace (two containers can both be PID
/// 1): a name someone else holds is skipped. Release it with
/// [`discard_staging`].
pub(crate) fn staging_path(path: &Path, tag: &str) -> Result<PathBuf, FleetError> {
    let file_name = path.file_name().ok_or_else(|| FleetError::Io {
        op: "reserve staging name",
        detail: format!("{} has no file name", path.display()),
    })?;
    let file_label = file_name.to_string_lossy();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(io_error("create database directory"))?;
    }
    loop {
        let dir = path.with_file_name(format!(
            ".{file_label}.{tag}-{}-{}",
            std::process::id(),
            STAGING_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok(dir.join(file_name)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(io_error("reserve staging name")(e)),
        }
    }
}

/// Remove a staging file, any sidecars `SQLite` left beside it, and the
/// directory [`staging_path`] reserved for it.
pub(crate) fn discard_staging(staging: &Path) {
    let _ = std::fs::remove_file(staging);
    for sidecar in crate::fleet_layout::sidecar_paths(staging) {
        let _ = std::fs::remove_file(sidecar);
    }
    if let Some(dir) = staging.parent() {
        // Only an empty directory goes: nothing of anyone else's is touched.
        let _ = std::fs::remove_dir(dir);
    }
}

/// Refuse a database path that leads out of the fleet's (canonical) root: the
/// file itself a symlink, or a directory on the way to it resolving
/// elsewhere (the nearest one that exists, so a directory not created yet is
/// judged by where it would be created). A stale or planted link would
/// otherwise let a key open, claim and migrate a file that is not a fleet
/// database (the control database, say). Blocking.
fn check_contained(root: &Path, path: &Path) -> Result<(), FleetError> {
    let refuse = |detail: String| FleetError::Io {
        op: "check database path",
        detail,
    };
    if std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return Err(refuse(format!(
            "{} is a symlink; a fleet database must be a regular file inside the root",
            path.display()
        )));
    }
    // The nearest directory that exists decides: a missing one below it would
    // be created through whatever it resolves to.
    let existing = path
        .ancestors()
        .skip(1)
        .find_map(|dir| std::fs::canonicalize(dir).ok());
    if let Some(resolved) = existing
        && !resolved.starts_with(root)
    {
        return Err(refuse(format!(
            "{} resolves to {}, outside the fleet root {}",
            path.display(),
            resolved.display(),
            root.display()
        )));
    }
    Ok(())
}

/// Prove no other process has `path` open, and keep it that way while the
/// returned connection lives. Blocking. `Ok(None)` when there is no file.
///
/// The connection takes `locking_mode = EXCLUSIVE` with no busy wait and
/// switches the database out of WAL. `SQLite` refuses that switch while any
/// other connection — in any process — has the file open, so a refusal is
/// [`FleetError::InUseElsewhere`]. On success the database is in rollback
/// mode with no `-wal`, and the exclusive lock keeps new openers out until
/// the guard is dropped; closing it then cannot touch a `-wal` some later
/// opener creates at the same path. On Windows, callers drop it before they
/// remove files.
fn exclusive_guard(
    path: &Path,
    name: &str,
) -> Result<Option<diesel::SqliteConnection>, FleetError> {
    use diesel::RunQueryDsl as _;
    use diesel::connection::SimpleConnection as _;

    #[derive(diesel::QueryableByName)]
    struct JournalMode {
        #[diesel(sql_type = diesel::sql_types::Text)]
        journal_mode: String,
    }

    if !path.is_file() {
        return Ok(None);
    }
    let in_use = || FleetError::InUseElsewhere {
        name: name.to_owned(),
    };
    let mut conn = crate::db::establish_sqlite_migration_connection(&DatabaseFleet::url_of(path))
        .map_err(|e| FleetError::Unavailable {
        name: name.to_owned(),
        detail: e.to_string(),
    })?;
    conn.batch_execute("PRAGMA busy_timeout = 0; PRAGMA locking_mode = EXCLUSIVE;")
        .map_err(|_| in_use())?;
    let mode = diesel::sql_query("PRAGMA journal_mode = DELETE")
        .get_result::<JournalMode>(&mut conn)
        .map_err(|_| in_use())?;
    if !mode.journal_mode.eq_ignore_ascii_case("delete") {
        return Err(in_use());
    }
    conn.batch_execute("BEGIN EXCLUSIVE; COMMIT;")
        .map_err(|_| in_use())?;
    Ok(Some(conn))
}

/// Whether the guard stays open while files are removed. Unix removes open
/// files. Windows does not: `SQLite` opens files without `FILE_SHARE_DELETE`,
/// so our guard would block the removal.
const HOLD_GUARD_WHILE_REMOVING: bool = !cfg!(windows);

/// Run `work` once no other process has `path` open. Blocking. `work` gets
/// whether the guard found the file.
///
/// With `hold`, the guard stays open during `work`. Without it, the guard
/// closes first. Then another process can open the file before `work` runs,
/// and `work` must handle that. Windows refuses to delete an open file (see
/// [`removal_error`]). [`publish_restored`] refuses a file that appeared.
fn with_exclusive_access<T>(
    path: &Path,
    name: &str,
    hold: bool,
    work: impl FnOnce(bool) -> Result<T, FleetError>,
) -> Result<T, FleetError> {
    let guard = exclusive_guard(path, name)?;
    let present = guard.is_some();
    if hold {
        let done = work(present);
        drop(guard);
        done
    } else {
        drop(guard);
        work(present)
    }
}

/// Map a failed file removal. Windows error 32 (sharing violation) means
/// another process has the file open. Return [`FleetError::InUseElsewhere`].
fn removal_error<'a>(
    op: &'static str,
    name: &'a str,
) -> impl FnOnce(std::io::Error) -> FleetError + 'a {
    const ERROR_SHARING_VIOLATION: i32 = 32;
    move |e| {
        if e.raw_os_error() == Some(ERROR_SHARING_VIOLATION) {
            FleetError::InUseElsewhere {
                name: name.to_owned(),
            }
        } else {
            io_error(op)(e)
        }
    }
}

/// Link the staged copy to `path`. Never replace a file. If a file appeared
/// after the removal, return [`FleetError::AlreadyExists`].
fn publish_restored(staging: &Path, path: &Path, name: &str) -> Result<(), FleetError> {
    std::fs::hard_link(staging, path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            FleetError::AlreadyExists {
                name: name.to_owned(),
            }
        } else {
            io_error("publish restored database")(e)
        }
    })
}

/// Replace the database at `path` with its replica. Blocking.
///
/// The replica is rebuilt in a private staging file first, so a failed
/// restore leaves the current file untouched. Then the old files go and the
/// staging file is linked into place. The old main file goes last. On Unix,
/// the exclusive guard stays open for this. On Windows, it closes first. A file that appears in that gap
/// gives `AlreadyExists`.
fn restore_in_place(
    root: &Path,
    replication: &super::fleet_replication::FleetReplication,
    key: &FleetDbKey,
    target: Option<chrono::DateTime<chrono::Utc>>,
    path: &Path,
) -> Result<crate::replication::RestoreOutcome, FleetError> {
    let name = key.name();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(io_error("create database directory"))?;
    }
    let staging = staging_path(path, "restoring")?;
    let restored = replication
        .restore_to(key, target, &staging)
        .map_err(|error| match error {
            crate::replication::RestoreError::NoReplica { .. } => {
                FleetError::NotFound { name: name.clone() }
            }
            error => FleetError::Restore {
                name: name.clone(),
                detail: error.to_string(),
            },
        });
    let outcome = match restored {
        Ok(outcome) => outcome,
        Err(error) => {
            discard_staging(&staging);
            return Err(error);
        }
    };
    let published = with_path_lock(root, &name, || {
        with_exclusive_access(path, &name, HOLD_GUARD_WHILE_REMOVING, |present| {
            unlink_database_set(path, "replace database", &name, present, |file| {
                std::fs::remove_file(file)
            })?;
            publish_restored(&staging, path, &name)
        })
    });
    discard_staging(&staging);
    published.map(|()| crate::replication::RestoreOutcome {
        output: path.to_path_buf(),
        ..outcome
    })
}

/// Create a fleet database atomically. Blocking.
///
/// The database is built — identity row, every migration — in a private
/// staging file beside `path` (`.<name>.creating-<pid>-<n>`, which no
/// template matches), then published with a hard link, which refuses an
/// existing target. So a database is either absent or complete, never half
/// made; a failure removes only the staging file; and when another process
/// publishes first, its file wins and this returns `Ok(false)`.
fn create_database_file(
    root: &Path,
    path: &Path,
    name: &str,
    migrations: &FleetMigrations,
) -> Result<bool, FleetError> {
    let parent = path.parent().ok_or_else(|| FleetError::Io {
        op: "create database",
        detail: "database path has no parent directory".to_owned(),
    })?;
    std::fs::create_dir_all(parent).map_err(io_error("create database directory"))?;
    let staging = staging_path(path, "creating")?;
    let discard = discard_staging;
    let built = build_staged_database(&staging, name, migrations);
    if let Err(error) = built {
        discard(&staging);
        return Err(error);
    }
    // The path lock is held only for the publish, not for the build above.
    let published = with_path_lock(root, name, || {
        // Under the lock nobody else can publish here. A path with no main
        // file holds no live database, so its stray sidecars are stale.
        if !path.exists() {
            unlink_database_set(path, "clear stray sidecars", name, false, |file| {
                std::fs::remove_file(file)
            })?;
        }
        Ok(std::fs::hard_link(&staging, path))
    });
    discard(&staging);
    let published = published?;
    match published {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(io_error("publish database")(e)),
    }
}

/// Identity row and migrations on a staging file, closed before it returns.
fn build_staged_database(
    staging: &Path,
    name: &str,
    migrations: &FleetMigrations,
) -> Result<(), FleetError> {
    use diesel::RunQueryDsl as _;
    use diesel::connection::SimpleConnection as _;
    let url = DatabaseFleet::url_of(staging);
    let unavailable = |detail: String| FleetError::Unavailable {
        name: name.to_owned(),
        detail,
    };
    {
        let mut conn = crate::db::establish_sqlite_migration_connection(&url)
            .map_err(|e| unavailable(e.to_string()))?;
        conn.batch_execute(&format!(
            "CREATE TABLE IF NOT EXISTS {IDENTITY_TABLE} (\
                 id INTEGER PRIMARY KEY CHECK (id = 1), \
                 name TEXT NOT NULL)"
        ))
        .map_err(|e| unavailable(e.to_string()))?;
        diesel::sql_query(format!(
            "INSERT OR IGNORE INTO {IDENTITY_TABLE} (id, name) VALUES (1, ?)"
        ))
        .bind::<diesel::sql_types::Text, _>(name)
        .execute(&mut conn)
        .map_err(|e| unavailable(e.to_string()))?;
    }
    migrations.apply(&url, name)?;
    Ok(())
}

/// Remove a database file, its sidecars, and the directories the template
/// created for it when they are now empty (never `root` itself). `Ok(false)`
/// when there was no file. The main file goes last (see
/// [`unlink_database_set`]). After that, only `remove_dir` runs, and it
/// refuses a non-empty directory.
fn remove_database_files(
    path: &Path,
    root: &Path,
    name: &str,
    present: bool,
) -> Result<bool, FleetError> {
    unlink_database_set(path, "delete database", name, present, |file| {
        std::fs::remove_file(file)
    })?;
    let mut dir = path.parent();
    while let Some(current) = dir {
        if current == root || !current.starts_with(root) {
            break;
        }
        // `remove_dir` refuses a non-empty directory, which is the check.
        if std::fs::remove_dir(current).is_err() {
            break;
        }
        dir = current.parent();
    }
    Ok(present)
}

/// Unlink a database's sidecars, then its main file, with `unlink`. A missing
/// file is fine.
///
/// The main file goes last. Once it is gone, another process can publish a
/// new database at the path. Any unlink after that could hit the new
/// database's `-wal`.
///
/// An absent main file stays untouched, and only its stray sidecars go. The
/// caller holds the path lock ([`with_path_lock`]), so no other process can
/// publish a database at the path meanwhile. A stale `-wal` left by a crash
/// would otherwise replay onto the next database made there.
///
/// On Windows, a process that has the database open also holds its `-wal`.
/// The sharing violation then stops the unlink at the sidecar, before the
/// main file goes.
fn unlink_database_set(
    path: &Path,
    op: &'static str,
    name: &str,
    had_main: bool,
    mut unlink: impl FnMut(&Path) -> std::io::Result<()>,
) -> Result<(), FleetError> {
    // `had_main` is what the exclusive guard saw. Do not look again.
    let main = had_main.then(|| path.to_path_buf());
    for file in crate::fleet_layout::sidecar_paths(path)
        .into_iter()
        .chain([crate::fleet_layout::replicator_lock_path(path)])
        .chain(main)
    {
        match unlink(&file) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(removal_error(op, name)(e)),
        }
    }
    Ok(())
}

// ── The path lock ────────────────────────────────────────────────────────────

/// Directory under the fleet root that holds the path locks.
const LOCK_DIR: &str = crate::fleet_layout::RESERVED_DIR;

/// How many lock files the fleet uses. A key maps to one of them by a stable
/// hash, so a fleet of millions of tenants needs no file per tenant, and a
/// lock file is never deleted (deleting one would break the exclusion).
const LOCK_STRIPES: u64 = 256;

/// Pause between attempts to take a held path lock.
const LOCK_POLL: Duration = Duration::from_millis(5);

/// Attempts before a path lock times out: about 60 seconds.
const LOCK_POLLS: u32 = 12_000;

/// FNV-1a over the key name. Not `DefaultHasher`: processes built by
/// different compilers share one root and must agree on the lock file.
fn lock_stripe(name: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in name.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash % LOCK_STRIPES
}

/// Run `work` while holding the cross-process lock for `name`'s path.
/// Blocking. Every process that shares the root takes this lock to publish a
/// database at a path (create, restore) or to remove one (delete). So a
/// process that finds the path empty under the lock knows that no other
/// process can publish there until it lets go. It can then clear the stray
/// sidecars of the absent database without risking a live `-wal`.
///
/// The OS drops the lock when a process dies, so a crash leaves no stale lock.
/// Keep `work` short: other keys on the same lock file wait for it.
fn with_path_lock<T>(
    root: &Path,
    name: &str,
    work: impl FnOnce() -> Result<T, FleetError>,
) -> Result<T, FleetError> {
    let dir = root.join(LOCK_DIR);
    std::fs::create_dir_all(&dir).map_err(io_error("create lock directory"))?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(format!("{:02x}.lock", lock_stripe(name))))
        .map_err(io_error("open path lock"))?;
    let mut lock = fd_lock::RwLock::new(file);
    let mut polls = 0_u32;
    let guard = loop {
        match lock.try_write() {
            Ok(guard) => break guard,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                polls += 1;
                if polls >= LOCK_POLLS {
                    return Err(FleetError::Unavailable {
                        name: name.to_owned(),
                        detail: "timed out waiting for the path lock".to_owned(),
                    });
                }
                std::thread::sleep(LOCK_POLL);
            }
            Err(e) => return Err(io_error("lock database path")(e)),
        }
    };
    let done = work();
    drop(guard);
    done
}

/// Readiness indicator `db:fleet`: the root directory is usable, with the
/// fleet's counters as details. Individual databases are not pinged — a
/// tenant fleet can hold millions — a failed open answers its own request.
/// An OS lock on `<root>/.fleet-locks/owner.lock`. Each process with
/// ownership holds it shared. A process that gets it exclusive is the last one
/// on the volume. The OS frees it when the process ends.
struct VolumeLock {
    _file: std::fs::File,
}

impl VolumeLock {
    fn open(root: &Path) -> std::io::Result<fd_lock::RwLock<std::fs::File>> {
        let dir = root.join(crate::fleet_layout::RESERVED_DIR);
        std::fs::create_dir_all(&dir)?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join("owner.lock"))?;
        Ok(fd_lock::RwLock::new(file))
    }

    /// Take the lock shared. Blocks only while a process holds it exclusive
    /// (for a moment, at its shutdown).
    fn shared(root: &Path) -> std::io::Result<Self> {
        let lock = Self::open(root)?;
        // Keep it held: closing the file frees it.
        std::mem::forget(lock.read()?);
        Ok(Self {
            _file: lock.into_inner(),
        })
    }

    /// Take the lock exclusive, without waiting. `None` while another
    /// process holds it.
    fn exclusive(root: &Path) -> std::io::Result<Option<Self>> {
        let mut lock = Self::open(root)?;
        match lock.try_write() {
            Ok(guard) => std::mem::forget(guard),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
            Err(e) => return Err(e),
        }
        Ok(Some(Self {
            _file: lock.into_inner(),
        }))
    }
}

/// Sleep until `at`, or forever when there is none.
async fn sleep_until_some(at: Option<tokio::time::Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

pub(crate) struct FleetHealthIndicator {
    fleet: DatabaseFleet,
}

impl FleetHealthIndicator {
    pub(crate) const fn new(fleet: DatabaseFleet) -> Self {
        Self { fleet }
    }
}

impl crate::actuator::HealthIndicator for FleetHealthIndicator {
    fn check(&self) -> futures::future::BoxFuture<'_, crate::actuator::HealthCheckOutput> {
        Box::pin(async move {
            use crate::actuator::HealthCheckOutput;
            let root = self.fleet.root().to_path_buf();
            let writable = crate::time::spawn_blocking(move || {
                // Unique per check, created new and removed by this check only:
                // concurrent probes (or processes) never race on one name.
                let probe = staging_path(&root.join("fleet"), "probe")
                    .map_err(|e| std::io::Error::other(e.to_string()))?;
                let written = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&probe)
                    .map(drop);
                discard_staging(&probe);
                written
            })
            .await;
            let stats = self.fleet.stats();
            let mut details: HashMap<String, serde_json::Value> = serde_json::to_value(stats)
                .ok()
                .and_then(|value| serde_json::from_value(value).ok())
                .unwrap_or_default();
            details.insert("mode".to_owned(), self.fleet.mode().to_string().into());
            details.insert(
                "replicating".to_owned(),
                serde_json::Value::Bool(self.fleet.replicating()),
            );
            match writable {
                Ok(Ok(())) => HealthCheckOutput::up().with_details(details),
                Ok(Err(e)) => {
                    details.insert(
                        "error".to_owned(),
                        format!("fleet root is not writable: {e}").into(),
                    );
                    HealthCheckOutput::down().with_details(details)
                }
                Err(e) => {
                    details.insert("error".to_owned(), e.to_string().into());
                    HealthCheckOutput::down().with_details(details)
                }
            }
        })
    }
}

/// The fleet built for an app at boot, from `[database.fleet]` and the app's
/// registered migrations (the control-plane framework set excluded).
pub(crate) fn build_for_app(
    config: &crate::config::AutumnConfig,
    resolved_control_url: Option<&str>,
    all: &Arc<Vec<(&'static str, EmbeddedMigrations)>>,
    keep: impl Fn(&EmbeddedMigrations) -> bool,
) -> Result<Option<DatabaseFleet>, String> {
    let Some(fleet_config) = config.database.fleet.as_ref() else {
        return Ok(None);
    };
    let apply_on_open = crate::migrate::should_auto_apply(
        config.profile.as_deref(),
        config.database.auto_migrate,
        config.database.auto_migrate_in_production,
    );
    let fleet = DatabaseFleet::from_parts(
        fleet_config,
        config.database.connect_timeout_secs,
        FleetMigrations::new(all, keep, apply_on_open),
        DEFAULT_MIN_RESIDENCY,
    )
    .map_err(|e| format!("Failed to set up database.fleet: {e}"))?;
    // The configured control database, and the one a custom pool provider
    // resolved at runtime (it may differ).
    let control_urls: Vec<&str> = config
        .database
        .effective_primary_url()
        .into_iter()
        .chain(resolved_control_url)
        .collect();
    fleet
        .check_control_outside_root(&control_urls)
        .map_err(|e| format!("Failed to set up database.fleet: {e}"))?;
    Ok(Some(fleet))
}

/// `path` with every symlink resolved: the file itself when it exists, else
/// the nearest directory on its way that exists, made canonical, plus the
/// rest of the path. A directory not created yet is judged by where it would
/// be created (as in `check_contained`). `None` when no ancestor exists.
fn canonical_target(path: &Path) -> Option<PathBuf> {
    if let Ok(resolved) = std::fs::canonicalize(path) {
        return Some(resolved);
    }
    let mut rest = Vec::new();
    let mut dir = path;
    loop {
        rest.push(dir.file_name()?);
        dir = dir.parent()?;
        let existing = if dir.as_os_str().is_empty() {
            Path::new(".")
        } else {
            dir
        };
        if let Ok(mut resolved) = std::fs::canonicalize(existing) {
            resolved.extend(rest.iter().rev());
            return Some(resolved);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn config(root: &Path, mode: FleetMode) -> DatabaseFleetConfig {
        DatabaseFleetConfig {
            mode,
            root: root.display().to_string(),
            path: None,
            max_open: 8,
            pool_size: 2,
            create_on_demand: None,
            idle_close_secs: 0,
            restore_missing: false,
            ownership: None,
        }
    }

    fn fleet(
        root: &Path,
        mode: FleetMode,
        tweak: impl FnOnce(&mut DatabaseFleetConfig),
    ) -> DatabaseFleet {
        let mut config = config(root, mode);
        tweak(&mut config);
        DatabaseFleet::builder(config)
            .min_residency(Duration::ZERO)
            .migrations(
                "version-history",
                crate::version_history::VERSION_HISTORY_MIGRATIONS,
            )
            .build()
            .unwrap()
    }

    async fn table_exists(db: &FleetDatabase, table: &str) -> bool {
        use diesel_async::RunQueryDsl as _;
        #[derive(diesel::QueryableByName)]
        struct Count {
            #[diesel(sql_type = diesel::sql_types::BigInt)]
            n: i64,
        }
        let mut conn = db.pool().get().await.unwrap();
        let count: Count = diesel::sql_query(
            "SELECT COUNT(*) AS n FROM sqlite_master WHERE type = 'table' AND name = ?",
        )
        .bind::<diesel::sql_types::Text, _>(table)
        .get_result(&mut *conn)
        .await
        .unwrap();
        count.n == 1
    }

    /// How long a test waits for a call that must not hang. A stranded drain
    /// hangs for ever, so a long bound still catches it. A short one fails on
    /// a slow runner: a database create takes seconds on Windows CI, and a
    /// build plus its migrations takes longer still.
    const NOT_STUCK: Duration = Duration::from_secs(60);

    async fn settle(fleet: &DatabaseFleet) {
        for _ in 0..500 {
            if fleet.stats().draining == 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("drains did not settle");
    }

    #[tokio::test]
    async fn a_slot_database_is_created_migrated_and_tagged_on_first_use() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let db = fleet.open_for("acme").await.unwrap();
        let slot = crate::sharding::slot_for_key("acme".into()).0;
        assert_eq!(db.key(), &FleetDbKey::Slot(slot));
        let expected = fleet
            .root()
            .join(format!("{:03}/slot-{slot:05}.db", slot / 64));
        assert_eq!(db.path(), expected);
        assert!(expected.is_file());
        assert!(table_exists(&db, "_autumn_version_history").await);
        assert!(table_exists(&db, IDENTITY_TABLE).await);
        let stats = fleet.stats();
        assert_eq!(
            (stats.open, stats.opens_total, stats.created_total),
            (1, 1, 1)
        );
        assert_eq!(stats.migrations_applied_total, 3);
        // Two tenants in the same slot share the database.
        let again = fleet.open(&FleetDbKey::Slot(slot)).await.unwrap();
        assert_eq!(again.path(), db.path());
        assert_eq!(fleet.stats().opens_total, 1);
    }

    #[tokio::test]
    async fn a_tenant_database_must_be_provisioned_unless_created_on_demand() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Tenant, |_| {});
        let key = fleet.key_for("acme").unwrap();
        assert!(matches!(
            fleet.open(&key).await,
            Err(FleetError::NotFound { .. })
        ));
        assert!(!fleet.exists(&key));
        assert_eq!(fleet.stats().open, 0, "a failed open leaves nothing behind");

        let db = fleet.provision(&key).await.unwrap();
        assert!(db.path().ends_with(format!("{:03}/acme.db", key.bucket())));
        assert!(matches!(
            fleet.provision(&key).await,
            Err(FleetError::AlreadyExists { .. })
        ));
        fleet.open(&key).await.unwrap();

        assert!(matches!(
            fleet.open_for("../etc/passwd").await,
            Err(FleetError::InvalidKey(_))
        ));
        assert!(matches!(
            fleet.open_for("Acme").await,
            Err(FleetError::InvalidKey(_))
        ));

        let on_demand = super::DatabaseFleet::builder(DatabaseFleetConfig {
            create_on_demand: Some(true),
            ..config(tmp.path(), FleetMode::Tenant)
        })
        .build()
        .unwrap();
        on_demand.open_for("globex").await.unwrap();
    }

    #[tokio::test]
    async fn concurrent_first_opens_share_one_open_and_one_migration() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let key = FleetDbKey::Slot(7);
        let opens = futures::future::join_all((0..16).map(|_| fleet.open(&key))).await;
        assert!(opens.iter().all(Result::is_ok));
        let stats = fleet.stats();
        assert_eq!(stats.opens_total, 1);
        assert_eq!(stats.migrations_applied_total, 3);
    }

    #[tokio::test]
    async fn the_least_recently_used_idle_database_is_closed_past_max_open() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |c| c.max_open = 2);
        let first = fleet.open(&FleetDbKey::Slot(1)).await.unwrap();
        fleet.open(&FleetDbKey::Slot(2)).await.unwrap();
        fleet.open(&FleetDbKey::Slot(1)).await.unwrap(); // 1 is now newer than 2
        fleet.open(&FleetDbKey::Slot(3)).await.unwrap();
        settle(&fleet).await;
        assert_eq!(
            fleet.open_keys(),
            vec![FleetDbKey::Slot(1), FleetDbKey::Slot(3)]
        );
        assert!(!first.pool().is_closed());
        assert_eq!(fleet.stats().closes_total, 1);
        // A closed database reopens on demand.
        fleet.open(&FleetDbKey::Slot(2)).await.unwrap();
        settle(&fleet).await;
        assert_eq!(fleet.stats().open, 2);
    }

    #[tokio::test]
    async fn a_database_used_within_min_residency_is_not_closed_by_lru() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = DatabaseFleet::builder(DatabaseFleetConfig {
            max_open: 1,
            ..config(tmp.path(), FleetMode::Slot)
        })
        .min_residency(Duration::from_secs(60))
        .build()
        .unwrap();
        let resolved = fleet.open(&FleetDbKey::Slot(1)).await.unwrap();
        fleet.open(&FleetDbKey::Slot(2)).await.unwrap();
        settle(&fleet).await;
        assert_eq!(
            fleet.stats().open,
            2,
            "the cap is soft while both are recent"
        );
        // The handle resolved first still checks out: nothing closed it.
        assert!(resolved.pool().get().await.is_ok());
    }

    #[tokio::test]
    async fn the_sweeper_closes_a_burst_once_its_residency_has_passed() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = DatabaseFleet::builder(DatabaseFleetConfig {
            max_open: 1,
            ..config(tmp.path(), FleetMode::Slot)
        })
        .min_residency(Duration::from_millis(150))
        .build()
        .unwrap();
        assert!(fleet.sweep_interval() <= Duration::from_millis(150));
        let started = std::time::Instant::now();
        for slot in 1..=3 {
            fleet.open(&FleetDbKey::Slot(slot)).await.unwrap();
        }
        // A slow runner can outlast the grace while it opens the burst. Then
        // the sweep may already have closed some, and this check does not apply.
        let open = fleet.stats().open;
        if started.elapsed() < Duration::from_millis(150) {
            assert_eq!(open, 3, "a burst inside the grace stays open");
        }
        settle(&fleet).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        // A sweep closes only an idle database. On a busy runner a pool can
        // still hold a connection after the sleep, so sweep as the sweeper
        // does, until the burst is closed or the deadline passes.
        for _ in 0..100 {
            fleet.enforce_capacity(None);
            settle(&fleet).await;
            if fleet.stats().open == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            fleet.stats().open,
            1,
            "the sweep enforces max_open afterwards"
        );
    }

    const BROKEN: EmbeddedMigrations =
        diesel_migrations::embed_migrations!("tests/fixtures/fleet_broken_migrations");

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = walk(dir);
        names.sort();
        names
    }

    fn walk(dir: &Path) -> Vec<String> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            if entry.file_name() == LOCK_DIR {
                continue;
            }
            if entry.file_type().unwrap().is_dir() {
                out.extend(walk(&entry.path()));
            } else {
                out.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
        out
    }

    #[tokio::test]
    async fn a_failed_first_migration_publishes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = DatabaseFleet::builder(config(tmp.path(), FleetMode::Slot))
            .migrations("broken", BROKEN)
            .build()
            .unwrap();
        let err = fleet.open(&FleetDbKey::Slot(3)).await.unwrap_err();
        assert!(matches!(err, FleetError::Migration { .. }), "{err}");
        assert!(!fleet.exists(&FleetDbKey::Slot(3)));
        assert!(
            entries(fleet.root()).is_empty(),
            "no database and no staging file left: {:?}",
            entries(fleet.root())
        );
    }

    #[tokio::test]
    async fn of_concurrent_provisions_only_the_creator_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Tenant, |_| {});
        let key = fleet.key_for("acme").unwrap();
        let results = futures::future::join_all((0..8).map(|_| fleet.provision(&key))).await;
        let created = results.iter().filter(|r| r.is_ok()).count();
        let refused = results
            .iter()
            .filter(|r| matches!(r, Err(FleetError::AlreadyExists { .. })))
            .count();
        assert_eq!((created, refused), (1, 7), "{results:?}");
        assert_eq!(fleet.stats().created_total, 1);
    }

    #[tokio::test]
    async fn a_failing_later_set_rolls_back_the_earlier_sets() {
        let tmp = tempfile::tempdir().unwrap();
        // An existing database with no migrations at all.
        let bare = DatabaseFleet::builder(config(tmp.path(), FleetMode::Slot))
            .build()
            .unwrap();
        bare.open(&FleetDbKey::Slot(8)).await.unwrap();
        bare.close_all().await;

        let fleet = DatabaseFleet::builder(config(tmp.path(), FleetMode::Slot))
            .migrations(
                "version-history",
                crate::version_history::VERSION_HISTORY_MIGRATIONS,
            )
            .migrations("broken", BROKEN)
            .build()
            .unwrap();
        let err = fleet.open(&FleetDbKey::Slot(8)).await.unwrap_err();
        assert!(matches!(err, FleetError::Migration { .. }), "{err}");

        let check = DatabaseFleet::builder(config(tmp.path(), FleetMode::Slot))
            .build()
            .unwrap();
        let db = check.open(&FleetDbKey::Slot(8)).await.unwrap();
        assert!(
            !table_exists(&db, "_autumn_version_history").await,
            "the earlier set's migrations were rolled back with the failing one"
        );
    }

    #[test]
    fn creating_a_database_another_process_published_keeps_theirs() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("000/slot-00001.db");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, b"their bytes").unwrap();
        let migrations = FleetMigrations::new(&Arc::new(Vec::new()), |_| true, true);
        let created = create_database_file(tmp.path(), &target, "slot:00001", &migrations).unwrap();
        assert!(!created, "the other process won the race");
        assert_eq!(std::fs::read(&target).unwrap(), b"their bytes");
        assert_eq!(
            entries(tmp.path()),
            vec!["slot-00001.db".to_owned()],
            "no staging left"
        );

        let fresh = tmp.path().join("000/slot-00002.db");
        assert!(create_database_file(tmp.path(), &fresh, "slot:00002", &migrations).unwrap());
        assert!(fresh.is_file());
    }

    #[tokio::test]
    async fn a_database_with_a_checked_out_connection_is_never_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |c| c.max_open = 1);
        let busy = fleet.open(&FleetDbKey::Slot(1)).await.unwrap();
        let held = busy.pool().get().await.unwrap();
        fleet.open(&FleetDbKey::Slot(2)).await.unwrap();
        fleet.open(&FleetDbKey::Slot(3)).await.unwrap();
        settle(&fleet).await;
        let open = fleet.open_keys();
        assert!(
            open.contains(&FleetDbKey::Slot(1)),
            "busy database kept: {open:?}"
        );
        assert!(!busy.closed().is_cancelled());
        drop(held);
    }

    #[tokio::test]
    async fn idle_databases_close_after_idle_close() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |c| c.idle_close_secs = 1);
        let db = fleet.open(&FleetDbKey::Slot(1)).await.unwrap();
        let (pool, closed) = (db.pool().clone(), db.closed().clone());
        drop(db);
        // The first open stamps its use before creating and migrating the
        // file, which can take over a second on a slow runner. Using the open
        // database again (a cached hit) stamps it now.
        drop(fleet.open(&FleetDbKey::Slot(1)).await.unwrap());
        assert_eq!(fleet.close_idle(), 0, "not idle long enough yet");
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert_eq!(fleet.close_idle(), 1);
        settle(&fleet).await;
        assert!(closed.is_cancelled());
        assert!(pool.is_closed());
        assert!(fleet.open_keys().is_empty());
    }

    #[tokio::test]
    async fn a_held_lease_keeps_a_database_open() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |c| {
            c.idle_close_secs = 1;
            c.max_open = 1;
        });
        // A lazily acquiring repository holds only the lease and a pool.
        let db = fleet.open(&FleetDbKey::Slot(1)).await.unwrap();
        let (lease, pool) = (db.lease(), db.pool().clone());
        drop(db);
        fleet.open(&FleetDbKey::Slot(2)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(1100)).await;
        let _ = fleet.close_idle();
        fleet.enforce_capacity(None);
        settle(&fleet).await;
        assert!(fleet.open_keys().contains(&FleetDbKey::Slot(1)));
        assert!(pool.get().await.is_ok(), "the leased pool still checks out");
        drop(lease);
        let _ = fleet.close_idle();
        settle(&fleet).await;
        assert!(!fleet.open_keys().contains(&FleetDbKey::Slot(1)));
    }

    #[tokio::test]
    async fn a_background_open_never_grows_the_fleet_past_max_open() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |c| c.max_open = 1);
        drop(fleet.open(&FleetDbKey::Slot(2)).await.unwrap());
        let hot = fleet.open(&FleetDbKey::Slot(1)).await.unwrap();
        settle(&fleet).await;
        // Slot 2 exists on disk, but the one open slot is taken.
        let refused = fleet.open_background(&FleetDbKey::Slot(2)).await.unwrap();
        assert!(refused.is_none(), "no room: the background open waits");
        settle(&fleet).await;
        assert_eq!(fleet.open_keys(), vec![FleetDbKey::Slot(1)]);
        assert!(!hot.closed().is_cancelled(), "the hot database stays open");

        // An open database is shared, and the call says it did not open it.
        let shared = fleet
            .open_background(&FleetDbKey::Slot(1))
            .await
            .unwrap()
            .expect("already open");
        assert!(!shared.opened);
        assert_eq!(fleet.stats().open, 1);
    }

    #[tokio::test]
    async fn a_background_open_never_creates_a_database() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let missing = FleetDbKey::Slot(7);
        let error = fleet.open_background(&missing).await.unwrap_err();
        assert!(matches!(error, FleetError::NotFound { .. }), "{error}");
        assert!(!fleet.exists(&missing));
        assert!(fleet.open_keys().is_empty());
    }

    #[tokio::test]
    async fn close_if_unused_spares_a_leased_or_used_database() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        for slot in [1, 2] {
            drop(fleet.open(&FleetDbKey::Slot(slot)).await.unwrap());
            fleet.close(&FleetDbKey::Slot(slot)).await;
        }

        // Opened in the background and still held: a lease.
        let held = fleet
            .open_background(&FleetDbKey::Slot(1))
            .await
            .unwrap()
            .expect("room");
        assert!(held.opened);
        assert!(!fleet.close_if_unused(&FleetDbKey::Slot(1)), "leased");
        drop(held);
        assert!(fleet.close_if_unused(&FleetDbKey::Slot(1)));
        settle(&fleet).await;
        assert!(!fleet.open_keys().contains(&FleetDbKey::Slot(1)));

        // A request used it after the background open: it is the request's now.
        let background = fleet
            .open_background(&FleetDbKey::Slot(2))
            .await
            .unwrap()
            .expect("room");
        drop(fleet.open(&FleetDbKey::Slot(2)).await.unwrap());
        drop(background);
        assert!(!fleet.close_if_unused(&FleetDbKey::Slot(2)), "used");
        settle(&fleet).await;
        assert!(fleet.open_keys().contains(&FleetDbKey::Slot(2)));
    }

    #[tokio::test]
    async fn a_background_database_is_the_first_lru_victim() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |c| c.max_open = 2);
        drop(fleet.open(&FleetDbKey::Slot(3)).await.unwrap());
        fleet.close(&FleetDbKey::Slot(3)).await;
        drop(fleet.open(&FleetDbKey::Slot(1)).await.unwrap());
        drop(
            fleet
                .open_background(&FleetDbKey::Slot(3))
                .await
                .unwrap()
                .expect("room"),
        );
        // A request past the cap evicts the background database, not slot 1.
        drop(fleet.open(&FleetDbKey::Slot(2)).await.unwrap());
        settle(&fleet).await;
        let mut open = fleet.open_keys();
        open.sort();
        assert_eq!(open, vec![FleetDbKey::Slot(1), FleetDbKey::Slot(2)]);
    }

    #[tokio::test]
    async fn keys_of_another_mode_or_out_of_range_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let slots = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let tenant = FleetDbKey::Tenant {
            id: TenantDbId::parse("acme").unwrap(),
            slot: crate::sharding::slot_for_key("acme".into()).0,
        };
        for bad in [tenant.clone(), FleetDbKey::Slot(16384)] {
            assert!(matches!(
                slots.open(&bad).await,
                Err(FleetError::InvalidKey(_))
            ));
            assert!(matches!(
                slots.delete(&bad).await,
                Err(FleetError::InvalidKey(_))
            ));
        }
        let tenants = fleet(&tmp.path().join("t"), FleetMode::Tenant, |c| {
            c.create_on_demand = Some(true);
        });
        let wrong_slot = FleetDbKey::Tenant {
            id: TenantDbId::parse("acme").unwrap(),
            slot: crate::sharding::slot_for_key("acme".into()).0 ^ 1,
        };
        for bad in [FleetDbKey::Slot(1), wrong_slot] {
            assert!(matches!(
                tenants.open(&bad).await,
                Err(FleetError::InvalidKey(_))
            ));
            assert!(matches!(
                tenants.provision(&bad).await,
                Err(FleetError::InvalidKey(_))
            ));
        }
        assert!(
            entries(tenants.root()).is_empty(),
            "nothing touched the disk"
        );
    }

    fn plain_database(dir: &Path) -> PathBuf {
        use diesel::connection::SimpleConnection as _;
        let path = dir.join("plain.db");
        crate::db::establish_sqlite_migration_connection(&DatabaseFleet::url_of(&path))
            .unwrap()
            .batch_execute("PRAGMA journal_mode = WAL; CREATE TABLE t (id INTEGER)")
            .unwrap();
        path
    }

    #[test]
    fn a_sharing_violation_is_in_use_elsewhere() {
        let busy = removal_error("delete database", "acme")(std::io::Error::from_raw_os_error(32));
        assert!(matches!(busy, FleetError::InUseElsewhere { name } if name == "acme"));
        let other = removal_error("delete database", "acme")(std::io::Error::other("disk"));
        assert!(matches!(other, FleetError::Io { .. }), "{other}");
    }

    #[test]
    fn held_access_keeps_other_openers_out() {
        let tmp = tempfile::tempdir().unwrap();
        let path = plain_database(tmp.path());
        let inner = with_exclusive_access(&path, "plain", true, |_| {
            Ok(exclusive_guard(&path, "plain").map(drop))
        })
        .unwrap();
        assert!(matches!(inner, Err(FleetError::InUseElsewhere { .. })));
    }

    #[test]
    fn released_access_lets_the_work_open_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = plain_database(tmp.path());
        let inner = with_exclusive_access(&path, "plain", false, |_| {
            Ok(exclusive_guard(&path, "plain").map(drop))
        })
        .unwrap();
        assert!(inner.is_ok(), "the guard was released first");
    }

    #[test]
    fn access_is_refused_while_another_connection_has_the_file_open() {
        use diesel::connection::SimpleConnection as _;
        let tmp = tempfile::tempdir().unwrap();
        let path = plain_database(tmp.path());
        let mut other =
            crate::db::establish_sqlite_migration_connection(&DatabaseFleet::url_of(&path))
                .unwrap();
        other.batch_execute("SELECT 1 FROM t").unwrap();
        for hold in [true, false] {
            let ran = std::cell::Cell::new(false);
            let err = with_exclusive_access(&path, "plain", hold, |_| {
                ran.set(true);
                Ok(())
            })
            .unwrap_err();
            assert!(matches!(err, FleetError::InUseElsewhere { .. }), "{err}");
            assert!(!ran.get(), "no work without proof");
        }
    }

    #[test]
    fn publishing_over_a_file_that_appeared_is_a_conflict() {
        let tmp = tempfile::tempdir().unwrap();
        let staging = tmp.path().join("staging.db");
        let path = tmp.path().join("live.db");
        std::fs::write(&staging, b"restored").unwrap();
        std::fs::write(&path, b"opened by someone else").unwrap();
        let err = publish_restored(&staging, &path, "acme").unwrap_err();
        assert!(matches!(err, FleetError::AlreadyExists { .. }), "{err}");
        assert_eq!(std::fs::read(&path).unwrap(), b"opened by someone else");
        std::fs::remove_file(&path).unwrap();
        publish_restored(&staging, &path, "acme").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"restored");
    }

    #[tokio::test]
    async fn delete_refuses_a_database_another_process_has_open() {
        use diesel::connection::SimpleConnection as _;
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let recorder = Arc::new(Recorder::default());
        fleet.add_lifecycle(recorder.clone());
        let key = FleetDbKey::Slot(4);
        let path = fleet.open(&key).await.unwrap().path().to_path_buf();
        fleet.close(&key).await;
        // Another process's connection (a separate handle to the same file).
        let mut other = crate::db::establish_sqlite_migration_connection(&format!(
            "sqlite://{}",
            path.display()
        ))
        .unwrap();
        other
            .batch_execute("SELECT 1 FROM _autumn_fleet_identity")
            .unwrap();
        let err = fleet.delete(&key).await.unwrap_err();
        assert!(matches!(err, FleetError::InUseElsewhere { .. }), "{err}");
        assert_eq!(err.http_status(), http::StatusCode::CONFLICT);
        assert!(path.is_file(), "nothing was unlinked");
        assert_eq!(
            recorder.deleted.load(Ordering::SeqCst),
            0,
            "on_delete (say, replica cleanup) must not run for a refused delete"
        );
        fleet.open(&key).await.expect("the key is not stranded");
        fleet.close(&key).await;
        drop(other);
        assert!(fleet.delete(&key).await.unwrap());
        assert!(!path.exists());
        assert_eq!(recorder.deleted.load(Ordering::SeqCst), 1);

        // Nothing on disk: nothing deleted, so no hook either.
        assert!(!fleet.delete(&key).await.unwrap());
        assert_eq!(recorder.deleted.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn delete_removes_the_files_and_the_emptied_bucket() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Tenant, |_| {});
        let key = fleet.key_for("acme").unwrap();
        let db = fleet.provision(&key).await.unwrap();
        let path = db.path().to_path_buf();
        let bucket = path.parent().unwrap().to_path_buf();
        drop(db);
        assert!(fleet.delete(&key).await.unwrap());
        assert!(!path.exists());
        for sidecar in crate::fleet_layout::sidecar_paths(&path) {
            assert!(!sidecar.exists(), "{}", sidecar.display());
        }
        assert!(!bucket.exists(), "the emptied bucket directory is removed");
        assert!(fleet.root().is_dir(), "the root is kept");
        assert!(matches!(
            fleet.open(&key).await,
            Err(FleetError::NotFound { .. })
        ));
        assert!(
            !fleet.delete(&key).await.unwrap(),
            "deleting nothing reports false"
        );
        assert_eq!(fleet.stats().deleted_total, 1);
    }

    #[tokio::test]
    async fn a_file_copied_to_another_key_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Tenant, |_| {});
        let acme = fleet.key_for("acme").unwrap();
        let globex = fleet.key_for("globex").unwrap();
        fleet.provision(&acme).await.unwrap();
        fleet.close(&acme).await;
        let target = fleet.path_of(&globex);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::copy(fleet.path_of(&acme), &target).unwrap();
        let err = fleet.open(&globex).await.unwrap_err();
        assert!(
            matches!(&err, FleetError::Misplaced { name, found } if name == "tenant:globex" && found == "tenant:acme"),
            "{err}"
        );
        assert_eq!(err.http_status(), http::StatusCode::INTERNAL_SERVER_ERROR);

        // `AUTUMN_MIGRATE=1` must not migrate it either: the copy is acme's
        // data, and its migrations are not globex's to run.
        let report = fleet.migrate_all(2).await.unwrap();
        assert_eq!(report.databases, 2);
        assert_eq!(report.failed.len(), 1, "{:?}", report.failed);
        let (failed_key, failure) = &report.failed[0];
        assert_eq!(failed_key, &globex);
        assert!(
            matches!(failure, FleetError::Misplaced { found, .. } if found == "tenant:acme"),
            "{failure}"
        );
    }

    /// A database path, or a bucket directory on the way to it, that is a
    /// symlink must not lead the fleet out of its root: tenant `control`
    /// linked to the control database would otherwise be claimed and
    /// migrated as a tenant.
    #[cfg(unix)]
    #[tokio::test]
    async fn symlinks_out_of_the_root_are_refused() {
        use diesel::connection::SimpleConnection as _;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("fleet");
        std::fs::create_dir(&root).unwrap();
        let fleet = fleet(&root, FleetMode::Tenant, |c| {
            c.create_on_demand = Some(true);
        });
        let outside = tmp.path().join("control.db");
        crate::db::establish_sqlite_migration_connection(&DatabaseFleet::url_of(&outside))
            .unwrap()
            .batch_execute("CREATE TABLE control_only (id INTEGER)")
            .unwrap();

        // The database file itself is a symlink.
        let control = fleet.key_for("control").unwrap();
        let path = fleet.path_of(&control);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&outside, &path).unwrap();
        let err = fleet.open(&control).await.unwrap_err();
        assert!(err.to_string().contains("symlink"), "{err}");

        // A bucket directory is a symlink that leaves the root.
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        let acme = fleet.key_for("acme").unwrap();
        let acme_path = fleet.path_of(&acme);
        let bucket = acme_path.parent().unwrap();
        if bucket != path.parent().unwrap() {
            std::os::unix::fs::symlink(&elsewhere, bucket).unwrap();
            let err = fleet.open(&acme).await.unwrap_err();
            assert!(err.to_string().contains("outside"), "{err}");
            // Provisioning does its own file work first: it must check too.
            let err = fleet.provision(&acme).await.unwrap_err();
            assert!(err.to_string().contains("outside"), "{err}");
            assert!(
                std::fs::read_dir(&elsewhere).unwrap().next().is_none(),
                "nothing was created outside the root"
            );
        }

        // `AUTUMN_MIGRATE=1` never migrates it: enumeration skips a symlink,
        // and the containment check refuses one that slips through.
        fleet.migrate_all(2).await.unwrap();
        let mut conn =
            crate::db::establish_sqlite_migration_connection(&DatabaseFleet::url_of(&outside))
                .unwrap();
        assert!(
            conn.batch_execute(&format!("SELECT 1 FROM {IDENTITY_TABLE}"))
                .is_err(),
            "the control database was never claimed"
        );
    }

    /// A missing directory under a symlinked one: the nearest directory that
    /// exists decides, before `create_dir_all` follows the link.
    #[cfg(unix)]
    #[tokio::test]
    #[allow(
        clippy::literal_string_with_formatting_args,
        reason = "fleet path templates use {placeholders}"
    )]
    async fn a_missing_directory_under_a_symlink_out_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("fleet");
        std::fs::create_dir(&root).unwrap();
        let fleet = fleet(&root, FleetMode::Tenant, |c| {
            c.path = Some("{bucket}/new/{tenant}.db".to_owned());
            c.create_on_demand = Some(true);
        });
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        let acme = fleet.key_for("acme").unwrap();
        let bucket = fleet
            .path_of(&acme)
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        std::os::unix::fs::symlink(&elsewhere, &bucket).unwrap();
        let err = fleet.provision(&acme).await.unwrap_err();
        assert!(err.to_string().contains("outside"), "{err}");
        let err = fleet.open(&acme).await.unwrap_err();
        assert!(err.to_string().contains("outside"), "{err}");
        assert!(
            std::fs::read_dir(&elsewhere).unwrap().next().is_none(),
            "nothing was created outside the root"
        );
    }

    /// `AUTUMN_MIGRATE=1` lists the fleet, then migrates each file. A file
    /// deleted in between (by a live process sharing the root) must stay
    /// deleted: migrating it must not recreate an empty tenant database.
    #[cfg(unix)]
    #[tokio::test]
    async fn migrating_a_database_deleted_after_listing_does_not_recreate_it() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Tenant, |_| {});
        let acme = fleet.key_for("acme").unwrap();
        fleet.provision(&acme).await.unwrap();
        fleet.close(&acme).await;
        let path = fleet.path_of(&acme);
        std::fs::remove_file(&path).unwrap();
        let applied = migrate_existing_blocking(&fleet, &acme).expect("a vanished file is skipped");
        assert_eq!(applied, 0);
        assert!(!path.exists(), "the deleted database stayed deleted");
    }

    #[test]
    fn a_staging_name_another_process_took_is_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("acme.db");
        // Another container with the same PID and counter got there first.
        let next = STAGING_SEQ.load(Ordering::Relaxed);
        let taken: Vec<PathBuf> = (next..next + 8)
            .map(|n| {
                tmp.path()
                    .join(format!(".acme.db.creating-{}-{n}", std::process::id()))
            })
            .collect();
        for dir in &taken {
            std::fs::create_dir(dir).unwrap();
            std::fs::write(dir.join("acme.db"), b"theirs").unwrap();
        }
        let staging = staging_path(&path, "creating").unwrap();
        assert!(
            !taken.iter().any(|dir| staging.starts_with(dir)),
            "{} reused a taken name",
            staging.display()
        );
        assert!(staging.parent().unwrap().is_dir(), "the name is reserved");
        discard_staging(&staging);
        assert!(
            !staging.parent().unwrap().exists(),
            "discard frees the name"
        );
        for dir in &taken {
            assert_eq!(std::fs::read(dir.join("acme.db")).unwrap(), b"theirs");
        }
    }

    #[tokio::test]
    async fn backup_writes_a_consistent_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let key = FleetDbKey::Slot(9);
        let db = fleet.open(&key).await.unwrap();
        {
            use diesel_async::RunQueryDsl as _;
            let mut conn = db.pool().get().await.unwrap();
            diesel::sql_query("CREATE TABLE notes (body TEXT NOT NULL)")
                .execute(&mut *conn)
                .await
                .unwrap();
            diesel::sql_query("INSERT INTO notes (body) VALUES ('kept')")
                .execute(&mut *conn)
                .await
                .unwrap();
        }
        let dest = tmp.path().join("backup.db");
        fleet.backup(&key, &dest).await.unwrap();
        assert!(matches!(
            fleet.backup(&key, &dest).await,
            Err(FleetError::AlreadyExists { .. })
        ));
        let rows = read_bodies(&dest);
        assert_eq!(rows, vec!["kept".to_owned()]);
    }

    fn read_bodies(path: &Path) -> Vec<String> {
        use diesel::RunQueryDsl as _;
        #[derive(diesel::QueryableByName)]
        struct Body {
            #[diesel(sql_type = diesel::sql_types::Text)]
            body: String,
        }
        let mut conn = crate::db::establish_sqlite_migration_connection(&format!(
            "sqlite://{}",
            path.display()
        ))
        .unwrap();
        diesel::sql_query("SELECT body FROM notes")
            .load::<Body>(&mut conn)
            .unwrap()
            .into_iter()
            .map(|b| b.body)
            .collect()
    }

    #[tokio::test]
    async fn list_finds_databases_and_ignores_everything_else() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Tenant, |_| {});
        let mut keys = Vec::new();
        for id in ["acme", "globex", "initech"] {
            let key = fleet.key_for(id).unwrap();
            fleet.provision(&key).await.unwrap();
            keys.push(key);
        }
        keys.sort();
        // Strays: a sidecar-looking file, a foreign file, a tenant file in the
        // wrong bucket, and a symlink.
        let acme = fleet.path_of(&keys[0]);
        std::fs::write(acme.with_file_name("notes.txt"), b"x").unwrap();
        std::fs::write(fleet.root().join("README"), b"x").unwrap();
        let wrong_bucket = fleet
            .root()
            .join(format!("{:03}", (keys[0].bucket() + 1) % 256));
        std::fs::create_dir_all(&wrong_bucket).unwrap();
        std::fs::write(wrong_bucket.join("acme.db"), b"").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&acme, acme.with_file_name("zzz.db")).unwrap();
        assert_eq!(fleet.list().await.unwrap(), keys);

        let names = fleet
            .each(2, |db| async move { Ok(db.key().name()) })
            .await
            .unwrap();
        let names: Vec<String> = names.into_iter().map(|(_, r)| r.unwrap()).collect();
        assert_eq!(
            names,
            vec!["tenant:acme", "tenant:globex", "tenant:initech"]
        );
    }

    #[tokio::test]
    async fn migrate_applies_the_pending_migrations_of_one_database() {
        let tmp = tempfile::tempdir().unwrap();
        let bare = DatabaseFleet::builder(config(tmp.path(), FleetMode::Slot))
            .build()
            .unwrap();
        bare.open(&FleetDbKey::Slot(5)).await.unwrap();
        bare.open(&FleetDbKey::Slot(6)).await.unwrap();
        bare.close_all().await;

        let fleet = DatabaseFleet::builder(config(tmp.path(), FleetMode::Slot))
            .migrations(
                "version-history",
                crate::version_history::VERSION_HISTORY_MIGRATIONS,
            )
            .apply_migrations_on_open(false)
            .build()
            .unwrap();
        assert_eq!(fleet.migrate(&FleetDbKey::Slot(5)).await.unwrap(), 3);
        assert_eq!(fleet.migrate(&FleetDbKey::Slot(5)).await.unwrap(), 0);
        // Only the key that was named.
        assert!(matches!(
            fleet.open(&FleetDbKey::Slot(6)).await,
            Err(FleetError::PendingMigrations { count: 3, .. })
        ));
        // A missing database is reported and not created.
        let missing = FleetDbKey::Slot(7);
        assert!(matches!(
            fleet.migrate(&missing).await,
            Err(FleetError::NotFound { .. })
        ));
        assert!(!fleet.exists(&missing));
    }

    #[tokio::test]
    async fn a_file_migration_source_and_the_framework_sets_migrate_a_database() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("migrations/20260101000000_create_notes");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("up.sql"), "CREATE TABLE notes (id INTEGER);").unwrap();
        std::fs::write(dir.join("down.sql"), "DROP TABLE notes;").unwrap();
        let root = tmp.path().join("fleet");
        let source =
            diesel_migrations::FileBasedMigrations::from_path(tmp.path().join("migrations"))
                .unwrap();
        let fleet = DatabaseFleet::builder(config(&root, FleetMode::Slot))
            .migration_source("app", source)
            .framework_migrations()
            .build()
            .unwrap();
        let db = fleet.open(&FleetDbKey::Slot(1)).await.unwrap();
        for table in [
            "notes",
            "_autumn_version_history",
            "autumn_repository_commit_hooks",
            "_autumn_derivations",
        ] {
            assert!(table_exists(&db, table).await, "{table}");
        }
        assert!(
            !table_exists(&db, "api_tokens").await,
            "the control-plane set stays on the control database"
        );
        fleet.close_all().await;

        assert!(fleet.remapped_migrations().is_empty());

        // Boot's embedded framework sets find nothing left to apply.
        let all = Arc::new(vec![
            (
                "repository-commit-hooks",
                crate::repository_commit_hooks::REPOSITORY_COMMIT_HOOK_MIGRATIONS,
            ),
            (
                "version-history",
                crate::version_history::VERSION_HISTORY_MIGRATIONS,
            ),
            ("derivations", crate::derivation::DERIVATION_MIGRATIONS),
        ]);
        let boot = DatabaseFleet::from_parts(
            &config(&root, FleetMode::Slot),
            5,
            FleetMigrations::new(&all, |_| true, false),
            Duration::ZERO,
        )
        .unwrap();
        boot.open(&FleetDbKey::Slot(1)).await.unwrap();
    }

    #[tokio::test]
    async fn migrate_keys_visits_only_the_given_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let bare = DatabaseFleet::builder(config(tmp.path(), FleetMode::Slot))
            .build()
            .unwrap();
        for slot in [1, 2] {
            bare.open(&FleetDbKey::Slot(slot)).await.unwrap();
        }
        bare.close_all().await;
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let report = fleet
            .migrate_keys(vec![FleetDbKey::Slot(1), FleetDbKey::Slot(3)], 2)
            .await;
        assert_eq!((report.databases, report.applied), (2, 3));
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        assert!(
            !fleet.exists(&FleetDbKey::Slot(3)),
            "a missing key is not created"
        );
        // Slot 2 was not given: it still has pending migrations.
        let pending = DatabaseFleet::builder(config(tmp.path(), FleetMode::Slot))
            .migrations(
                "version-history",
                crate::version_history::VERSION_HISTORY_MIGRATIONS,
            )
            .apply_migrations_on_open(false)
            .build()
            .unwrap();
        assert!(matches!(
            pending.open(&FleetDbKey::Slot(2)).await,
            Err(FleetError::PendingMigrations { .. })
        ));
    }

    #[tokio::test]
    async fn refuse_unknown_migrations_checks_the_migrated_file() {
        let tmp = tempfile::tempdir().unwrap();
        // A set the next fleet does not have, as a plugin's.
        let full = DatabaseFleet::builder(config(tmp.path(), FleetMode::Slot))
            .migrations(
                "version-history",
                crate::version_history::VERSION_HISTORY_MIGRATIONS,
            )
            .migrations("derivations", crate::derivation::DERIVATION_MIGRATIONS)
            .build()
            .unwrap();
        full.open(&FleetDbKey::Slot(1)).await.unwrap();
        full.close_all().await;

        let partial = |refuse| {
            DatabaseFleet::builder(config(tmp.path(), FleetMode::Slot))
                .migrations(
                    "version-history",
                    crate::version_history::VERSION_HISTORY_MIGRATIONS,
                )
                .apply_migrations_on_open(false)
                .refuse_unknown_migrations(refuse)
                .build()
                .unwrap()
        };
        let key = FleetDbKey::Slot(1);
        match partial(true).migrate(&key).await {
            Err(FleetError::Migration { detail, .. }) => {
                assert!(detail.contains("none of the sets"), "{detail}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        let report = partial(true).migrate_keys(vec![key.clone()], 1).await;
        assert_eq!(report.failed.len(), 1, "{:?}", report.failed);
        // The default does not check.
        assert_eq!(partial(false).migrate(&key).await.unwrap(), 0);
    }

    /// While pinned, a database cannot be removed, so opening it cannot
    /// create a new file in its place; a missing file is `None`.
    #[cfg(windows)]
    #[test]
    fn a_pinned_database_cannot_be_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("t.db");
        assert!(pin_existing(&path).unwrap().is_none());
        std::fs::write(&path, b"").unwrap();
        let pin = pin_existing(&path).unwrap().expect("the file exists");
        assert!(std::fs::remove_file(&path).is_err());
        assert!(std::fs::rename(&path, tmp.path().join("moved.db")).is_err());
        drop(pin);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn known_versions_are_the_recorded_versions_of_every_set() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let mut expected: Vec<String> =
            crate::migrate::migration_versions_and_names::<diesel::sqlite::Sqlite, _>(
                &crate::version_history::VERSION_HISTORY_MIGRATIONS,
            )
            .unwrap()
            .into_iter()
            .map(|(version, _)| version)
            .collect();
        expected.sort();
        assert_eq!(fleet.known_migration_versions(), expected);
    }

    #[test]
    fn a_control_database_inside_the_root_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(&tmp.path().join("fleet"), FleetMode::Slot, |_| {});
        let inside = format!("sqlite://{}", fleet.root().join("control.db").display());
        let outside = format!("sqlite://{}", tmp.path().join("control.db").display());
        let err = fleet.check_control_outside_root(&[&inside]).unwrap_err();
        assert!(err.contains("control database"), "{err}");
        fleet.check_control_outside_root(&[&outside]).unwrap();
    }

    /// A control database under a directory not created yet, reached
    /// through a symlinked root, is judged by where it would be created.
    #[cfg(unix)]
    #[test]
    fn a_control_database_under_a_missing_directory_of_the_root_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let alias = tmp.path().join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let fleet = fleet(&alias, FleetMode::Slot, |_| {});
        let control = format!("sqlite://{}", real.join("000/sub/control.db").display());
        let err = fleet.check_control_outside_root(&[&control]).unwrap_err();
        assert!(err.contains("control database"), "{err}");
        assert_eq!(
            canonical_target(Path::new("missing-dir/control.db")),
            Some(
                std::fs::canonicalize(".")
                    .unwrap()
                    .join("missing-dir/control.db")
            )
        );
    }

    #[test]
    fn a_shared_version_is_reported_as_remapped() {
        let tmp = tempfile::tempdir().unwrap();
        // The version of the framework's derivations migration.
        let dir = tmp.path().join("migrations/20260907101530_create_notes");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("up.sql"), "CREATE TABLE notes (id INTEGER);").unwrap();
        std::fs::write(dir.join("down.sql"), "DROP TABLE notes;").unwrap();
        let source =
            diesel_migrations::FileBasedMigrations::from_path(tmp.path().join("migrations"))
                .unwrap();
        let fleet = DatabaseFleet::builder(config(&tmp.path().join("fleet"), FleetMode::Slot))
            .migration_source("app", source)
            .framework_migrations()
            .build()
            .unwrap();
        assert_eq!(
            fleet.remapped_migrations(),
            vec!["20260907101530_create_notes".to_owned()]
        );
    }

    #[tokio::test]
    async fn inspect_reads_the_identity_and_version_and_writes_nothing() {
        use diesel::connection::SimpleConnection as _;
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Tenant, |_| {});
        let acme = fleet.key_for("acme").unwrap();
        fleet.provision(&acme).await.unwrap();
        fleet.close_all().await;
        let info = fleet.inspect(&acme).await.unwrap();
        assert_eq!(info.identity.as_deref(), Some("tenant:acme"));
        let mut expected: Vec<String> =
            crate::migrate::migration_versions_and_names::<diesel::sqlite::Sqlite, _>(
                &crate::version_history::VERSION_HISTORY_MIGRATIONS,
            )
            .unwrap()
            .into_iter()
            .map(|(version, _)| version)
            .collect();
        expected.sort();
        assert_eq!(info.applied_migrations, expected);

        // A file the fleet never opened: nothing to read, and nothing added.
        let globex = fleet.key_for("globex").unwrap();
        let path = fleet.path_of(&globex);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        crate::db::establish_sqlite_migration_connection(&DatabaseFleet::url_of(&path))
            .unwrap()
            .batch_execute("CREATE TABLE notes (id INTEGER)")
            .unwrap();
        let info = fleet.inspect(&globex).await.unwrap();
        assert_eq!(info.identity, None);
        assert!(info.applied_migrations.is_empty(), "{info:?}");
        let mut conn =
            crate::db::establish_sqlite_migration_connection(&DatabaseFleet::url_of(&path))
                .unwrap();
        assert!(
            conn.batch_execute(&format!("SELECT 1 FROM {IDENTITY_TABLE}"))
                .is_err(),
            "inspect did not claim the file"
        );

        let missing = fleet.key_for("initech").unwrap();
        assert!(matches!(
            fleet.inspect(&missing).await,
            Err(FleetError::NotFound { .. })
        ));
        assert!(!fleet.exists(&missing));
    }

    #[tokio::test]
    async fn an_existing_database_with_pending_migrations_is_refused_until_migrated() {
        let tmp = tempfile::tempdir().unwrap();
        // Create a database with no migrations at all.
        let bare = DatabaseFleet::builder(config(tmp.path(), FleetMode::Slot))
            .build()
            .unwrap();
        bare.open(&FleetDbKey::Slot(5)).await.unwrap();
        bare.close_all().await;

        let report_only = DatabaseFleet::builder(config(tmp.path(), FleetMode::Slot))
            .migrations(
                "version-history",
                crate::version_history::VERSION_HISTORY_MIGRATIONS,
            )
            .apply_migrations_on_open(false)
            .build()
            .unwrap();
        let err = report_only.open(&FleetDbKey::Slot(5)).await.unwrap_err();
        assert!(
            matches!(err, FleetError::PendingMigrations { count: 3, .. }),
            "{err}"
        );
        assert_eq!(err.http_status(), http::StatusCode::SERVICE_UNAVAILABLE);
        // A new database is always migrated: there is nothing to break.
        report_only.open(&FleetDbKey::Slot(6)).await.unwrap();

        let report = report_only.migrate_all(4).await.unwrap();
        assert_eq!(report.databases, 2);
        assert_eq!(report.applied, 3);
        assert!(report.failed.is_empty());
        let db = report_only.open(&FleetDbKey::Slot(5)).await.unwrap();
        assert!(table_exists(&db, "_autumn_version_history").await);
    }

    /// A worker that, once told to stop, finishes one last step that needs
    /// the pool (an ack) after `release` fires.
    struct SettlingWorker {
        release: Arc<Notify>,
        acked: Arc<AtomicBool>,
        hang: bool,
    }

    impl FleetLifecycle for SettlingWorker {
        fn on_open(&self, db: &FleetDatabase) -> Result<(), String> {
            let stop = CancellationToken::new();
            let (pool, release, acked, hang) = (
                db.pool().clone(),
                Arc::clone(&self.release),
                Arc::clone(&self.acked),
                self.hang,
            );
            let task_stop = stop.clone();
            let handle = tokio::spawn(async move {
                task_stop.cancelled().await;
                if hang {
                    std::future::pending::<()>().await;
                }
                release.notified().await;
                acked.store(pool.get().await.is_ok(), Ordering::SeqCst);
            });
            db.register_worker(stop, handle);
            Ok(())
        }
    }

    fn settling(hang: bool) -> (Arc<SettlingWorker>, Arc<Notify>, Arc<AtomicBool>) {
        let worker = Arc::new(SettlingWorker {
            release: Arc::new(Notify::new()),
            acked: Arc::new(AtomicBool::new(false)),
            hang,
        });
        (
            worker.clone(),
            Arc::clone(&worker.release),
            Arc::clone(&worker.acked),
        )
    }

    #[tokio::test]
    async fn close_lets_a_worker_settle_before_the_pool_closes() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let (worker, release, acked) = settling(false);
        fleet.add_lifecycle(worker);
        let key = FleetDbKey::Slot(21);
        let db = fleet.open(&key).await.unwrap();
        let closing = {
            let (fleet, key) = (fleet.clone(), key.clone());
            tokio::spawn(async move { fleet.close(&key).await })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !db.pool().is_closed(),
            "the pool stays open while the worker runs"
        );
        assert!(!db.closed().is_cancelled());
        release.notify_one();
        closing.await.unwrap();
        assert!(
            acked.load(Ordering::SeqCst),
            "the worker got a connection to settle"
        );
        assert!(db.pool().is_closed());
    }

    /// Issue 3231 holds for a sweeper's close: the worker settles first.
    #[tokio::test]
    async fn close_if_unused_lets_a_worker_settle_before_the_pool_closes() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let key = FleetDbKey::Slot(22);
        drop(fleet.open(&key).await.unwrap());
        fleet.close(&key).await;
        let (worker, release, acked) = settling(false);
        fleet.add_lifecycle(worker);
        let opened = fleet.open_background(&key).await.unwrap().expect("room");
        let db = opened.db.clone();
        drop(opened);
        let pool = db.pool().clone();
        drop(db);
        assert!(fleet.close_if_unused(&key));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !pool.is_closed(),
            "the pool stays open while the worker runs"
        );
        release.notify_one();
        settle(&fleet).await;
        assert!(acked.load(Ordering::SeqCst), "the worker settled first");
        assert!(pool.is_closed());
    }

    /// Counts the restores the fleet asks for, and produces nothing.
    #[derive(Default)]
    struct CountingRestore(AtomicUsize);

    impl FleetLifecycle for CountingRestore {
        fn restore_missing(&self, _key: &FleetDbKey, _path: &Path) -> Result<bool, String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(false)
        }
    }

    #[tokio::test]
    async fn a_background_open_never_restores_a_missing_database() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |c| {
            c.create_on_demand = Some(false);
        });
        let restores = Arc::new(CountingRestore::default());
        fleet.add_lifecycle(restores.clone());
        let key = FleetDbKey::Slot(23);
        let error = fleet.open_background(&key).await.unwrap_err();
        assert!(matches!(error, FleetError::NotFound { .. }), "{error}");
        assert_eq!(
            restores.0.load(Ordering::SeqCst),
            0,
            "a deleted tenant stays deleted"
        );
        let _ = fleet.open(&key).await.unwrap_err();
        assert_eq!(
            restores.0.load(Ordering::SeqCst),
            1,
            "a request may restore it"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn close_gives_up_on_a_worker_that_never_settles() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let (worker, _release, acked) = settling(true);
        fleet.add_lifecycle(worker);
        let key = FleetDbKey::Slot(22);
        let db = fleet.open(&key).await.unwrap();
        let began = tokio::time::Instant::now();
        fleet.close(&key).await;
        assert!(
            db.pool().is_closed(),
            "a stuck worker cannot block the close"
        );
        assert!(began.elapsed() >= WORKER_QUIESCE_TIMEOUT);
        assert!(!acked.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn delete_stops_workers_at_once_instead_of_letting_them_settle() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let (worker, _release, acked) = settling(false);
        fleet.add_lifecycle(worker);
        let key = FleetDbKey::Slot(29);
        let db = fleet.open(&key).await.unwrap();
        let deleted = tokio::time::timeout(Duration::from_secs(5), fleet.delete(&key))
            .await
            .expect("a delete does not wait for a worker");
        assert!(deleted.unwrap());
        assert!(db.pool().is_closed());
        assert!(!acked.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn a_worker_registered_after_the_stop_began_is_stopped_at_once() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let key = FleetDbKey::Slot(30);
        let db = fleet.open(&key).await.unwrap();
        fleet.close(&key).await;
        let stop = CancellationToken::new();
        let task_stop = stop.clone();
        let handle = tokio::spawn(async move { task_stop.cancelled().await });
        let probe = handle.abort_handle();
        db.register_worker(stop.clone(), handle);
        assert!(stop.is_cancelled());
        tokio::task::yield_now().await;
        assert!(probe.is_finished());
    }

    #[tokio::test(start_paused = true)]
    async fn close_all_pays_the_worker_timeout_once() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let (worker, _release, _acked) = settling(true);
        fleet.add_lifecycle(worker);
        for slot in 25..28 {
            fleet.open(&FleetDbKey::Slot(slot)).await.unwrap();
        }
        let began = tokio::time::Instant::now();
        fleet.close_all().await;
        assert!(
            began.elapsed() < WORKER_QUIESCE_TIMEOUT * 2,
            "three stuck workers cost one timeout, not three"
        );
    }

    #[tokio::test]
    async fn close_all_waits_for_workers_of_every_database_together() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let (worker, release, acked) = settling(false);
        fleet.add_lifecycle(worker);
        let first = fleet.open(&FleetDbKey::Slot(23)).await.unwrap();
        let second = fleet.open(&FleetDbKey::Slot(24)).await.unwrap();
        let closing = {
            let fleet = fleet.clone();
            tokio::spawn(async move { fleet.close_all().await })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!first.pool().is_closed() && !second.pool().is_closed());
        // One permit per worker: `notify_waiters` would be lost to a late one.
        release.notify_one();
        release.notify_one();
        closing.await.unwrap();
        assert!(acked.load(Ordering::SeqCst));
        assert!(first.pool().is_closed() && second.pool().is_closed());
    }

    #[derive(Default)]
    struct Recorder {
        opened: AtomicUsize,
        closed: AtomicUsize,
        deleted: AtomicUsize,
    }

    impl FleetLifecycle for Recorder {
        fn on_open(&self, _db: &FleetDatabase) -> Result<(), String> {
            self.opened.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn on_close(&self, db: &FleetDatabase) {
            assert!(db.path().is_file());
            assert!(db.pool().is_closed());
            assert_eq!(db.pool().status().size, 0);
            self.closed.fetch_add(1, Ordering::SeqCst);
        }
        fn on_delete(&self, _key: &FleetDbKey) {
            self.deleted.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct Refuse;
    impl FleetLifecycle for Refuse {
        fn on_open(&self, _db: &FleetDatabase) -> Result<(), String> {
            Err("replicator unavailable".to_owned())
        }
    }

    #[tokio::test]
    async fn lifecycle_hooks_follow_open_close_and_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let recorder = Arc::new(Recorder::default());
        fleet.add_lifecycle(recorder.clone());
        let key = FleetDbKey::Slot(11);
        let db = fleet.open(&key).await.unwrap();
        let held = db.pool().get().await.unwrap();
        let closing = {
            let fleet = fleet.clone();
            let key = key.clone();
            tokio::spawn(async move { fleet.close(&key).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            recorder.closed.load(Ordering::SeqCst),
            0,
            "on_close waits for the in-flight connection"
        );
        assert!(db.closed().is_cancelled());
        assert!(
            db.pool().get().await.is_err(),
            "a closed pool hands out nothing new"
        );
        drop(held);
        closing.await.unwrap();
        assert_eq!(recorder.closed.load(Ordering::SeqCst), 1);

        fleet.open(&key).await.unwrap();
        assert!(fleet.delete(&key).await.unwrap());
        assert_eq!(recorder.opened.load(Ordering::SeqCst), 2);
        assert_eq!(recorder.closed.load(Ordering::SeqCst), 2);
        assert_eq!(recorder.deleted.load(Ordering::SeqCst), 1);

        fleet.add_lifecycle(Arc::new(Refuse));
        let err = fleet.open(&FleetDbKey::Slot(12)).await.unwrap_err();
        assert!(matches!(err, FleetError::Unavailable { .. }), "{err}");
        assert_eq!(fleet.stats().open_failures_total, 1);
    }

    #[tokio::test]
    async fn opening_during_a_delete_is_refused_and_after_it_starts_clean() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let key = FleetDbKey::Slot(13);
        let db = fleet.open(&key).await.unwrap();
        let held = db.pool().get().await.unwrap();
        let deleting = {
            let fleet = fleet.clone();
            let key = key.clone();
            tokio::spawn(async move { fleet.delete(&key).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(matches!(
            fleet.open(&key).await,
            Err(FleetError::Deleting { .. })
        ));
        drop(held);
        assert!(deleting.await.unwrap().unwrap());
        // A slot fleet creates on demand: the slot comes back empty.
        let fresh = fleet.open(&key).await.unwrap();
        assert!(!table_exists(&fresh, "notes").await);
    }

    // ── Cancellation safety (#3229) ──────────────────────────────────────

    /// Hook whose `on_open` blocks for a while, like a slow replicator start.
    struct SlowOpen {
        delay: Duration,
        started: AtomicUsize,
        inner: Recorder,
    }

    impl SlowOpen {
        fn new(delay: Duration) -> Arc<Self> {
            Arc::new(Self {
                delay,
                started: AtomicUsize::new(0),
                inner: Recorder::default(),
            })
        }
    }

    impl FleetLifecycle for SlowOpen {
        fn on_open(&self, db: &FleetDatabase) -> Result<(), String> {
            self.started.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(self.delay);
            self.inner.on_open(db)
        }
        fn on_close(&self, db: &FleetDatabase) {
            self.inner.on_close(db);
        }
    }

    /// Cancel `call` after `after`, as a request timeout does.
    async fn cancelled<T>(after: Duration, call: impl Future<Output = T>) {
        assert!(
            tokio::time::timeout(after, call).await.is_err(),
            "the call finished before the timeout"
        );
    }

    /// Cancel `call` once a new run of the slow `on_open` hook has started.
    async fn cancelled_in_hook<T>(hook: &SlowOpen, call: impl Future<Output = T>) {
        let before = hook.started.load(Ordering::SeqCst);
        let started = async {
            while hook.started.load(Ordering::SeqCst) == before {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        };
        tokio::select! {
            _ = call => panic!("the call finished before the hook started"),
            () = started => {}
        }
    }

    /// Wait until `key` has a file, as a publish that outlived its caller does.
    async fn published(fleet: &DatabaseFleet, key: &FleetDbKey) {
        for _ in 0..1000 {
            if fleet.exists(key) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("the file was never published");
    }

    #[tokio::test]
    async fn a_cancelled_delete_still_finishes_and_frees_the_key() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let key = FleetDbKey::Slot(21);
        let db = fleet.open(&key).await.unwrap();
        let held = db.pool().get().await.unwrap();
        cancelled(Duration::from_millis(60), fleet.delete(&key)).await;
        drop(held);
        drop(db);
        settle(&fleet).await;
        assert!(!fleet.exists(&key), "the delete ran to the end");
        assert_eq!(fleet.stats().deleted_total, 1);
        // A slot fleet creates on demand: the key opens again, empty.
        let fresh = tokio::time::timeout(NOT_STUCK, fleet.open(&key))
            .await
            .expect("open is not stuck on a stranded drain")
            .unwrap();
        assert!(!table_exists(&fresh, "notes").await);
        drop(fresh);
        tokio::time::timeout(NOT_STUCK, fleet.close_all())
            .await
            .expect("close_all does not wait on a stranded drain");
    }

    #[tokio::test]
    async fn a_cancelled_close_still_finishes_and_frees_the_key() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let recorder = Arc::new(Recorder::default());
        fleet.add_lifecycle(recorder.clone());
        let key = FleetDbKey::Slot(22);
        let db = fleet.open(&key).await.unwrap();
        let held = db.pool().get().await.unwrap();
        cancelled(Duration::from_millis(60), fleet.close(&key)).await;
        drop(held);
        drop(db);
        settle(&fleet).await;
        assert_eq!(recorder.closed.load(Ordering::SeqCst), 1);
        tokio::time::timeout(NOT_STUCK, fleet.open(&key))
            .await
            .expect("open is not stuck on a stranded drain")
            .unwrap();
        tokio::time::timeout(NOT_STUCK, fleet.close_all())
            .await
            .expect("close_all returns");
    }

    #[tokio::test]
    async fn a_cancelled_first_open_leaves_nothing_untracked() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let hook = SlowOpen::new(Duration::from_millis(400));
        fleet.add_lifecycle(hook.clone());
        let key = FleetDbKey::Slot(23);
        cancelled_in_hook(&hook, fleet.open(&key)).await;
        // The delete waits for the open that is still running, then drains it.
        assert!(fleet.delete(&key).await.unwrap());
        settle(&fleet).await;
        let (opened, closed) = (
            hook.inner.opened.load(Ordering::SeqCst),
            hook.inner.closed.load(Ordering::SeqCst),
        );
        assert_eq!(opened, 1, "the abandoned open ran its hook");
        assert_eq!(opened, closed, "every on_open has its on_close");
        assert!(!fleet.exists(&key));
    }

    /// Fails every open after a delay, and counts the attempts.
    struct SlowFail(AtomicUsize);

    impl FleetLifecycle for SlowFail {
        fn on_open(&self, _db: &FleetDatabase) -> Result<(), String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(150));
            Err("hook failed".to_owned())
        }
    }

    #[tokio::test]
    async fn many_cancelled_callers_share_one_opener() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let hook = Arc::new(SlowFail(AtomicUsize::new(0)));
        fleet.add_lifecycle(hook.clone());
        let key = FleetDbKey::Slot(27);
        let callers = futures::future::join_all((0..5).map(|_| fleet.open(&key)));
        // Drop all five callers while the one opener runs its hook.
        let _ = tokio::time::timeout(Duration::from_millis(60), callers).await;
        // The opener fails on its own task; wait for its entry to go.
        for _ in 0..1000 {
            if fleet.stats().open_failures_total > 0 && fleet.open_keys().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(hook.0.load(Ordering::SeqCst), 1, "one opener, not five");
        assert_eq!(fleet.stats().open_failures_total, 1);
        assert!(fleet.open_keys().is_empty(), "the failed entry is gone");
    }

    #[tokio::test]
    async fn a_cancelled_open_still_keeps_the_fleet_within_max_open() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |c| c.max_open = 1);
        let hook = SlowOpen::new(Duration::from_millis(100));
        fleet.add_lifecycle(hook.clone());
        fleet.open(&FleetDbKey::Slot(28)).await.unwrap();
        let second = FleetDbKey::Slot(29);
        // The caller leaves; the opener task still fills the entry.
        cancelled_in_hook(&hook, fleet.open(&second)).await;
        for _ in 0..500 {
            if fleet.stats().open == 1 && fleet.stats().draining == 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the older database stayed open: {:?}", fleet.open_keys());
    }

    #[tokio::test]
    async fn close_all_waits_for_a_delete_that_just_started() {
        use futures::FutureExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let key = FleetDbKey::Slot(30);
        fleet.open(&key).await.unwrap();
        // Closed, so `close_all` has no open key or drain to wait on.
        fleet.close(&key).await;
        // One poll starts the owned delete; the caller then leaves.
        assert!(fleet.delete(&key).now_or_never().is_none());
        fleet.close_all().await;
        assert_eq!(fleet.stats().deleted_total, 1, "the delete ended first");
        assert!(!fleet.exists(&key));
    }

    #[tokio::test]
    async fn close_waits_for_an_open_that_is_still_running() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let hook = SlowOpen::new(Duration::from_millis(200));
        fleet.add_lifecycle(hook.clone());
        let key = FleetDbKey::Slot(26);
        cancelled_in_hook(&hook, fleet.open(&key)).await;
        fleet.close(&key).await;
        settle(&fleet).await;
        assert_eq!(hook.inner.closed.load(Ordering::SeqCst), 1);
        assert!(fleet.open_keys().is_empty());
    }

    #[tokio::test]
    async fn close_all_waits_for_an_open_that_is_still_running() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let hook = SlowOpen::new(Duration::from_millis(300));
        fleet.add_lifecycle(hook.clone());
        let key = FleetDbKey::Slot(25);
        cancelled_in_hook(&hook, fleet.open(&key)).await;
        fleet.close_all().await;
        assert_eq!(hook.inner.opened.load(Ordering::SeqCst), 1);
        assert_eq!(
            hook.inner.closed.load(Ordering::SeqCst),
            1,
            "the late open was closed, not left running"
        );
        assert!(fleet.open_keys().is_empty());
    }

    #[tokio::test]
    async fn a_cancelled_first_open_is_kept_for_the_next_opener() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let hook = SlowOpen::new(Duration::from_millis(300));
        fleet.add_lifecycle(hook.clone());
        let key = FleetDbKey::Slot(24);
        cancelled_in_hook(&hook, fleet.open(&key)).await;
        fleet.open(&key).await.unwrap();
        assert_eq!(
            hook.inner.opened.load(Ordering::SeqCst),
            1,
            "one open, not one per caller"
        );
        fleet.close_all().await;
        assert_eq!(hook.inner.closed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_cancelled_provision_is_reported_once_by_the_retry() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Tenant, |_| {});
        let key = fleet.key_for("acme").unwrap();
        // A zero timeout drops the call at its first wait, mid-publish.
        cancelled(Duration::ZERO, fleet.provision(&key)).await;
        // Let the abandoned publish finish.
        published(&fleet, &key).await;
        let retry = fleet.provision(&key).await;
        assert!(retry.is_ok(), "the retry sees the creation: {retry:?}");
        let again = fleet.provision(&key).await;
        assert!(
            matches!(again, Err(FleetError::AlreadyExists { .. })),
            "the creation is reported once: {again:?}"
        );
        assert_eq!(fleet.stats().created_total, 1);
    }

    #[tokio::test]
    async fn a_provision_that_finishes_is_not_reported_again() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Tenant, |_| {});
        let key = fleet.key_for("acme").unwrap();
        fleet.provision(&key).await.unwrap();
        assert!(matches!(
            fleet.provision(&key).await,
            Err(FleetError::AlreadyExists { .. })
        ));
        assert_eq!(fleet.stats().created_total, 1);
    }

    #[tokio::test]
    async fn a_provision_cancelled_while_opening_is_reported_by_the_retry() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Tenant, |_| {});
        let hook = SlowOpen::new(Duration::from_millis(200));
        fleet.add_lifecycle(hook.clone());
        let key = fleet.key_for("acme").unwrap();
        // The file is published; the caller leaves during the open.
        cancelled_in_hook(&hook, fleet.provision(&key)).await;
        let retry = fleet.provision(&key).await;
        assert!(retry.is_ok(), "the retry reports the creation: {retry:?}");
        assert!(matches!(
            fleet.provision(&key).await,
            Err(FleetError::AlreadyExists { .. })
        ));
        assert_eq!(hook.inner.opened.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_delete_clears_an_unclaimed_creation() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Tenant, |_| {});
        let key = fleet.key_for("acme").unwrap();
        cancelled(Duration::ZERO, fleet.provision(&key)).await;
        published(&fleet, &key).await;
        assert!(fleet.delete(&key).await.unwrap());
        // Another process creates the file. This fleet did not.
        let other = self::fleet(tmp.path(), FleetMode::Tenant, |_| {});
        other.provision(&key).await.unwrap();
        assert!(
            matches!(
                fleet.provision(&key).await,
                Err(FleetError::AlreadyExists { .. })
            ),
            "a stale marker must not claim another process's creation"
        );
    }

    #[test]
    fn names_resolve_back_to_keys_of_the_fleet_mode() {
        let tmp = tempfile::tempdir().unwrap();
        let tenants = DatabaseFleet::builder(config(tmp.path(), FleetMode::Tenant))
            .build()
            .unwrap();
        let key = tenants.key_for_name("tenant:acme").unwrap();
        assert_eq!(key, tenants.key_for("acme").unwrap());
        assert!(tenants.key_for_name("slot:00001").is_err());
        let slots = DatabaseFleet::builder(config(tmp.path(), FleetMode::Slot))
            .build()
            .unwrap();
        assert_eq!(
            slots.key_for_name("slot:00001").unwrap(),
            FleetDbKey::Slot(1)
        );
        assert!(slots.key_for_name("tenant:acme").is_err());
    }

    #[test]
    fn the_main_file_is_unlinked_last() {
        let tmp = tempfile::tempdir().unwrap();
        let path = plain_database(tmp.path());
        let sidecars = crate::fleet_layout::sidecar_paths(&path);
        let mut seen: Vec<PathBuf> = Vec::new();
        unlink_database_set(&path, "delete database", "acme", true, |file| {
            seen.push(file.to_path_buf());
            Ok(())
        })
        .unwrap();
        assert_eq!(seen.last(), Some(&path));
        assert_eq!(seen.len(), sidecars.len() + 2, "sidecars, lock file, main");
    }

    #[test]
    fn a_database_published_after_the_unlink_keeps_its_wal() {
        let tmp = tempfile::tempdir().unwrap();
        let path = plain_database(tmp.path());
        let wal = tmp.path().join(format!(
            "{}-wal",
            path.file_name().unwrap().to_string_lossy()
        ));
        std::fs::write(&wal, b"old").unwrap();
        // Another process publishes a new database the moment the main file
        // is gone, and commits to its `-wal`.
        unlink_database_set(&path, "delete database", "acme", true, |file| {
            let done = std::fs::remove_file(file);
            if file == path {
                std::fs::write(&path, b"new").unwrap();
                std::fs::write(&wal, b"new frames").unwrap();
            }
            done
        })
        .unwrap();
        assert_eq!(std::fs::read(&wal).unwrap(), b"new frames");
    }

    #[test]
    fn a_sharing_violation_on_a_sidecar_stops_before_the_main_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = plain_database(tmp.path());
        let err = unlink_database_set(&path, "delete database", "acme", true, |file| {
            if file == path {
                std::fs::remove_file(file)
            } else {
                Err(std::io::Error::from_raw_os_error(32))
            }
        });
        if cfg!(windows) {
            assert!(matches!(err, Err(FleetError::InUseElsewhere { .. })));
        } else {
            assert!(err.is_err());
        }
        assert!(path.is_file(), "the main file is still there");
    }

    #[test]
    fn remove_database_files_removes_everything() {
        let tmp = tempfile::tempdir().unwrap();
        let path = plain_database(tmp.path());
        let sidecars = crate::fleet_layout::sidecar_paths(&path);
        for sidecar in &sidecars {
            std::fs::write(sidecar, b"x").unwrap();
        }
        assert!(remove_database_files(&path, tmp.path(), "acme", true).unwrap());
        assert!(!path.exists());
        assert!(sidecars.iter().all(|sidecar| !sidecar.exists()));
    }

    #[test]
    fn the_strays_of_an_absent_path_go_and_a_main_file_is_never_unlinked() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("plain.db");
        let mut seen = Vec::new();
        // The caller holds the path lock, so no other process can publish.
        unlink_database_set(&path, "delete database", "acme", false, |file| {
            seen.push(file.to_path_buf());
            Ok(())
        })
        .unwrap();
        let mut strays = crate::fleet_layout::sidecar_paths(&path).to_vec();
        strays.push(crate::fleet_layout::replicator_lock_path(&path));
        assert_eq!(seen, strays);
    }

    #[tokio::test]
    async fn provision_after_shutdown_creates_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Tenant, |_| {});
        let key = fleet.key_for("acme").unwrap();
        fleet.close_all().await;
        let err = fleet.provision(&key).await.unwrap_err();
        assert!(matches!(err, FleetError::Unavailable { .. }), "{err}");
        assert!(!fleet.path_of(&key).exists());
    }

    #[test]
    fn the_guard_tells_the_work_whether_the_file_was_there() {
        let tmp = tempfile::tempdir().unwrap();
        let path = plain_database(tmp.path());
        let seen = with_exclusive_access(&path, "plain", true, Ok).unwrap();
        assert!(seen);
        let absent = tmp.path().join("missing.db");
        // The file appears after the guard looked. The work still sees what
        // the guard saw.
        let seen = with_exclusive_access(&absent, "missing", true, |present| {
            std::fs::write(&absent, b"new").unwrap();
            Ok(present)
        })
        .unwrap();
        assert!(!seen);
    }

    #[test]
    fn a_database_published_after_an_absent_guard_is_not_reported_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        // The guard saw no file. Another process publishes one before the work.
        let path = plain_database(tmp.path());
        assert!(!remove_database_files(&path, tmp.path(), "acme", false).unwrap());
        assert!(path.is_file(), "the new database stays");
    }

    #[tokio::test]
    async fn a_delete_waits_for_a_running_open_and_no_new_opener_gets_in() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let key = FleetDbKey::Slot(26);
        // An opener is running when the delete takes the key.
        let Next::Open(entry, true) = fleet.claim_entry(&key, false).unwrap() else {
            panic!("expected to make the entry");
        };
        let opener = fleet.spawn_opener(&key, true, true, None, &entry);
        let (taken, held, drain) = fleet.begin_exclusive(&key, true).unwrap().unwrap();
        // The drain is in first: no new opener gets an entry.
        assert!(matches!(
            fleet.claim_entry(&key, false),
            Err(FleetError::Deleting { .. })
        ));
        // The running open ends, and the delete gets its database.
        let db = fleet
            .take_database(held)
            .await
            .expect("the opened database");
        assert!(db.path().is_file());
        opener.await.unwrap().unwrap();
        fleet.drain(&taken, Some(db), true).await;
        fleet.finish_drain(&taken, &drain);
    }

    #[tokio::test]
    async fn an_open_after_close_all_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let key = FleetDbKey::Slot(27);
        fleet.open(&key).await.unwrap();
        fleet.close_all().await;
        assert_eq!(fleet.stats().open, 0);
        let err = fleet.open(&key).await.unwrap_err();
        assert!(matches!(err, FleetError::Unavailable { .. }), "{err}");
        assert_eq!(err.http_status(), http::StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn close_all_refuses_opens_only_after_the_counted_work_ends() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        // A counted operation (a provision, say) is still running.
        let pending = MutationGuard::new(&fleet);
        let closing = {
            let fleet = fleet.clone();
            tokio::spawn(async move { fleet.close_all().await })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!closing.is_finished(), "close_all waits for the work");
        // The work may still open its database: the flag is not set yet.
        fleet.open(&FleetDbKey::Slot(28)).await.unwrap();
        drop(pending);
        closing.await.unwrap();
        let after = fleet.open(&FleetDbKey::Slot(29)).await;
        assert!(matches!(after, Err(FleetError::Unavailable { .. })));
    }

    #[tokio::test]
    async fn no_work_is_admitted_once_shutdown_is_set() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        fleet.close_all().await;
        assert!(fleet.admit_mutation(&FleetDbKey::Slot(30)).is_err());
    }

    #[tokio::test]
    async fn delete_and_restore_are_refused_once_shutdown_is_set() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let key = FleetDbKey::Slot(31);
        fleet.open(&key).await.unwrap();
        fleet.close_all().await;
        let path = fleet.path_of(&key);
        let err = fleet.delete(&key).await.unwrap_err();
        assert!(matches!(err, FleetError::Unavailable { .. }), "{err}");
        assert!(path.is_file(), "a refused delete removes nothing");
        // `restore` checks the shutdown flag before it needs replication.
        let err = fleet.restore(&key, None).await.unwrap_err();
        assert!(matches!(err, FleetError::Unavailable { .. }), "{err}");
    }

    #[test]
    fn path_locks_exclude_each_other() {
        let tmp = tempfile::tempdir().unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        let (entered_tx, entered) = std::sync::mpsc::channel();
        let first = {
            let (root, log) = (tmp.path().to_path_buf(), Arc::clone(&log));
            std::thread::spawn(move || {
                with_path_lock(&root, "slot:00001", || {
                    entered_tx.send(()).unwrap();
                    std::thread::sleep(Duration::from_millis(200));
                    log.lock().unwrap().push("first leaves");
                    Ok(())
                })
                .unwrap();
            })
        };
        entered.recv().unwrap();
        with_path_lock(tmp.path(), "slot:00001", || {
            log.lock().unwrap().push("second enters");
            Ok(())
        })
        .unwrap();
        first.join().unwrap();
        assert_eq!(*log.lock().unwrap(), vec!["first leaves", "second enters"]);
    }

    #[test]
    fn a_create_waits_for_a_publisher_and_leaves_its_wal_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("000/slot-00005.db");
        let wal = tmp.path().join("000/slot-00005.db-wal");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        // Another process holds the path lock. It publishes a database and
        // commits to it while the lock is held.
        let (held_tx, held) = std::sync::mpsc::channel();
        let publisher = {
            let (root, target, wal) = (tmp.path().to_path_buf(), target.clone(), wal.clone());
            std::thread::spawn(move || {
                with_path_lock(&root, "slot:00005", || {
                    held_tx.send(()).unwrap();
                    std::thread::sleep(Duration::from_millis(300));
                    std::fs::write(&target, b"their database").unwrap();
                    std::fs::write(&wal, b"their live frames").unwrap();
                    Ok(())
                })
                .unwrap();
            })
        };
        held.recv().unwrap();
        let migrations = FleetMigrations::new(&Arc::new(Vec::new()), |_| true, true);
        let created = create_database_file(tmp.path(), &target, "slot:00005", &migrations).unwrap();
        publisher.join().unwrap();
        assert!(!created, "the publisher won: this create waited for it");
        assert_eq!(std::fs::read(&target).unwrap(), b"their database");
        assert_eq!(std::fs::read(&wal).unwrap(), b"their live frames");
    }

    #[test]
    fn a_new_database_does_not_inherit_a_stray_wal() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("000/slot-00003.db");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        let wal = tmp.path().join("000/slot-00003.db-wal");
        std::fs::write(&wal, b"left behind by a crash").unwrap();
        let migrations = FleetMigrations::new(&Arc::new(Vec::new()), |_| true, true);
        assert!(create_database_file(tmp.path(), &target, "slot:00003", &migrations).unwrap());
        assert!(!wal.exists(), "a stale -wal would replay onto the new file");
    }

    #[test]
    fn a_stray_wal_beside_a_published_database_is_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("000/slot-00004.db");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, b"their bytes").unwrap();
        let wal = tmp.path().join("000/slot-00004.db-wal");
        std::fs::write(&wal, b"their frames").unwrap();
        let migrations = FleetMigrations::new(&Arc::new(Vec::new()), |_| true, true);
        assert!(!create_database_file(tmp.path(), &target, "slot:00004", &migrations).unwrap());
        assert_eq!(std::fs::read(&wal).unwrap(), b"their frames");
    }

    #[tokio::test]
    async fn delete_waits_for_the_path_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let key = FleetDbKey::Slot(32);
        let path = fleet.open(&key).await.unwrap().path().to_path_buf();
        fleet.close(&key).await;
        let (held_tx, held) = std::sync::mpsc::channel();
        let holder = {
            let (root, name) = (tmp.path().to_path_buf(), key.name());
            std::thread::spawn(move || {
                with_path_lock(&root, &name, || {
                    held_tx.send(()).unwrap();
                    std::thread::sleep(Duration::from_millis(400));
                    Ok(())
                })
                .unwrap();
            })
        };
        held.recv().unwrap();
        let began = std::time::Instant::now();
        let deleting = {
            let (fleet, key) = (fleet.clone(), key.clone());
            tokio::spawn(async move { fleet.delete(&key).await })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(path.is_file(), "the delete waits for the lock holder");
        assert!(deleting.await.unwrap().unwrap());
        assert!(began.elapsed() >= Duration::from_millis(300));
        assert!(!path.exists());
        holder.join().unwrap();
    }

    #[tokio::test]
    async fn delete_clears_the_strays_of_an_absent_path() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let key = FleetDbKey::Slot(33);
        let path = fleet.path_of(&key);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let wal = crate::fleet_layout::sidecar_paths(&path)[0].clone();
        std::fs::write(&wal, b"left behind by a crash").unwrap();
        assert!(!fleet.delete(&key).await.unwrap(), "there was no database");
        assert!(!wal.exists());
    }

    /// A restore hook that records whether the stray `-wal` of the absent
    /// database was still there when it ran. It restores nothing.
    struct SeesStrays {
        wal_present: AtomicUsize,
        locked: AtomicUsize,
        root: PathBuf,
        name: String,
    }

    impl FleetLifecycle for SeesStrays {
        fn restore_missing(&self, _key: &FleetDbKey, path: &Path) -> Result<bool, String> {
            if crate::fleet_layout::sidecar_paths(path)[0].exists() {
                self.wal_present.fetch_add(1, Ordering::SeqCst);
            }
            // The fleet must hold the path lock while the hook runs: taking
            // it here from this thread must time out, so only try it.
            let dir = self.root.join(LOCK_DIR);
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(dir.join(format!("{:02x}.lock", lock_stripe(&self.name))))
                .map_err(|e| e.to_string())?;
            let mut lock = fd_lock::RwLock::new(file);
            if lock.try_write().is_err() {
                self.locked.fetch_add(1, Ordering::SeqCst);
            }
            Ok(false)
        }
    }

    #[tokio::test]
    async fn a_restore_on_open_runs_under_the_lock_after_the_strays_go() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = fleet(tmp.path(), FleetMode::Slot, |_| {});
        let key = FleetDbKey::Slot(34);
        let hook = Arc::new(SeesStrays {
            wal_present: AtomicUsize::new(0),
            locked: AtomicUsize::new(0),
            root: tmp.path().to_path_buf(),
            name: key.name(),
        });
        fleet.add_lifecycle(hook.clone());
        let path = fleet.path_of(&key);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let wal = crate::fleet_layout::sidecar_paths(&path)[0].clone();
        std::fs::write(&wal, b"left behind by a crash").unwrap();
        fleet.open(&key).await.unwrap();
        assert_eq!(hook.wal_present.load(Ordering::SeqCst), 0, "strays cleared");
        assert_eq!(hook.locked.load(Ordering::SeqCst), 1, "the hook ran locked");
    }
}
