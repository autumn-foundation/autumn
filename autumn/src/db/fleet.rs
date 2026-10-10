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
//!   remove from the map  ──► pool.close() (new checkouts fail fast)
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

// autumn-determinism-gate: production code in this module must read time and
// mint identifiers through the framework's injected seams (ClockSource /
// Entropy), never `Instant::now()` / `Utc::now()` / `SystemTime::now()` /
// `Uuid::new_v4()` directly. See CONTRIBUTING.md "Determinism seam gate"
// (issue #1797). Justify exceptions with
// #[allow(clippy::disallowed_methods, reason = "…")] at the narrowest scope.
#![cfg_attr(not(test), deny(clippy::disallowed_methods))]

use std::collections::HashMap;
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
/// [`DatabaseFleet::restore`] does not wait for leases: once it runs, the
/// pool refuses new checkouts and the next [`DatabaseFleet::open`] opens the
/// database fresh.
#[derive(Clone)]
pub struct FleetDatabase {
    key: FleetDbKey,
    path: Arc<Path>,
    pool: Pool<RuntimeConnection>,
    closed: CancellationToken,
    /// Counts the holders of this database. The fleet's own copy is one; any
    /// more and the database is in use, so it is never closed.
    lease: Arc<()>,
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

/// The migration sets every fleet database receives, with the collision map
/// computed once over them.
pub(crate) struct FleetMigrations {
    all: Arc<Vec<(&'static str, EmbeddedMigrations)>>,
    /// Indices into `all` of the sets a fleet database receives.
    selected: Vec<usize>,
    disambiguated: HashMap<String, String>,
    history: Vec<Vec<(String, String)>>,
    /// Whether opening an existing database applies its pending migrations.
    apply_on_open: bool,
}

impl FleetMigrations {
    /// The sets of `all` that `keep` accepts. The app passes a filter that
    /// drops the control-plane framework set.
    pub(crate) fn new(
        all: Arc<Vec<(&'static str, EmbeddedMigrations)>>,
        keep: impl Fn(&EmbeddedMigrations) -> bool,
        apply_on_open: bool,
    ) -> Self {
        let selected: Vec<usize> = all
            .iter()
            .enumerate()
            .filter(|(_, (_, set))| keep(set))
            .map(|(idx, _)| idx)
            .collect();
        let named: Vec<(&str, &EmbeddedMigrations)> = selected
            .iter()
            .map(|&idx| (all[idx].0, &all[idx].1))
            .collect();
        let disambiguated = crate::migrate::compute_migration_disambiguation(&named);
        let history = crate::migrate::sqlite_collision_pairs(&named);
        Self {
            all,
            selected,
            disambiguated,
            history,
            apply_on_open,
        }
    }

    /// Migrations in the selected sets, which a new database receives.
    fn count(&self) -> usize {
        use diesel::migration::MigrationSource;
        self.sets()
            .map(|set| {
                MigrationSource::<diesel::sqlite::Sqlite>::migrations(set).map_or(0, |m| m.len())
            })
            .sum()
    }

    fn sets(&self) -> impl Iterator<Item = &EmbeddedMigrations> {
        self.selected.iter().map(|&idx| &self.all[idx].1)
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
        let result = crate::migrate::run_pending_sqlite_quiet(url, self.chained()).map_err(fail)?;
        for migration in &result.applied {
            tracing::info!(database = %name, migration = %migration, "fleet migration applied");
        }
        Ok(result.applied.len())
    }

    /// The selected sets as one migration source.
    fn chained(&self) -> ChainedMigrations<'_> {
        ChainedMigrations(
            self.sets()
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
struct ChainedMigrations<'a>(Vec<crate::migrate::DisambiguatedMigrations<'a>>);

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

/// One entry of the open map. `cell` is filled once by the single opener.
struct Entry {
    cell: OnceCell<FleetDatabase>,
    last_use_seq: AtomicU64,
    last_use_at: Mutex<std::time::Instant>,
}

/// A database leaving the open map: its key, its handle, its drain marker.
type Draining = (FleetDbKey, Option<FleetDatabase>, Arc<Drain>);

/// What an opener does next: wait for a drain, or open through an entry.
enum Next {
    Wait(Arc<Drain>),
    Open(Arc<Entry>),
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
    seq: u64,
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
    state: Mutex<State>,
    lifecycle: std::sync::RwLock<Vec<Arc<dyn FleetLifecycle>>>,
    counters: Counters,
    in_flight_drains: AtomicUsize,
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
    migrations: Vec<(&'static str, EmbeddedMigrations)>,
    apply_on_open: bool,
}

impl DatabaseFleetBuilder {
    /// Add a migration set every database receives.
    #[must_use]
    pub fn migrations(mut self, name: &'static str, set: EmbeddedMigrations) -> Self {
        self.migrations.push((name, set));
        self
    }

    /// Whether opening an existing database applies its pending migrations.
    /// Default: `true`.
    #[must_use]
    pub const fn apply_migrations_on_open(mut self, apply: bool) -> Self {
        self.apply_on_open = apply;
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
        let migrations =
            FleetMigrations::new(Arc::new(self.migrations), |_| true, self.apply_on_open);
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
                state: Mutex::new(State::default()),
                lifecycle: std::sync::RwLock::new(Vec::new()),
                counters: Counters::default(),
                in_flight_drains: AtomicUsize::new(0),
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
    /// # Errors
    ///
    /// [`FleetError::AlreadyExists`] when the file is already there, or an
    /// [`open`](Self::open) error.
    pub async fn provision(&self, key: &FleetDbKey) -> Result<FleetDatabase, FleetError> {
        self.check_key(key)?;
        let path = self.path_of(key);
        if path.exists() {
            return Err(FleetError::AlreadyExists { name: key.name() });
        }
        // Publish here, outside the single-flight open, so the publish result
        // decides: of two concurrent provisions (in this process or another)
        // only the one whose link created the file succeeds, and its caller
        // alone runs whatever signup work follows.
        let fleet = self.clone();
        let name = key.name();
        let created = crate::time::spawn_blocking(move || {
            // Before any file work: staging and publish would otherwise follow
            // a bucket symlink and leave a database outside the root.
            check_contained(fleet.root(), &path)?;
            create_database_file(&path, &name, &fleet.inner.migrations)
        })
        .await
        .map_err(|e| FleetError::Unavailable {
            name: key.name(),
            detail: format!("create task failed: {e}"),
        })??;
        if !created {
            return Err(FleetError::AlreadyExists { name: key.name() });
        }
        let counters = &self.inner.counters;
        counters.created.fetch_add(1, Ordering::Relaxed);
        counters
            .migrations_applied
            .fetch_add(self.inner.migrations.count() as u64, Ordering::Relaxed);
        self.open_with(key, false).await
    }

    /// Whether the database's file exists.
    #[must_use]
    pub fn exists(&self, key: &FleetDbKey) -> bool {
        self.path_of(key).is_file()
    }

    async fn open_with(&self, key: &FleetDbKey, create: bool) -> Result<FleetDatabase, FleetError> {
        self.check_key(key)?;
        loop {
            let next = self.claim_entry(key)?;
            let entry = match next {
                Next::Wait(drain) => {
                    drain.wait().await;
                    continue;
                }
                Next::Open(entry) => entry,
            };
            let result = entry
                .cell
                .get_or_try_init(|| self.open_now(key.clone(), create))
                .await
                .cloned();
            match result {
                Ok(db) => {
                    // The pool is closed when the database was drained between
                    // our lookup and now; go round and open it fresh.
                    if db.pool.is_closed() {
                        continue;
                    }
                    self.enforce_capacity(Some(key));
                    return Ok(db);
                }
                Err(error) => {
                    self.inner
                        .counters
                        .open_failures
                        .fetch_add(1, Ordering::Relaxed);
                    self.forget_entry(key, &entry);
                    return Err(error);
                }
            }
        }
    }

    /// Under the lock: wait for a drain of `key`, or take (or make) its entry
    /// and mark it used.
    fn claim_entry(&self, key: &FleetDbKey) -> Result<Next, FleetError> {
        let mut state = self.lock_state();
        if let Some(drain) = state.draining.get(key).cloned() {
            drop(state);
            if drain.deleting {
                return Err(FleetError::Deleting { name: key.name() });
            }
            return Ok(Next::Wait(drain));
        }
        state.seq += 1;
        let seq = state.seq;
        let entry = Arc::clone(state.open.entry(key.clone()).or_insert_with(|| {
            Arc::new(Entry {
                cell: OnceCell::new(),
                last_use_seq: AtomicU64::new(seq),
                last_use_at: Mutex::new(crate::time::ambient_instant()),
            })
        }));
        drop(state);
        entry.touch(seq);
        Ok(Next::Open(entry))
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

    async fn open_now(&self, key: FleetDbKey, create: bool) -> Result<FleetDatabase, FleetError> {
        let path = self.path_of(&key);
        let name = key.name();
        check_contained(self.root(), &path)?;
        let mut existed = path.is_file();
        if !existed {
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
                create_database_file(&create_path, &create_name, &fleet.inner.migrations)
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
        };
        // The file is complete here — published fully migrated, restored, or
        // already there — so a failure below closes the pool and never
        // removes the file another opener (or process) may be serving.
        let applied = match self.prepare(&db).await {
            Ok(applied) => applied,
            Err(error) => {
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
        crate::time::spawn_blocking(move || {
            for hook in hooks {
                if hook.restore_missing(&hook_key, &hook_path)? {
                    return Ok(true);
                }
            }
            Ok(false)
        })
        .await
        .map_err(|e| e.to_string())
        .and_then(|r| r)
        .map_err(|detail| FleetError::Restore {
            name: key.name(),
            detail,
        })
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
                .filter(|(_, entry)| {
                    now.saturating_duration_since(entry.last_used()) >= min_residency
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
            self.spawn_drain(key, db, drain);
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
            self.spawn_drain(key, db, drain);
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

    fn spawn_drain(&self, key: FleetDbKey, db: Option<FleetDatabase>, drain: Arc<Drain>) {
        let fleet = self.clone();
        tokio::spawn(async move {
            fleet.drain(&key, db).await;
            fleet.finish_drain(&key, &drain);
        });
    }

    /// Close one database: refuse new checkouts, wait for the in-flight ones,
    /// then run the `on_close` hooks.
    async fn drain(&self, key: &FleetDbKey, db: Option<FleetDatabase>) {
        let Some(db) = db else {
            return;
        };
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
    pub async fn close(&self, key: &FleetDbKey) {
        let begun = self.begin_drains(|_| vec![key.clone()]).pop();
        if let Some((key, db, drain)) = begun {
            self.drain(&key, db).await;
            self.finish_drain(&key, &drain);
        } else {
            let pending = self.lock_state().draining.get(key).cloned();
            if let Some(drain) = pending {
                drain.wait().await;
            }
        }
    }

    /// Close every open database and wait for all drains, for shutdown.
    pub async fn close_all(&self) {
        let keys: Vec<FleetDbKey> = self.lock_state().open.keys().cloned().collect();
        for key in keys {
            self.close(&key).await;
        }
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
            fleet.close_all().await;
            // Every database is closed and shipped. Give parked replicators a
            // last chance to catch up, then stop the loop thread so it does
            // not outlive the app (an embedded or restarted server).
            if let Some(replication) = fleet.replication().cloned() {
                let _ = crate::time::spawn_blocking(move || {
                    replication.tick_all();
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
    /// The database's replica, if any, is not deleted here: a lifecycle hook
    /// (`on_delete`) decides.
    ///
    /// # Errors
    ///
    /// [`FleetError::Deleting`] when a delete is already running, or
    /// [`FleetError::Io`].
    pub async fn delete(&self, key: &FleetDbKey) -> Result<bool, FleetError> {
        self.check_key(key)?;
        self.settle_opening(key).await;
        let begun = self.begin_exclusive(key, true)?;
        let Some((key, db, drain)) = begun else {
            // A plain close is running; wait, then delete.
            let pending = self.lock_state().draining.get(key).cloned();
            if let Some(pending) = pending {
                pending.wait().await;
            }
            return Box::pin(self.delete(key)).await;
        };
        self.drain(&key, db).await;
        // Hooks let go of the files first (a parked replicator holds one open).
        for hook in self.hooks() {
            hook.release(&key);
        }
        let path = self.path_of(&key);
        let root = self.inner.root.clone();
        let name = key.name();
        let removed = crate::time::spawn_blocking(move || {
            // Another process sharing the root may still serve the file;
            // unlinking it under that process would split the tenant's data
            // between the old inode and a new file. The guard refuses then,
            // and blocks new openers while the files go.
            check_contained(&root, &path)?;
            let guard = exclusive_guard(&path, &name)?;
            let removed = remove_database_files(&path, &root);
            drop(guard);
            removed
        })
        .await
        .map_err(|e| FleetError::Io {
            op: "delete database",
            detail: e.to_string(),
        })
        .and_then(|r| r);
        if matches!(removed, Ok(true)) {
            self.inner.counters.deleted.fetch_add(1, Ordering::Relaxed);
            // Only now: a refused delete (another process has the file open)
            // or a missing file must not trigger, say, replica cleanup.
            for hook in self.hooks() {
                hook.on_delete(&key);
            }
        }
        self.finish_drain(&key, &drain);
        removed
    }

    /// Wait until no open of `key` is in progress. An open is never
    /// interrupted: the probe joins the running initializer, or fails at once
    /// when there is none.
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
            let _ = entry
                .cell
                .get_or_try_init(|| async { Err::<FleetDatabase, ()>(()) })
                .await;
            if entry.cell.get().is_none() {
                // Our probe settled it as failed: drop the empty entry.
                self.forget_entry(key, &entry);
            }
        }
    }

    /// Under the lock: take `key` out of service for a delete (openers get
    /// `410`) or a restore (openers wait). `Ok(None)` when a plain close is
    /// running: the caller waits for it, then retries.
    fn begin_exclusive(
        &self,
        key: &FleetDbKey,
        deleting: bool,
    ) -> Result<Option<Draining>, FleetError> {
        let mut state = self.lock_state();
        if let Some(drain) = state.draining.get(key) {
            return if drain.deleting {
                Err(FleetError::Deleting { name: key.name() })
            } else {
                Ok(None)
            };
        }
        if let Some(begun) = self.begin_drain_locked(&mut state, key, deleting) {
            return Ok(Some(begun));
        }
        let drain = Arc::new(Drain {
            deleting,
            finished: AtomicBool::new(false),
            done: Notify::new(),
        });
        state.draining.insert(key.clone(), Arc::clone(&drain));
        drop(state);
        self.inner.in_flight_drains.fetch_add(1, Ordering::Relaxed);
        Ok(Some((key.clone(), None, drain)))
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
    /// # Errors
    ///
    /// [`FleetError::NotReplicated`] without replication,
    /// [`FleetError::NotFound`] when nothing was ever shipped for the key,
    /// [`FleetError::Deleting`] during a delete, or [`FleetError::Restore`].
    pub async fn restore(
        &self,
        key: &FleetDbKey,
        target: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<crate::replication::RestoreOutcome, FleetError> {
        self.check_key(key)?;
        let replication = self
            .inner
            .replication
            .get()
            .cloned()
            .ok_or_else(|| FleetError::NotReplicated { name: key.name() })?;
        loop {
            self.settle_opening(key).await;
            if let Some((key, db, drain)) = self.begin_exclusive(key, false)? {
                self.drain(&key, db).await;
                for hook in self.hooks() {
                    hook.release(&key);
                }
                let path = self.path_of(&key);
                let root = self.inner.root.clone();
                let restore_key = key.clone();
                // Everything after the claim runs to `finish_drain`, whatever
                // fails: an error that skipped it would strand every later
                // opener of this key on a drain that never finishes.
                let restored = crate::time::spawn_blocking(move || {
                    check_contained(&root, &path)?;
                    restore_in_place(&replication, &restore_key, target, &path)
                })
                .await
                .map_err(|error| FleetError::Restore {
                    name: key.name(),
                    detail: error.to_string(),
                })
                .and_then(|r| r);
                self.finish_drain(&key, &drain);
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
    /// use it for admin work, not on a hot path.
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
        let keys = self.list().await?;
        let f = &f;
        let results = futures::stream::iter(keys.into_iter().map(|key| async move {
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
        use futures::StreamExt as _;
        let keys = self.list().await?;
        let mut report = FleetMigrationReport {
            databases: keys.len(),
            ..FleetMigrationReport::default()
        };
        let outcomes = futures::stream::iter(keys.into_iter().map(|key| {
            let fleet = self.clone();
            async move {
                let blocking_key = key.clone();
                let outcome = crate::time::spawn_blocking(move || {
                    migrate_existing_blocking(&fleet, &blocking_key)
                })
                .await
                .map_err(|e| FleetError::Migration {
                    name: key.name(),
                    detail: e.to_string(),
                })
                .and_then(|r| r);
                (key, outcome)
            }
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
        Ok(report)
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
}

impl Entry {
    fn last_used(&self) -> std::time::Instant {
        *self
            .last_use_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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
    let url = existing_only_url(&path);
    match check_identity_blocking(&url, &name) {
        Ok(()) => fleet.inner.migrations.apply(&url, &name),
        Err(_) if !path.exists() => Ok(0),
        Err(error) => Err(error),
    }
}

/// A connection string that opens `path` only if the file exists. On Unix a
/// `file:` URI with `mode=rw`, which `SQLite` refuses to create through
/// (`%`, `?` and `#` are escaped, as the URI grammar needs). Elsewhere the
/// plain path, whose canonical Windows spelling (`\\?\C:\…`) has no URI
/// form; [`migrate_existing_blocking`]'s existence check then narrows the
/// window instead.
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
/// opener creates at the same path.
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

/// Replace the database at `path` with its replica. Blocking.
///
/// The replica is rebuilt in a private staging file first, so a failed
/// restore leaves the current file untouched. Then, under the exclusive
/// guard (no other process may serve the old file), the old files are
/// removed and the staging file is linked into place.
fn restore_in_place(
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
    let published = (|| {
        let guard = exclusive_guard(path, &name)?;
        for file in
            std::iter::once(path.to_path_buf()).chain(crate::fleet_layout::sidecar_paths(path))
        {
            match std::fs::remove_file(&file) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(io_error("replace database")(e)),
            }
        }
        std::fs::hard_link(&staging, path).map_err(io_error("publish restored database"))?;
        drop(guard);
        Ok(())
    })();
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
    let published = std::fs::hard_link(&staging, path);
    discard(&staging);
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
/// when there was no file.
fn remove_database_files(path: &Path, root: &Path) -> Result<bool, FleetError> {
    let existed = path.is_file();
    for file in std::iter::once(path.to_path_buf()).chain(crate::fleet_layout::sidecar_paths(path))
    {
        match std::fs::remove_file(&file) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_error("delete database")(e)),
        }
    }
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
    Ok(existed)
}

/// Readiness indicator `db:fleet`: the root directory is usable, with the
/// fleet's counters as details. Individual databases are not pinged — a
/// tenant fleet can hold millions — a failed open answers its own request.
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
    all: Arc<Vec<(&'static str, EmbeddedMigrations)>>,
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
    // Config validation compares paths lexically; a symlinked root can still
    // reach the control database. `root()` is canonical, so compare the
    // control file's canonical path too: the configured one, and the one a
    // custom pool provider resolved at runtime (it may differ).
    let control_targets = config
        .database
        .effective_primary_url()
        .into_iter()
        .chain(resolved_control_url)
        .filter_map(crate::config::sqlite_url_file)
        .filter_map(|path| canonical_target(&path));
    if let Some(control) = control_targets
        .into_iter()
        .find(|control| control.starts_with(fleet.root()))
    {
        return Err(format!(
            "Failed to set up database.fleet: the control database {} resolves inside \
             database.fleet.root {}; a tenant or slot path could open it. Put the control \
             database outside the fleet root",
            control.display(),
            fleet.root().display()
        ));
    }
    Ok(Some(fleet))
}

/// `path` with every symlink resolved: the file itself when it exists, else
/// its canonical parent plus the file name. `None` when the parent is missing
/// too (then nothing under the root can alias it yet).
fn canonical_target(path: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(path).ok().or_else(|| {
        let parent = std::fs::canonicalize(path.parent()?).ok()?;
        Some(parent.join(path.file_name()?))
    })
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
        for slot in 1..=3 {
            fleet.open(&FleetDbKey::Slot(slot)).await.unwrap();
        }
        settle(&fleet).await;
        assert_eq!(fleet.stats().open, 3, "a burst inside the grace stays open");
        tokio::time::sleep(Duration::from_millis(200)).await;
        fleet.enforce_capacity(None);
        settle(&fleet).await;
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
        let migrations = FleetMigrations::new(Arc::new(Vec::new()), |_| true, true);
        let created = create_database_file(&target, "slot:00001", &migrations).unwrap();
        assert!(!created, "the other process won the race");
        assert_eq!(std::fs::read(&target).unwrap(), b"their bytes");
        assert_eq!(
            entries(tmp.path()),
            vec!["slot-00001.db".to_owned()],
            "no staging left"
        );

        let fresh = tmp.path().join("000/slot-00002.db");
        assert!(create_database_file(&fresh, "slot:00002", &migrations).unwrap());
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
}
