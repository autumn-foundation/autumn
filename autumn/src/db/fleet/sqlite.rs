//! The `SQLite` engine of a fleet.
//!
//! It also holds the fleet operations only `SQLite` has: provision, import,
//! backup, restore from the replica, migrations, inspection and replication.

use diesel_async::pooled_connection::deadpool::Pool;

use super::{
    Arc, AtomicBool, AtomicU64, DatabaseFleet, DatabaseFleetConfig, Duration, FleetDatabase,
    FleetDbKey, FleetEngine, FleetError, HashMap, MutationGuard, Ordering, Path, PathBuf, ShipMode,
    check_contained, discard_staging, fmt, io_error, prune_empty_dirs, removal_error, staging_path,
    with_path_lock,
};
use crate::db::RuntimeConnection;
use crate::migrate::EmbeddedMigrations;

/// The table that records which database a file is.
pub(super) const IDENTITY_TABLE: &str = "_autumn_fleet_identity";

/// One row: the newest ownership epoch the file was opened under (issue
/// #3233). A file stamped below the replica's newest epoch is stale.
pub(super) const EPOCH_TABLE: &str = "_autumn_fleet_epoch";

/// The page cache one `SQLite` connection can hold: the default
/// `cache_size` of 2000 KiB. The fleet sets no other value.
pub(super) const CACHE_BYTES_PER_CONNECTION: u64 = 2000 * 1024;

// ── The engine ───────────────────────────────────────────────────────────────

/// The `SQLite` engine: one database file per key, opened as a connection
/// pool. Built by [`DatabaseFleet::builder`].
///
/// A new file gets an identity row and every migration before it is
/// published. An open records the epoch, checks the identity row on the
/// first pooled connection (which also sets WAL), then applies pending
/// migrations or refuses them.
pub struct SqliteEngine {
    root: PathBuf,
    pool_size: usize,
    connect_timeout_secs: u64,
    /// `mmap_size` and `cache_size` of each database connection.
    tuning: crate::db::SqliteTuning,
    migrations: Arc<FleetMigrations>,
    replicating: AtomicBool,
    migrations_applied: AtomicU64,
}

impl fmt::Debug for SqliteEngine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SqliteEngine")
            .field("root", &self.root)
            .field("pool_size", &self.pool_size)
            .finish_non_exhaustive()
    }
}

impl SqliteEngine {
    fn new(
        root: &Path,
        pool_size: usize,
        connect_timeout_secs: u64,
        tuning: crate::db::SqliteTuning,
        migrations: FleetMigrations,
    ) -> Self {
        Self {
            root: root.to_path_buf(),
            pool_size: pool_size.max(1),
            connect_timeout_secs,
            tuning,
            migrations: Arc::new(migrations),
            replicating: AtomicBool::new(false),
            migrations_applied: AtomicU64::new(0),
        }
    }

    pub(super) fn url_of(path: &Path) -> String {
        format!("sqlite://{}", path.display())
    }

    fn count_applied(&self, applied: usize) {
        self.migrations_applied
            .fetch_add(applied as u64, Ordering::Relaxed);
    }

    /// Identity check and migrations on a new pool. Returns the number of
    /// migrations applied.
    async fn prepare(
        &self,
        pool: &Pool<RuntimeConnection>,
        path: &Path,
        name: &str,
    ) -> Result<usize, FleetError> {
        // The first pooled connection sets WAL and checks (or, for a file
        // that predates identity rows, records) which database it is.
        check_identity(pool, name).await?;
        let migrations = Arc::clone(&self.migrations);
        let (url, migrate_name) = (Self::url_of(path), name.to_owned());
        crate::time::spawn_blocking(move || {
            if migrations.apply_on_open {
                migrations.apply(&url, &migrate_name)
            } else {
                match migrations.pending(&url, &migrate_name)? {
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
            name: name.to_owned(),
            detail: format!("migration task failed: {e}"),
        })?
    }
}

/// The error of a blocking task that did not finish.
pub(super) fn task_failed(
    name: &str,
    what: &str,
) -> impl FnOnce(tokio::task::JoinError) -> FleetError {
    let (name, what) = (name.to_owned(), what.to_owned());
    move |e| FleetError::Unavailable {
        name,
        detail: format!("{what} task failed: {e}"),
    }
}

impl FleetEngine for SqliteEngine {
    type Handle = Pool<RuntimeConnection>;

    async fn open(
        &self,
        key: &FleetDbKey,
        path: &Path,
        epoch: Option<crate::lock::FencingToken>,
    ) -> Result<Pool<RuntimeConnection>, FleetError> {
        let name = key.name();
        if !self.exists(path) {
            // Whether this open published the file. Another process may win
            // the race to create it: then we open theirs.
            let (root, create_path, create_name, migrations) = (
                self.root.clone(),
                path.to_path_buf(),
                name.clone(),
                Arc::clone(&self.migrations),
            );
            let created = crate::time::spawn_blocking(move || {
                create_database_file(&root, &create_path, &create_name, &migrations)
            })
            .await
            .map_err(task_failed(&name, "create"))??;
            if created {
                self.count_applied(self.migrations.count());
            }
        }
        // Record the epoch before anyone serves the file. A newer stamp
        // means a newer owner: refuse.
        if let Some(epoch) = epoch {
            let (stamp_path, stamp_name) = (path.to_path_buf(), name.clone());
            crate::time::spawn_blocking(move || stamp_epoch(&stamp_path, &stamp_name, epoch.get()))
                .await
                .map_err(task_failed(&name, "epoch stamp"))??;
        }
        let pool = crate::db::build_sqlite_pool_with(
            &Self::url_of(path),
            self.pool_size,
            self.connect_timeout_secs,
            crate::db::SqliteCheckpointOwner::Fleet(self.replicating.load(Ordering::Acquire)),
            self.tuning,
        )
        .map_err(|e| FleetError::Unavailable {
            name: name.clone(),
            detail: e.to_string(),
        })?;
        // The file is complete here, so a failure below closes the pool and
        // never removes the file another opener (or process) may serve.
        match self.prepare(&pool, path, &name).await {
            Ok(applied) => {
                self.count_applied(applied);
                Ok(pool)
            }
            Err(error) => {
                let closer = pool.clone();
                let _ = crate::time::spawn_blocking(move || closer.close()).await;
                Err(error)
            }
        }
    }

    fn close(&self, pool: &Pool<RuntimeConnection>) {
        pool.close();
    }

    fn in_use(&self, pool: &Pool<RuntimeConnection>) -> bool {
        let status = pool.status();
        status.available < status.size || status.waiting > 0
    }

    async fn snapshot(
        &self,
        pool: &Pool<RuntimeConnection>,
        path: &Path,
        dir: &Path,
    ) -> Result<Vec<PathBuf>, FleetError> {
        let file = path.file_name().ok_or_else(|| FleetError::Io {
            op: "snapshot",
            detail: format!("{} has no file name", path.display()),
        })?;
        let dest = dir.join(file);
        vacuum_into(pool, &dest, &path.display().to_string()).await?;
        Ok(vec![dest])
    }

    fn remove(&self, key: &FleetDbKey, path: &Path) -> Result<bool, FleetError> {
        let name = key.name();
        // Another process sharing the root may still serve the file;
        // unlinking it under that process would split the tenant's data
        // between the old inode and a new file. The guard refuses then. On
        // Unix it also blocks new openers while the files go. On Windows it
        // closes first.
        with_exclusive_access(path, &name, HOLD_GUARD_WHILE_REMOVING, |present| {
            remove_database_files(path, &self.root, &name, present)
        })
    }

    fn resident_bytes(&self, pool: &Pool<RuntimeConnection>) -> u64 {
        u64::try_from(pool.status().size)
            .unwrap_or(u64::MAX)
            .saturating_mul(CACHE_BYTES_PER_CONNECTION)
    }

    fn ship_mode(&self) -> ShipMode {
        ShipMode::Continuous(Arc::new(crate::db::fleet_engine::WalShipper))
    }

    fn epoch_stamp(&self, key: &FleetDbKey, path: &Path) -> Result<Option<u64>, FleetError> {
        read_epoch_stamp(path, &key.name()).map(Some)
    }

    fn exists(&self, path: &Path) -> bool {
        path.is_file()
    }

    fn sidecars(&self, path: &Path) -> Vec<PathBuf> {
        let mut files = crate::fleet_layout::sidecar_paths(path).to_vec();
        files.push(crate::fleet_layout::replicator_lock_path(path));
        files
    }

    fn migrations_applied(&self) -> u64 {
        self.migrations_applied.load(Ordering::Relaxed)
    }
}

/// Write a consistent copy of the database to `dest` with `VACUUM INTO`.
/// Readers and writers keep running meanwhile.
pub(super) async fn vacuum_into(
    pool: &Pool<RuntimeConnection>,
    dest: &Path,
    name: &str,
) -> Result<(), FleetError> {
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
    let mut conn = pool.get().await.map_err(|e| FleetError::Unavailable {
        name: name.to_owned(),
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
pub(super) type SqliteSource =
    dyn diesel::migration::MigrationSource<diesel::sqlite::Sqlite> + Send + Sync;

/// One migration set of a fleet. Cheap to clone.
#[derive(Clone)]
pub(super) struct FleetSet(Arc<SqliteSource>);

impl diesel::migration::MigrationSource<diesel::sqlite::Sqlite> for FleetSet {
    fn migrations(
        &self,
    ) -> diesel::migration::Result<Vec<Box<dyn diesel::migration::Migration<diesel::sqlite::Sqlite>>>>
    {
        self.0.migrations()
    }
}

/// One embedded set of the app's shared list.
pub(super) struct SharedEmbedded {
    all: Arc<Vec<crate::migrate::RegisteredMigrations>>,
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
pub struct FleetMigrations {
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
    /// The sets of `all` that reach the shards, by
    /// [`applied_scope`](crate::migrate::applied_scope): a `Control` set and
    /// the control-plane framework set do not.
    pub(crate) fn new(
        all: &Arc<Vec<crate::migrate::RegisteredMigrations>>,
        apply_on_open: bool,
    ) -> Self {
        let sets = all
            .iter()
            .enumerate()
            .filter(|(_, (_, set, scope, _))| {
                crate::migrate::applied_scope(set, *scope).reaches_shards()
            })
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
pub(super) struct ChainedMigrations<'a>(Vec<crate::migrate::DisambiguatedMigrations<'a, FleetSet>>);

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

/// Builder for a [`DatabaseFleet`] outside app boot (tests, tools).
pub struct DatabaseFleetBuilder {
    config: DatabaseFleetConfig,
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
    /// close it. Replaces [`DatabaseFleetConfig::min_residency`].
    #[must_use]
    pub const fn min_residency(mut self, residency: Duration) -> Self {
        self.config.min_residency = residency;
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
        DatabaseFleet::from_parts(&self.config, self.connect_timeout_secs, migrations)
    }
}

impl FleetDatabase<SqliteEngine> {
    /// The database's connection pool.
    #[must_use]
    pub const fn pool(&self) -> &Pool<RuntimeConnection> {
        &self.handle
    }
}

impl DatabaseFleet<SqliteEngine> {
    /// Start building a fleet from its configuration section.
    #[must_use]
    pub const fn builder(config: DatabaseFleetConfig) -> DatabaseFleetBuilder {
        DatabaseFleetBuilder {
            config,
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
    ) -> Result<Self, FleetError> {
        Self::from_config(config, |root| {
            SqliteEngine::new(
                root,
                config.pool_size,
                connect_timeout_secs,
                config.sqlite_tuning(),
                migrations,
            )
        })
    }

    /// Connections per open database.
    #[must_use]
    pub fn pool_size(&self) -> usize {
        self.inner.engine.pool_size
    }

    /// Start WAL shipping: databases open with `wal_autocheckpoint = 0`,
    /// and `replication` is their only checkpointer. It registers on open
    /// and ships the last frames on close.
    pub(super) fn attach_wal_shipping(
        &self,
        replication: Arc<crate::db::fleet_replication::FleetReplication>,
    ) {
        self.inner.engine.replicating.store(true, Ordering::Release);
        self.add_lifecycle(replication);
    }

    /// A lake over this fleet's replicas (issue #3236). It carries the
    /// tombstones and refuses a scratch directory under the fleet root.
    ///
    /// # Errors
    ///
    /// [`FleetError::NotReplicated`] when the fleet does not replicate.
    pub fn lake(&self) -> Result<crate::db::fleet_lake::FleetLake, FleetError> {
        let replication = self
            .replication()
            .ok_or_else(|| FleetError::NotReplicated {
                name: self.root().display().to_string(),
            })?;
        Ok(replication.lake().outside_root(self.root()))
    }

    /// Whether databases are opened for an external checkpointer.
    #[must_use]
    pub fn replicating(&self) -> bool {
        self.inner.engine.replicating.load(Ordering::Acquire)
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

    async fn publish(&self, key: &FleetDbKey) -> Result<(), FleetError> {
        let path = self.path_of(key);
        if path.exists() {
            return Ok(());
        }
        self.purge_before_create(key, true).await?;
        // The publish result decides: of two concurrent provisions (in this
        // process or another) only the one whose link created the file
        // records a creation, and its caller alone runs the signup work.
        let fleet = self.clone();
        let name = key.name();
        let created = crate::time::spawn_blocking(move || {
            // Before any file work: staging and publish would otherwise follow
            // a bucket symlink and leave a database outside the root.
            check_contained(fleet.root(), &path)?;
            create_database_file(fleet.root(), &path, &name, &fleet.inner.engine.migrations)
        })
        .await
        .map_err(|e| FleetError::Unavailable {
            name: key.name(),
            detail: format!("create task failed: {e}"),
        })??;
        if created {
            self.inner.counters.created.fetch_add(1, Ordering::Relaxed);
            self.inner
                .engine
                .count_applied(self.inner.engine.migrations.count());
            self.lock_state().unacknowledged.insert(key.clone());
        }
        Ok(())
    }

    /// Copy a database file into the fleet as `key`, for example a backup
    /// that [`backup`](Self::backup) wrote, then open it. See
    /// [`import_file`](Self::import_file) for the steps and the errors.
    ///
    /// # Errors
    ///
    /// An [`import_file`](Self::import_file) error, or an
    /// [`open`](Self::open) error. An open error comes after the publish:
    /// the file is in place, and a second import answers
    /// [`FleetError::AlreadyExists`].
    pub async fn import(
        &self,
        key: &FleetDbKey,
        source: &Path,
    ) -> Result<FleetDatabase, FleetError> {
        // Held to the end, so `close_all` also sees the open that follows.
        let _open_counted = self.admit_mutation(key)?;
        self.import_file(key, source).await?;
        self.open_with(key, false).await
    }

    /// Copy a database file into the fleet as `key`, and do not open it.
    /// `source` does not change.
    ///
    /// 1. The fleet copies `source` with `VACUUM INTO` into a private
    ///    staging file, so rows still in a `-wal` come too.
    /// 2. The copy must pass `PRAGMA integrity_check`, and its identity row
    ///    must record `key`.
    /// 3. The fleet applies the pending migrations to the copy. With
    ///    ownership, it sets the copy's epoch to this host's epoch. Without
    ///    ownership, it removes the copy's epoch.
    /// 4. With replication, the fleet purges the tombstone and the old
    ///    replica of a deleted key, as [`provision`](Self::provision) does.
    ///    It does this only after the checks pass, so a refused source keeps
    ///    the replica.
    /// 5. The fleet publishes the copy in one step under the path lock. It
    ///    never replaces a file.
    ///
    /// Use it for files you trust: the fleet keeps the source's schema, its
    /// triggers and its migration records. Cancellation-safe: the work runs
    /// in an owned task.
    ///
    /// # Errors
    ///
    /// [`FleetError::AlreadyExists`] when the key has a file.
    /// [`FleetError::InvalidSource`] when the source is missing, is not a
    /// database, fails the integrity check, or records no identity or
    /// another key. [`FleetError::NotOwner`] when this host loses the slot.
    /// [`FleetError::Migration`] or [`FleetError::Unavailable`] when a
    /// migration or the purge fails. Nothing is published on an error.
    pub async fn import_file(&self, key: &FleetDbKey, source: &Path) -> Result<(), FleetError> {
        self.check_key(key)?;
        let epoch = self.owned_epoch(key)?;
        let (fleet, task_key, source) = (self.clone(), key.clone(), source.to_path_buf());
        let counted = self.admit_mutation(key)?;
        tokio::spawn(async move {
            let _counted = counted;
            fleet
                .with_provision_turn(&task_key, fleet.import_owned(&task_key, &source, epoch))
                .await
        })
        .await
        .map_err(|e| FleetError::Unavailable {
            name: key.name(),
            detail: format!("import task failed: {e}"),
        })?
    }

    async fn import_owned(
        &self,
        key: &FleetDbKey,
        source: &Path,
        epoch: Option<crate::lock::FencingToken>,
    ) -> Result<(), FleetError> {
        let path = self.path_of(key);
        if path.exists() {
            return Err(FleetError::AlreadyExists { name: key.name() });
        }
        let task_failed = |e: tokio::task::JoinError| FleetError::Unavailable {
            name: key.name(),
            detail: format!("import task failed: {e}"),
        };
        // Check and migrate the copy first: a refused source must not end
        // the retention of a deleted key's replica.
        let (fleet, staged_path, source, name) =
            (self.clone(), path.clone(), source.to_path_buf(), key.name());
        let (staging, applied) = crate::time::spawn_blocking(move || {
            check_contained(fleet.root(), &staged_path)?;
            stage_import_file(
                &staged_path,
                &name,
                &source,
                &fleet.inner.engine.migrations,
                epoch.map(crate::lock::FencingToken::get),
            )
        })
        .await
        .map_err(task_failed)??;
        let published = self.publish_import(key, &path, &staging, epoch).await;
        discard_staging(&staging);
        published?;
        self.inner.counters.created.fetch_add(1, Ordering::Relaxed);
        self.inner.engine.count_applied(applied);
        tracing::info!(database = %key, "fleet database imported");
        Ok(())
    }

    /// Purge a deleted key's old replica, then link the checked staging file.
    async fn publish_import(
        &self,
        key: &FleetDbKey,
        path: &Path,
        staging: &Path,
        epoch: Option<crate::lock::FencingToken>,
    ) -> Result<(), FleetError> {
        // The copy carries the epoch read at the start. A slot lost since
        // then must not get a file stamped for it.
        if epoch.is_some() && self.owned_epoch(key)? != epoch {
            return Err(FleetError::NotOwner {
                name: key.name(),
                slot: key.routing_slot(),
            });
        }
        self.purge_before_create(key, true).await?;
        let (root, path, staging, name) = (
            self.inner.root.clone(),
            path.to_path_buf(),
            staging.to_path_buf(),
            key.name(),
        );
        crate::time::spawn_blocking(
            move || match publish_staged(&root, &path, &name, &staging)? {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    Err(FleetError::AlreadyExists { name })
                }
                Err(e) => Err(io_error("publish database")(e)),
            },
        )
        .await
        .map_err(|e| FleetError::Unavailable {
            name: key.name(),
            detail: format!("import task failed: {e}"),
        })?
    }

    /// Copy `source` into a staging file beside the database of `key`, then
    /// check and migrate it, for [`restore_from_file`](Self::restore_from_file).
    /// Blocking. A refused source is [`FleetError::Restore`]. Migrations run
    /// when `migrate` is set or when opens apply them.
    pub(super) fn stage_restore(
        &self,
        key: &FleetDbKey,
        source: &Path,
        rekey: bool,
        migrate: bool,
        epoch: Option<u64>,
    ) -> Result<Staged, FleetError> {
        let (path, name) = (self.path_of(key), key.name());
        check_contained(self.root(), &path)?;
        let migrations = &self.inner.engine.migrations;
        let rules = StageRules {
            tag: "restoring",
            rekey,
            migrate: migrate || migrations.apply_on_open,
        };
        let invalid = |detail: String| FleetError::Restore {
            name: name.clone(),
            detail: format!("{}: {detail}", source.display()),
        };
        stage_file(&path, &name, source, migrations, epoch, rules, invalid)
    }

    /// Count migrations a restore applied.
    pub(super) fn count_migrations(&self, applied: usize) {
        self.inner.engine.count_applied(applied);
    }

    /// Write a consistent copy of a database to `dest` with `VACUUM INTO`.
    /// `dest` must not exist. Readers and writers keep running meanwhile.
    ///
    /// # Errors
    ///
    /// An [`open`](Self::open) error, [`FleetError::AlreadyExists`] for an
    /// existing `dest`, or [`FleetError::Io`] when the copy fails.
    pub async fn backup(&self, key: &FleetDbKey, dest: &Path) -> Result<(), FleetError> {
        if dest.exists() {
            return Err(FleetError::AlreadyExists {
                name: dest.display().to_string(),
            });
        }
        let db = self.open_with(key, false).await?;
        vacuum_into(db.pool(), dest, &key.name()).await
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
        self.restore_to_point(key, target)
            .await?
            .ok_or_else(|| FleetError::Restore {
                name: key.name(),
                detail: "the restore wrote no SQLite database".to_owned(),
            })
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
        self.inner.engine.count_applied(report.applied);
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
            .engine
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
        self.inner.engine.migrations.known_versions()
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
        self.inner.engine.count_applied(applied);
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
}

/// Record the database's name in the file on first use, and refuse a file
/// that records another name.
pub(super) async fn check_identity(
    pool: &Pool<RuntimeConnection>,
    name: &str,
) -> Result<(), FleetError> {
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

/// The epoch `path` was last opened under, 0 for none. Blocking.
pub(super) fn read_epoch_stamp(path: &Path, name: &str) -> Result<u64, FleetError> {
    use diesel::{OptionalExtension as _, RunQueryDsl as _};

    #[derive(diesel::QueryableByName)]
    struct Stamp {
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        epoch: i64,
    }

    let unavailable = |detail: String| FleetError::Unavailable {
        name: name.to_owned(),
        detail,
    };
    let mut conn = crate::db::establish_sqlite_migration_connection(&existing_only_url(path))
        .map_err(|e| unavailable(e.to_string()))?;
    let exists = diesel::sql_query(format!(
        "SELECT 0 AS epoch FROM sqlite_master WHERE type = 'table' AND name = '{EPOCH_TABLE}'"
    ))
    .get_result::<Stamp>(&mut conn)
    .optional()
    .map_err(|e| unavailable(e.to_string()))?
    .is_some();
    if !exists {
        return Ok(0);
    }
    let stamp = diesel::sql_query(format!("SELECT epoch FROM {EPOCH_TABLE} WHERE id = 1"))
        .get_result::<Stamp>(&mut conn)
        .optional()
        .map_err(|e| unavailable(e.to_string()))?;
    Ok(stamp.map_or(0, |s| u64::try_from(s.epoch).unwrap_or(0)))
}

/// Record that `path` is opened under `epoch`. Never lowers it. Blocking.
pub(super) fn write_epoch_stamp(path: &Path, name: &str, epoch: u64) -> Result<(), FleetError> {
    use diesel::RunQueryDsl as _;
    use diesel::connection::SimpleConnection as _;

    let unavailable = |detail: String| FleetError::Unavailable {
        name: name.to_owned(),
        detail,
    };
    let epoch = i64::try_from(epoch).map_err(|e| unavailable(e.to_string()))?;
    let mut conn = crate::db::establish_sqlite_migration_connection(&existing_only_url(path))
        .map_err(|e| unavailable(e.to_string()))?;
    conn.batch_execute(&format!(
        "CREATE TABLE IF NOT EXISTS {EPOCH_TABLE} (\
             id INTEGER PRIMARY KEY CHECK (id = 1), \
             epoch INTEGER NOT NULL)"
    ))
    .map_err(|e| unavailable(e.to_string()))?;
    diesel::sql_query(format!(
        "INSERT INTO {EPOCH_TABLE} (id, epoch) VALUES (1, ?) \
         ON CONFLICT (id) DO UPDATE SET epoch = max(epoch, excluded.epoch)"
    ))
    .bind::<diesel::sql_types::BigInt, _>(epoch)
    .execute(&mut conn)
    .map_err(|e| unavailable(e.to_string()))?;
    Ok(())
}

/// Record that `path` is opened under `epoch`. Refuse a file stamped
/// above `epoch`: a newer owner opened it. Blocking.
///
/// The read, the check and the write run in one `BEGIN IMMEDIATE`
/// transaction. So two processes that open the file at once cannot both
/// pass the check with different epochs.
pub(super) fn stamp_epoch(path: &Path, name: &str, epoch: u64) -> Result<(), FleetError> {
    use diesel::connection::SimpleConnection as _;
    use diesel::{OptionalExtension as _, RunQueryDsl as _};

    #[derive(diesel::QueryableByName)]
    struct Stamp {
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        epoch: i64,
    }

    let unavailable = |detail: String| FleetError::Unavailable {
        name: name.to_owned(),
        detail,
    };
    let wanted = i64::try_from(epoch).map_err(|e| unavailable(e.to_string()))?;
    let mut conn = crate::db::establish_sqlite_migration_connection(&existing_only_url(path))
        .map_err(|e| unavailable(e.to_string()))?;
    let found = conn
        .immediate_transaction(|conn| {
            conn.batch_execute(&format!(
                "CREATE TABLE IF NOT EXISTS {EPOCH_TABLE} (\
                     id INTEGER PRIMARY KEY CHECK (id = 1), \
                     epoch INTEGER NOT NULL)"
            ))?;
            let found = diesel::sql_query(format!("SELECT epoch FROM {EPOCH_TABLE} WHERE id = 1"))
                .get_result::<Stamp>(conn)
                .optional()?
                .map_or(0, |stamp| stamp.epoch);
            if found < wanted {
                diesel::sql_query(format!(
                    "INSERT INTO {EPOCH_TABLE} (id, epoch) VALUES (1, ?) \
                     ON CONFLICT (id) DO UPDATE SET epoch = max(epoch, excluded.epoch)"
                ))
                .bind::<diesel::sql_types::BigInt, _>(wanted)
                .execute(conn)?;
            }
            Ok::<_, diesel::result::Error>(found)
        })
        .map_err(|e| unavailable(e.to_string()))?;
    if found > wanted {
        return Err(FleetError::NewerEpoch {
            name: name.to_owned(),
            epoch,
            found: u64::try_from(found).unwrap_or(0),
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
pub(super) fn migrate_existing_blocking(
    fleet: &DatabaseFleet,
    key: &FleetDbKey,
) -> Result<usize, FleetError> {
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
        Ok(()) => fleet.inner.engine.migrations.apply(&url, &name),
        Err(_) if !path.exists() => Ok(0),
        Err(error) => Err(error),
    }
}

/// The identity row of the database file at `path`, read on a query-only
/// connection. For the restore check of a scratch file.
pub fn read_identity(path: &Path, name: &str) -> Result<Option<String>, FleetError> {
    inspect_blocking(path, name).map(|info| info.identity)
}

/// [`DatabaseFleet::inspect`] on a blocking, query-only connection.
pub(super) fn inspect_blocking(path: &Path, name: &str) -> Result<FleetDbInfo, FleetError> {
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
pub(super) fn existing_only_url(path: &Path) -> String {
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
        SqliteEngine::url_of(path)
    }
}

/// A connection string that opens `path` read-only, if the file exists. On
/// Unix a `file:` URI with `mode=ro`. Elsewhere the plain path: hold
/// [`pin_existing`] while it is open.
pub(super) fn read_only_url(path: &Path) -> String {
    #[cfg(unix)]
    {
        existing_only_url(path).replace("?mode=rw", "?mode=ro")
    }
    #[cfg(not(unix))]
    {
        SqliteEngine::url_of(path)
    }
}

/// Keep `path` from being removed or replaced while the caller opens it, on
/// Windows, where [`existing_only_url`] cannot refuse to create the file.
/// The handle shares read and write but not delete, so a removal or rename
/// fails while it lives; `SQLite` also opens without `FILE_SHARE_DELETE`, so
/// the file stays put for as long as the caller's connections hold it.
/// `None` when the file does not exist.
#[cfg(windows)]
pub(super) fn pin_existing(path: &Path) -> std::io::Result<Option<std::fs::File>> {
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
pub(super) const fn pin_existing(_path: &Path) -> std::io::Result<Option<()>> {
    Ok(Some(()))
}

/// [`check_identity`] on a blocking connection, for paths that migrate a file
/// without opening a pool ([`DatabaseFleet::migrate_all`]).
pub(super) fn check_identity_blocking(url: &str, name: &str) -> Result<(), FleetError> {
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
pub(super) fn exclusive_guard(
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
    let mut conn = crate::db::establish_sqlite_migration_connection(&SqliteEngine::url_of(path))
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
pub(super) const HOLD_GUARD_WHILE_REMOVING: bool = !cfg!(windows);

/// Run `work` once no other process has `path` open. Blocking. `work` gets
/// whether the guard found the file.
///
/// With `hold`, the guard stays open during `work`. Without it, the guard
/// closes first. Then another process can open the file before `work` runs,
/// and `work` must handle that. Windows refuses to delete an open file (see
/// [`removal_error`]). [`publish_restored`] refuses a file that appeared.
pub(super) fn with_exclusive_access<T>(
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

/// Link the staged copy to `path`. Never replace a file. If a file appeared
/// after the removal, return [`FleetError::AlreadyExists`].
pub(super) fn publish_restored(staging: &Path, path: &Path, name: &str) -> Result<(), FleetError> {
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
pub(super) fn restore_in_place(
    root: &Path,
    replication: &crate::db::fleet_replication::FleetReplication,
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
    let published = swap_in_staged(root, path, &name, &staging);
    discard_staging(&staging);
    published.map(|_| crate::replication::RestoreOutcome {
        output: path.to_path_buf(),
        ..outcome
    })
}

/// Put the checked `staging` file at `path` in place of the database there.
/// Blocking. Returns whether a database was there.
///
/// Under the path lock and the exclusive guard: the old sidecars go, then
/// the old main file, then the staging file is linked into place. Another
/// process that has the file open gives [`FleetError::InUseElsewhere`], and
/// nothing changes.
pub(super) fn swap_in_staged(
    root: &Path,
    path: &Path,
    name: &str,
    staging: &Path,
) -> Result<bool, FleetError> {
    with_path_lock(root, name, || {
        with_exclusive_access(path, name, HOLD_GUARD_WHILE_REMOVING, |present| {
            unlink_database_set(path, "replace database", name, present, |file| {
                std::fs::remove_file(file)
            })?;
            publish_restored(staging, path, name).map(|()| present)
        })
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
pub(super) fn create_database_file(
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
    let published = publish_staged(root, path, name, &staging);
    discard(&staging);
    match published? {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(io_error("publish database")(e)),
    }
}

/// Link a complete staging file to `path` under the path lock. The inner
/// result is the link: `AlreadyExists` when another file is there.
pub(super) fn publish_staged(
    root: &Path,
    path: &Path,
    name: &str,
    staging: &Path,
) -> Result<std::io::Result<()>, FleetError> {
    // The path lock is held only for the publish, not for the build.
    with_path_lock(root, name, || {
        // Under the lock nobody else can publish here. A path with no main
        // file holds no live database, so its stray sidecars are stale.
        if !path.exists() {
            unlink_database_set(path, "clear stray sidecars", name, false, |file| {
                std::fs::remove_file(file)
            })?;
        }
        Ok(std::fs::hard_link(staging, path))
    })
}

/// How [`stage_file`] checks and migrates a copy.
#[derive(Debug, Clone, Copy)]
pub(super) struct StageRules {
    /// The tag of the staging directory.
    pub(super) tag: &'static str,
    /// Accept a copy of another key, and write this key into its identity row.
    pub(super) rekey: bool,
    /// Apply pending migrations. If `false`, refuse a copy that has some.
    pub(super) migrate: bool,
}

/// A checked copy in its staging file. The caller discards the file.
#[derive(Debug)]
pub(super) struct Staged {
    /// The staging file.
    pub(super) file: PathBuf,
    /// Migrations applied to the copy.
    pub(super) applied: usize,
    /// The key the source recorded, when the copy was rekeyed.
    pub(super) rekeyed_from: Option<String>,
}

/// Copy `source` into a new staging file beside `path`, check it and
/// migrate it. Returns the staging file and the number of migrations
/// applied. Removes the staging file on an error.
pub(super) fn stage_import_file(
    path: &Path,
    name: &str,
    source: &Path,
    migrations: &FleetMigrations,
    epoch: Option<u64>,
) -> Result<(PathBuf, usize), FleetError> {
    let rules = StageRules {
        tag: "importing",
        rekey: false,
        migrate: true,
    };
    let invalid = |detail: String| FleetError::InvalidSource {
        name: name.to_owned(),
        path: source.display().to_string(),
        detail,
    };
    stage_file(path, name, source, migrations, epoch, rules, invalid)
        .map(|staged| (staged.file, staged.applied))
}

/// Copy `source` into a new staging file beside `path`, then check and
/// migrate it by `rules`. `invalid` makes the error for a refused source.
/// Removes the staging file on an error.
pub(super) fn stage_file(
    path: &Path,
    name: &str,
    source: &Path,
    migrations: &FleetMigrations,
    epoch: Option<u64>,
    rules: StageRules,
    invalid: impl Fn(String) -> FleetError,
) -> Result<Staged, FleetError> {
    if !source.is_file() {
        return Err(invalid("no such file".to_owned()));
    }
    let staging = staging_path(path, rules.tag)?;
    match stage_import(source, &staging, name, migrations, epoch, rules, invalid) {
        Ok((applied, rekeyed_from)) => Ok(Staged {
            file: staging,
            applied,
            rekeyed_from,
        }),
        Err(error) => {
            discard_staging(&staging);
            Err(error)
        }
    }
}

/// Copy `source` to `staging`, check the copy, and migrate it. Returns the
/// migrations applied and the key the source recorded, if rekeyed.
pub(super) fn stage_import(
    source: &Path,
    staging: &Path,
    name: &str,
    migrations: &FleetMigrations,
    epoch: Option<u64>,
    rules: StageRules,
    invalid: impl Fn(String) -> FleetError,
) -> Result<(usize, Option<String>), FleetError> {
    use diesel::RunQueryDsl as _;
    use diesel::connection::SimpleConnection as _;

    let unavailable = |detail: String| FleetError::Unavailable {
        name: name.to_owned(),
        detail,
    };
    let dest = staging
        .to_str()
        .ok_or_else(|| unavailable("staging path is not UTF-8".to_owned()))?;
    {
        let Some(_pin) = pin_existing(source).map_err(|e| invalid(e.to_string()))? else {
            return Err(invalid("no such file".to_owned()));
        };
        let mut conn = crate::db::establish_sqlite_migration_connection(&read_only_url(source))
            .map_err(|e| invalid(format!("cannot open it: {e}")))?;
        // Do not run functions from the source's views or triggers.
        conn.batch_execute("PRAGMA trusted_schema = OFF;")
            .map_err(|e| unavailable(e.to_string()))?;
        diesel::sql_query("VACUUM INTO ?")
            .bind::<diesel::sql_types::Text, _>(dest)
            .execute(&mut conn)
            .map_err(|e| invalid(format!("cannot copy it: {e}")))?;
    }
    let url = SqliteEngine::url_of(staging);
    {
        let mut conn = crate::db::establish_sqlite_migration_connection(&url)
            .map_err(|e| unavailable(e.to_string()))?;
        conn.batch_execute("PRAGMA trusted_schema = OFF;")
            .map_err(|e| unavailable(e.to_string()))?;
        crate::replication::sqlite::integrity_check(&mut conn)
            .map_err(|e| invalid(e.to_string()))?;
        check_fleet_tables(&mut conn, &unavailable, &invalid)?;
        // The copy has no history in this fleet: set its epoch, never keep
        // the source's.
        conn.batch_execute(&format!("DROP TABLE IF EXISTS {EPOCH_TABLE};"))
            .map_err(|e| unavailable(e.to_string()))?;
    }
    let rekeyed_from = match read_identity(staging, name)? {
        Some(found) if found == name => None,
        Some(found) if rules.rekey => {
            write_identity(staging, name)?;
            Some(found)
        }
        Some(found) => {
            return Err(invalid(format!("it is a copy of {found}")));
        }
        None => {
            return Err(invalid("its identity table is empty".to_owned()));
        }
    };
    let applied = if rules.migrate {
        migrations.apply(&url, name)?
    } else {
        match migrations.pending(&url, name)? {
            0 => 0,
            count => {
                return Err(FleetError::PendingMigrations {
                    name: name.to_owned(),
                    count,
                });
            }
        }
    };
    if let Some(epoch) = epoch {
        write_epoch_stamp(staging, name, epoch)?;
    }
    // A trigger on another table can change the identity row while the
    // migrations run. Read it again.
    if read_identity(staging, name)?.as_deref() != Some(name) {
        return Err(invalid(
            "its identity row changed while it was migrated".to_owned(),
        ));
    }
    Ok((applied, rekeyed_from))
}

/// Refuse a copy whose identity or epoch is not a plain table, that records
/// no identity table, or that has a trigger on these tables.
fn check_fleet_tables(
    conn: &mut diesel::SqliteConnection,
    unavailable: &impl Fn(String) -> FleetError,
    invalid: &impl Fn(String) -> FleetError,
) -> Result<(), FleetError> {
    use diesel::RunQueryDsl as _;

    #[derive(diesel::QueryableByName)]
    struct Kind {
        #[diesel(sql_type = diesel::sql_types::Text)]
        kind: String,
    }

    // The identity and epoch must be plain tables: a view with a trigger
    // could answer any name.
    let kinds: Vec<Kind> = diesel::sql_query(format!(
        "SELECT name || ':' || type AS kind FROM sqlite_master \
         WHERE name IN ('{IDENTITY_TABLE}', '{EPOCH_TABLE}')"
    ))
    .load(conn)
    .map_err(|e| unavailable(e.to_string()))?;
    if kinds.iter().any(|row| !row.kind.ends_with(":table")) {
        return Err(invalid(
            "its identity or epoch is not a plain table".to_owned(),
        ));
    }
    if !kinds
        .iter()
        .any(|row| row.kind == format!("{IDENTITY_TABLE}:table"))
    {
        return Err(invalid(
            "it records no identity row; only a file that a fleet made is accepted".to_owned(),
        ));
    }
    // A trigger on these tables could block or undo a write to them.
    let triggers: Vec<Kind> = diesel::sql_query(format!(
        "SELECT name AS kind FROM sqlite_master WHERE type = 'trigger' \
         AND lower(tbl_name) IN ('{IDENTITY_TABLE}', '{EPOCH_TABLE}')"
    ))
    .load(conn)
    .map_err(|e| unavailable(e.to_string()))?;
    if !triggers.is_empty() {
        return Err(invalid(
            "it has a trigger on its identity or epoch table".to_owned(),
        ));
    }
    Ok(())
}

/// Write `name` into the identity row of the file at `path`. Blocking.
/// Exactly one row must change.
fn write_identity(path: &Path, name: &str) -> Result<(), FleetError> {
    use diesel::RunQueryDsl as _;
    use diesel::connection::SimpleConnection as _;
    let unavailable = |detail: String| FleetError::Unavailable {
        name: name.to_owned(),
        detail,
    };
    let mut conn = crate::db::establish_sqlite_migration_connection(&SqliteEngine::url_of(path))
        .map_err(|e| unavailable(e.to_string()))?;
    conn.batch_execute("PRAGMA trusted_schema = OFF;")
        .map_err(|e| unavailable(e.to_string()))?;
    let changed = diesel::sql_query(format!("UPDATE {IDENTITY_TABLE} SET name = ? WHERE id = 1"))
        .bind::<diesel::sql_types::Text, _>(name)
        .execute(&mut conn)
        .map_err(|e| unavailable(e.to_string()))?;
    if changed != 1 {
        return Err(unavailable(format!(
            "the identity row did not change ({changed} rows)"
        )));
    }
    Ok(())
}

/// Identity row and migrations on a staging file, closed before it returns.
pub(super) fn build_staged_database(
    staging: &Path,
    name: &str,
    migrations: &FleetMigrations,
) -> Result<(), FleetError> {
    use diesel::RunQueryDsl as _;
    use diesel::connection::SimpleConnection as _;
    let url = SqliteEngine::url_of(staging);
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

/// The fleet built for an app at boot, from `[database.fleet]` and the app's
/// registered migrations (the control-plane framework set excluded).
pub fn build_for_app(
    config: &crate::config::AutumnConfig,
    resolved_control_url: Option<&str>,
    all: &Arc<Vec<crate::migrate::RegisteredMigrations>>,
) -> Result<Option<DatabaseFleet>, String> {
    let Some(fleet_config) = config.database.fleet.as_ref() else {
        return Ok(None);
    };
    let apply_on_open = crate::migrate::should_auto_apply(
        config.profile.as_deref(),
        config.database.auto_migrate,
        config.database.auto_migrate_in_production,
    );
    // SQLite has no schemas: a set for a Postgres schema must not reach a
    // fleet file, where it would run in the default namespace.
    if let Some((label, _, _, Some(schema))) = all.iter().find(|(_, set, scope, schema)| {
        schema.is_some() && crate::migrate::applied_scope(set, *scope).reaches_shards()
    }) {
        return Err(format!(
            "Failed to set up database.fleet: the migration set `{label}` targets the \
             Postgres schema `{schema}`, and a SQLite fleet has no schemas"
        ));
    }
    let fleet = DatabaseFleet::from_parts(
        fleet_config,
        config.database.connect_timeout_secs,
        FleetMigrations::new(all, apply_on_open),
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
pub(super) fn canonical_target(path: &Path) -> Option<PathBuf> {
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

/// Remove a database file, its sidecars, and the directories the template
/// made for it when they are now empty (never `root` itself). `Ok(false)`
/// when there was no file. The main file goes last (see
/// [`unlink_database_set`]).
pub(super) fn remove_database_files(
    path: &Path,
    root: &Path,
    name: &str,
    present: bool,
) -> Result<bool, FleetError> {
    unlink_database_set(path, "delete database", name, present, |file| {
        std::fs::remove_file(file)
    })?;
    prune_empty_dirs(path, root);
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
pub(super) fn unlink_database_set(
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
