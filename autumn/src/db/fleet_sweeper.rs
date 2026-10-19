//! The fleet sweeper (issue 3234, ADR 0019 §6).
//!
//! Background work on a fleet database runs only while the database is open.
//! The sweeper does the work of databases that nobody asks for:
//!
//! ```text
//! enqueue (outbox row, commit hook), in the tenant transaction:
//!   insert the row ──► mark (key, kind) in _autumn_fleet_pending (control)
//! sweeper start, in a worker process:
//!   derivation fingerprint changed ──► mark (key, backfill) for each file
//! sweeper pass, every interval:
//!   read the index ──► open each key in the background (never past max_open)
//!   drain hooks, relay the outbox, run the backfill
//!   under the tenant write lock: find what is left, clear the rest
//!   close the database again when nobody else used it
//! ```
//!
//! # Why the index stays correct
//!
//! A database and the control database are two files, so no transaction
//! covers both. An enqueue therefore marks the control database *after* its
//! insert and *before* its commit, while it holds the tenant write lock. The
//! sweeper clears a row only while it holds the same lock, after it found
//! nothing pending. The two never interleave, so committed work always has a
//! row. A rolled-back write can leave a row with no work: the next visit
//! clears it. The lock order is always tenant database, then control
//! database.
//!
//! Write outbox rows in a transaction. In autocommit mode the row commits
//! before the mark, and a crash between the two leaves it unmarked until the
//! database opens again.
//!
//! # Roles
//!
//! A worker process (`relay`) opens closed databases, relays their outbox
//! and runs derivation backfills. A web process only visits the databases it
//! has open, drains their hooks and clears rows that have no work left.

// autumn-determinism-gate: production code in this module must read time and
// mint identifiers through the framework's injected seams (ClockSource /
// Entropy), never `Instant::now()` / `Utc::now()` / `SystemTime::now()` /
// `Uuid::new_v4()` directly. See CONTRIBUTING.md "Determinism seam gate"
// (issue #1797). Justify exceptions with
// #[allow(clippy::disallowed_methods, reason = "…")] at the narrowest scope.
#![cfg_attr(not(test), deny(clippy::disallowed_methods))]

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::Duration;

use chrono::{DateTime, TimeZone as _, Utc};
use diesel::sql_types::{BigInt, Nullable, Text};
use diesel_async::RunQueryDsl as _;
use diesel_async::SimpleAsyncConnection as _;
use diesel_async::pooled_connection::deadpool::Pool;
use futures::StreamExt as _;
use scoped_futures::ScopedFutureExt as _;
use tokio_util::sync::CancellationToken;

use super::fleet::{DatabaseFleet, FleetDatabase, FleetError, FleetLifecycle};
use crate::db::RuntimeConnection;
use crate::fleet_layout::FleetDbKey;
use crate::time::ClockSource;
use crate::{AppState, AutumnError, AutumnResult};

/// The `/actuator/health` indicator of the sweeper.
pub const SWEEPER_INDICATOR: &str = "fleet:sweeper";

/// The index table on the control database.
const PENDING_TABLE: &str = "_autumn_fleet_pending";

/// Sweeper facts that outlive a process, on the control database.
const STATE_TABLE: &str = "_autumn_fleet_sweeper_state";

/// The `STATE_TABLE` row that records the derivation fingerprint.
const FINGERPRINT_STATE: &str = "derivations";

/// The DDL, applied at boot. Times are epoch milliseconds of the app clock.
const SCHEMA_SQL: &str = "\
CREATE TABLE IF NOT EXISTS _autumn_fleet_pending (
    key         TEXT    NOT NULL,
    kind        TEXT    NOT NULL,
    since       BIGINT  NOT NULL,
    visit_after BIGINT  NOT NULL DEFAULT 0,
    attempts    INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (key, kind)
);
CREATE INDEX IF NOT EXISTS idx_autumn_fleet_pending_due
    ON _autumn_fleet_pending (visit_after, since);
CREATE TABLE IF NOT EXISTS _autumn_fleet_sweeper_state (
    name  TEXT PRIMARY KEY,
    value TEXT NOT NULL
);";

/// Rows per transaction when many databases are marked at once. Small, so a
/// tenant enqueue never waits long for the control write lock.
const MARK_CHUNK: usize = 100;

/// Backfill batches per round. The connection goes back to the pool between
/// rounds, so a request never waits long for it.
const BACKFILL_BATCHES_PER_ROUND: usize = 8;

/// The longest wait before a database that still has work is visited again.
const MAX_BACKOFF: Duration = Duration::from_secs(300);

// ── Kinds ───────────────────────────────────────────────────────────────────

/// A kind of work a fleet database can have pending.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PendingKind {
    /// Outbox messages that are not sent yet.
    Outbox,
    /// Commit hooks that have not run yet.
    Hooks,
    /// A derivation backfill that is not complete.
    Backfill,
}

impl PendingKind {
    /// Every kind.
    pub const ALL: [Self; 3] = [Self::Outbox, Self::Hooks, Self::Backfill];

    /// The spelling in the index.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Outbox => "outbox",
            Self::Hooks => "hooks",
            Self::Backfill => "backfill",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == value)
    }
}

impl std::fmt::Display for PendingKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ── Index ───────────────────────────────────────────────────────────────────

/// One row of the index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingEntry {
    /// The database name, as [`FleetDbKey::name`] gives it.
    pub key: String,
    /// The kind of work.
    pub kind: PendingKind,
    /// When the row was written (app clock).
    pub since: DateTime<Utc>,
}

/// The size of the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backlog {
    /// Databases with at least one row.
    pub databases: usize,
    /// Rows.
    pub entries: usize,
    /// The oldest row.
    pub oldest: Option<DateTime<Utc>>,
}

#[derive(diesel::QueryableByName)]
struct EntryRow {
    #[diesel(sql_type = Text)]
    key: String,
    #[diesel(sql_type = Text)]
    kind: String,
    #[diesel(sql_type = BigInt)]
    since: i64,
}

#[derive(diesel::QueryableByName)]
struct CountRow {
    #[diesel(sql_type = BigInt)]
    n: i64,
}

#[derive(diesel::QueryableByName)]
struct BacklogRow {
    #[diesel(sql_type = BigInt)]
    databases: i64,
    #[diesel(sql_type = BigInt)]
    entries: i64,
    #[diesel(sql_type = Nullable<BigInt>)]
    oldest: Option<i64>,
}

#[derive(diesel::QueryableByName)]
struct ValueRow {
    #[diesel(sql_type = Text)]
    value: String,
}

/// The pending-work index on the control database: which fleet databases
/// have work, of which kind, since when.
#[derive(Clone)]
pub struct FleetPendingIndex {
    control: Pool<RuntimeConnection>,
    clock: Arc<dyn ClockSource>,
}

impl std::fmt::Debug for FleetPendingIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FleetPendingIndex").finish_non_exhaustive()
    }
}

fn index_error(context: &str, error: impl std::fmt::Display) -> AutumnError {
    AutumnError::service_unavailable_msg(format!("fleet pending index: {context}: {error}"))
}

fn from_millis(millis: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(millis)
        .single()
        .unwrap_or(DateTime::<Utc>::MIN_UTC)
}

impl FleetPendingIndex {
    /// An index on the control database behind `control`.
    #[must_use]
    pub fn new(control: Pool<RuntimeConnection>, clock: Arc<dyn ClockSource>) -> Self {
        Self { control, clock }
    }

    async fn conn(
        &self,
    ) -> AutumnResult<diesel_async::pooled_connection::deadpool::Object<RuntimeConnection>> {
        self.control
            .get()
            .await
            .map_err(|error| index_error("no connection", error))
    }

    /// Create the index tables when they do not exist.
    ///
    /// # Errors
    ///
    /// Returns an error when no connection is available or the DDL fails.
    pub async fn ensure_schema(&self) -> AutumnResult<()> {
        self.conn()
            .await?
            .batch_execute(SCHEMA_SQL)
            .await
            .map_err(|error| index_error("schema", error))
    }

    /// Record that database `key` has work of `kind`. Keeps an existing row
    /// and its time. Reads first, so a database that is marked already costs
    /// no write lock.
    ///
    /// # Errors
    ///
    /// Returns an error when the control database cannot be read or written.
    pub async fn mark(&self, key: &str, kind: PendingKind) -> AutumnResult<()> {
        let mut conn = self.conn().await?;
        let found: CountRow = diesel::sql_query(format!(
            "SELECT COUNT(*) AS n FROM {PENDING_TABLE} WHERE key = ? AND kind = ?"
        ))
        .bind::<Text, _>(key)
        .bind::<Text, _>(kind.as_str())
        .get_result(&mut *conn)
        .await
        .map_err(|error| index_error("read", error))?;
        if found.n > 0 {
            return Ok(());
        }
        self.insert(&mut conn, key, kind).await
    }

    async fn insert(
        &self,
        conn: &mut RuntimeConnection,
        key: &str,
        kind: PendingKind,
    ) -> AutumnResult<()> {
        diesel::sql_query(format!(
            "INSERT INTO {PENDING_TABLE} (key, kind, since) VALUES (?, ?, ?) \
             ON CONFLICT (key, kind) DO NOTHING"
        ))
        .bind::<Text, _>(key)
        .bind::<Text, _>(kind.as_str())
        .bind::<BigInt, _>(self.clock.now().timestamp_millis())
        .execute(conn)
        .await
        .map(drop)
        .map_err(|error| index_error("mark", error))
    }

    /// Mark many databases, in transactions of a few hundred rows.
    async fn mark_many(&self, keys: &[String], kind: PendingKind) -> AutumnResult<()> {
        for chunk in keys.chunks(MARK_CHUNK) {
            let mut conn = self.conn().await?;
            let chunk = chunk.to_vec();
            let index = self.clone();
            crate::db::scoped_immediate_transaction::<(), AutumnError, _>(&mut conn, |conn| {
                async move {
                    for key in &chunk {
                        index.insert(conn, key, kind).await?;
                    }
                    Ok(())
                }
                .scope_boxed()
            })
            .await?;
        }
        Ok(())
    }

    /// Rows a visit may take now (no backoff in force), oldest first: of the
    /// databases in `keys`, or else the oldest `limit`.
    async fn due(&self, keys: Option<&[String]>, limit: usize) -> AutumnResult<Vec<PendingEntry>> {
        let now = self.clock.now().timestamp_millis();
        let mut conn = self.conn().await?;
        let Some(keys) = keys else {
            let rows: Vec<EntryRow> = diesel::sql_query(format!(
                "SELECT key, kind, since FROM {PENDING_TABLE} WHERE visit_after <= ? \
                 ORDER BY since, key, kind LIMIT ?"
            ))
            .bind::<BigInt, _>(now)
            .bind::<BigInt, _>(i64::try_from(limit).unwrap_or(i64::MAX))
            .load(&mut *conn)
            .await
            .map_err(|error| index_error("read", error))?;
            return Ok(entries(rows));
        };
        let mut found = Vec::new();
        for chunk in keys.chunks(MARK_CHUNK) {
            let marks = vec!["?"; chunk.len()].join(", ");
            let mut query = diesel::sql_query(format!(
                "SELECT key, kind, since FROM {PENDING_TABLE} \
                 WHERE visit_after <= ? AND key IN ({marks}) ORDER BY since, key, kind"
            ))
            .into_boxed::<diesel::sqlite::Sqlite>()
            .bind::<BigInt, _>(now);
            for key in chunk {
                query = query.bind::<Text, _>(key.clone());
            }
            let rows: Vec<EntryRow> = query
                .load(&mut *conn)
                .await
                .map_err(|error| index_error("read", error))?;
            found.extend(entries(rows));
        }
        Ok(found)
    }

    /// Put off the next visit of `key`: 1 s, then twice as long each time, up
    /// to [`MAX_BACKOFF`]. Stored in the index, so a stuck database never
    /// hides the others, in this process or another.
    async fn defer(&self, key: &str) -> AutumnResult<()> {
        let max = i64::try_from(MAX_BACKOFF.as_millis()).unwrap_or(i64::MAX);
        diesel::sql_query(format!(
            "UPDATE {PENDING_TABLE} \
             SET visit_after = ? + MIN(?, 1000 * (1 << MIN(attempts, 9))), \
                 attempts = attempts + 1 \
             WHERE key = ?"
        ))
        .bind::<BigInt, _>(self.clock.now().timestamp_millis())
        .bind::<BigInt, _>(max)
        .bind::<Text, _>(key)
        .execute(&mut *self.conn().await?)
        .await
        .map(drop)
        .map_err(|error| index_error("defer", error))
    }

    /// End the backoff of `key`: a visit made progress.
    async fn undefer(&self, key: &str) -> AutumnResult<()> {
        diesel::sql_query(format!(
            "UPDATE {PENDING_TABLE} SET visit_after = 0, attempts = 0 \
             WHERE key = ? AND attempts > 0"
        ))
        .bind::<Text, _>(key)
        .execute(&mut *self.conn().await?)
        .await
        .map(drop)
        .map_err(|error| index_error("defer", error))
    }

    /// The oldest `limit` rows, oldest first.
    ///
    /// # Errors
    ///
    /// Returns an error when the control database cannot be read.
    pub async fn pending(&self, limit: usize) -> AutumnResult<Vec<PendingEntry>> {
        let mut conn = self.conn().await?;
        let rows: Vec<EntryRow> = diesel::sql_query(format!(
            "SELECT key, kind, since FROM {PENDING_TABLE} ORDER BY since, key, kind LIMIT ?"
        ))
        .bind::<BigInt, _>(i64::try_from(limit).unwrap_or(i64::MAX))
        .load(&mut *conn)
        .await
        .map_err(|error| index_error("read", error))?;
        Ok(entries(rows))
    }

    /// The rows of the databases in `keys`.
    ///
    /// # Errors
    ///
    /// Returns an error when the control database cannot be read.
    pub async fn pending_for(&self, keys: &[String]) -> AutumnResult<Vec<PendingEntry>> {
        let mut found = Vec::new();
        for chunk in keys.chunks(MARK_CHUNK) {
            let mut conn = self.conn().await?;
            let marks = vec!["?"; chunk.len()].join(", ");
            let mut query = diesel::sql_query(format!(
                "SELECT key, kind, since FROM {PENDING_TABLE} WHERE key IN ({marks}) \
                 ORDER BY since, key, kind"
            ))
            .into_boxed::<diesel::sqlite::Sqlite>();
            for key in chunk {
                query = query.bind::<Text, _>(key.clone());
            }
            let rows: Vec<EntryRow> = query
                .load(&mut *conn)
                .await
                .map_err(|error| index_error("read", error))?;
            found.extend(entries(rows));
        }
        Ok(found)
    }

    /// How many databases and rows the index holds, and its oldest row.
    ///
    /// # Errors
    ///
    /// Returns an error when the control database cannot be read.
    pub async fn backlog(&self) -> AutumnResult<Backlog> {
        let mut conn = self.conn().await?;
        let row: BacklogRow = diesel::sql_query(format!(
            "SELECT COUNT(DISTINCT key) AS databases, COUNT(*) AS entries, \
             MIN(since) AS oldest FROM {PENDING_TABLE}"
        ))
        .get_result(&mut *conn)
        .await
        .map_err(|error| index_error("read", error))?;
        Ok(Backlog {
            databases: usize::try_from(row.databases).unwrap_or(0),
            entries: usize::try_from(row.entries).unwrap_or(0),
            oldest: row.oldest.map(from_millis),
        })
    }

    /// Make the rows of `key` name exactly the kinds in `remaining`, but never
    /// remove a row of kind `keep`. Writes nothing when they already do.
    async fn reconcile(
        &self,
        key: &str,
        remaining: &BTreeSet<PendingKind>,
        keep: Option<PendingKind>,
    ) -> AutumnResult<()> {
        let mut conn = self.conn().await?;
        let rows: Vec<EntryRow> = diesel::sql_query(format!(
            "SELECT key, kind, since FROM {PENDING_TABLE} WHERE key = ?"
        ))
        .bind::<Text, _>(key)
        .load(&mut *conn)
        .await
        .map_err(|error| index_error("read", error))?;
        let marked: BTreeSet<PendingKind> = entries(rows).into_iter().map(|e| e.kind).collect();
        for kind in remaining.difference(&marked) {
            self.insert(&mut conn, key, *kind).await?;
        }
        for kind in marked
            .difference(remaining)
            .filter(|kind| Some(**kind) != keep)
        {
            diesel::sql_query(format!(
                "DELETE FROM {PENDING_TABLE} WHERE key = ? AND kind = ?"
            ))
            .bind::<Text, _>(key)
            .bind::<Text, _>(kind.as_str())
            .execute(&mut *conn)
            .await
            .map_err(|error| index_error("clear", error))?;
        }
        Ok(())
    }

    /// Remove every row of database `key` (it was deleted).
    ///
    /// # Errors
    ///
    /// Returns an error when the control database cannot be written.
    pub async fn clear_database(&self, key: &str) -> AutumnResult<()> {
        diesel::sql_query(format!("DELETE FROM {PENDING_TABLE} WHERE key = ?"))
            .bind::<Text, _>(key)
            .execute(&mut *self.conn().await?)
            .await
            .map(drop)
            .map_err(|error| index_error("clear", error))
    }

    async fn state_value(&self, name: &str) -> AutumnResult<Option<String>> {
        let rows: Vec<ValueRow> =
            diesel::sql_query(format!("SELECT value FROM {STATE_TABLE} WHERE name = ?"))
                .bind::<Text, _>(name)
                .load(&mut *self.conn().await?)
                .await
                .map_err(|error| index_error("read", error))?;
        Ok(rows.into_iter().next().map(|row| row.value))
    }

    async fn set_state_value(&self, name: &str, value: &str) -> AutumnResult<()> {
        diesel::sql_query(format!(
            "INSERT INTO {STATE_TABLE} (name, value) VALUES (?, ?) \
             ON CONFLICT (name) DO UPDATE SET value = excluded.value"
        ))
        .bind::<Text, _>(name)
        .bind::<Text, _>(value)
        .execute(&mut *self.conn().await?)
        .await
        .map(drop)
        .map_err(|error| index_error("write", error))
    }
}

fn entries(rows: Vec<EntryRow>) -> Vec<PendingEntry> {
    rows.into_iter()
        .filter_map(|row| {
            Some(PendingEntry {
                kind: PendingKind::parse(&row.kind)?,
                key: row.key,
                since: from_millis(row.since),
            })
        })
        .collect()
}

// ── Marking from a connection ───────────────────────────────────────────────

/// The indexes of the fleets in this process, by fleet root. Enqueue code
/// has only a connection, so it finds its index here.
static REGISTRY: RwLock<Vec<(PathBuf, Weak<FleetPendingIndex>)>> = RwLock::new(Vec::new());

/// How many entries `REGISTRY` holds. `0` lets an app with no fleet skip the
/// lookup.
static REGISTERED: AtomicUsize = AtomicUsize::new(0);

fn register(root: &Path, index: &Arc<FleetPendingIndex>) {
    let mut registry = REGISTRY
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    registry.retain(|(_, index)| index.strong_count() > 0);
    registry.push((normalized(root), Arc::downgrade(index)));
    REGISTERED.store(registry.len(), Ordering::Release);
}

/// The index of the fleet whose root holds `file`. On Windows, with one
/// fleet in the process, that fleet: a path spelled two ways must not lose a
/// mark.
fn lookup(file: &Path) -> Option<Arc<FleetPendingIndex>> {
    let file = normalized(file);
    let live: Vec<(PathBuf, Arc<FleetPendingIndex>)> = REGISTRY
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .filter_map(|(root, index)| Some((root.clone(), index.upgrade()?)))
        .collect();
    if let Some((_, index)) = live.iter().find(|(root, _)| file.starts_with(root)) {
        return Some(Arc::clone(index));
    }
    #[cfg(windows)]
    if let [(_, only)] = live.as_slice() {
        return Some(Arc::clone(only));
    }
    None
}

/// `path` without the Windows verbatim prefix, which `canonicalize` adds and
/// `SQLite` may not report.
fn normalized(path: &Path) -> PathBuf {
    path.to_str()
        .and_then(|text| text.strip_prefix(r"\\?\"))
        .map_or_else(|| path.to_path_buf(), PathBuf::from)
}

/// Mark the fleet database behind `conn` as holding work of `kind`. Call it
/// after the insert that enqueues the work, in the same transaction. A no-op
/// when `conn` is not a fleet database, or no fleet in this process has a
/// sweeper.
///
/// # Errors
///
/// Returns an error when the control database cannot be written. The caller
/// fails its write, so no work is left without a row.
pub(crate) async fn mark_connection(
    conn: &mut RuntimeConnection,
    kind: PendingKind,
) -> AutumnResult<()> {
    if AtomicUsize::load(&REGISTERED, Ordering::Acquire) == 0 {
        return Ok(());
    }
    let Some(file) = main_file(conn).await? else {
        return Ok(());
    };
    let Some(index) = lookup(&file) else {
        return Ok(());
    };
    let Some(name) = fleet_identity(conn).await? else {
        return Ok(());
    };
    index.mark(&name, kind).await
}

/// The file of the connection's main database. `None` for an in-memory one.
async fn main_file(conn: &mut RuntimeConnection) -> AutumnResult<Option<PathBuf>> {
    let rows: Vec<ValueRow> =
        diesel::sql_query("SELECT file AS value FROM pragma_database_list WHERE name = 'main'")
            .load(conn)
            .await
            .map_err(|error| index_error("database list", error))?;
    Ok(rows
        .into_iter()
        .next()
        .filter(|row| !row.value.is_empty())
        .map(|row| PathBuf::from(row.value)))
}

/// The name a fleet database records for itself. `None` for any other
/// database.
async fn fleet_identity(conn: &mut RuntimeConnection) -> AutumnResult<Option<String>> {
    let table: CountRow = diesel::sql_query(
        "SELECT COUNT(*) AS n FROM sqlite_master \
         WHERE type = 'table' AND name = '_autumn_fleet_identity'",
    )
    .get_result(&mut *conn)
    .await
    .map_err(|error| index_error("identity", error))?;
    if table.n == 0 {
        return Ok(None);
    }
    let rows: Vec<ValueRow> =
        diesel::sql_query("SELECT name AS value FROM _autumn_fleet_identity WHERE id = 1")
            .load(conn)
            .await
            .map_err(|error| index_error("identity", error))?;
    Ok(rows.into_iter().next().map(|row| row.value))
}

// ── Derivations ─────────────────────────────────────────────────────────────

/// Reconcile the derivations of one database and run up to `rounds` rounds
/// of its backfill. Returns whether every backfill is complete, and how many
/// batches ran.
async fn backfill(
    pool: &Pool<RuntimeConnection>,
    rounds: usize,
    stop: &CancellationToken,
) -> AutumnResult<(bool, usize)> {
    let pool_error = |error| index_error("no connection to the fleet database", error);
    {
        let mut conn = pool.get().await.map_err(pool_error)?;
        if crate::derivation::derivations_settled(&mut conn).await? {
            return Ok((true, 0));
        }
        crate::derivation::ensure_derivations(&mut conn).await?;
    }
    let options = crate::derivation::BackfillOptions {
        max_batches: Some(BACKFILL_BATCHES_PER_ROUND),
        ..crate::derivation::BackfillOptions::default()
    };
    let mut batches = 0;
    for _ in 0..rounds {
        if stop.is_cancelled() {
            break;
        }
        let mut conn = pool.get().await.map_err(pool_error)?;
        let report = crate::derivation::run_backfill(&mut conn, &options).await?;
        batches += report.batches_run;
        if report.in_progress.is_empty() {
            return Ok((true, batches));
        }
        // No checkpoint moved: a further round would do the same.
        if report.batches_run == 0 {
            break;
        }
    }
    Ok((false, batches))
}

// ── Lifecycle ───────────────────────────────────────────────────────────────

/// The outbox and hook work a database holds when it opens. Reads only.
async fn pending_on_open(pool: &Pool<RuntimeConnection>) -> AutumnResult<Vec<PendingKind>> {
    let mut conn = pool
        .get()
        .await
        .map_err(|error| index_error("no connection to the fleet database", error))?;
    let mut found = Vec::new();
    if crate::outbox::has_pending(&mut conn).await? {
        found.push(PendingKind::Outbox);
    }
    if crate::repository_commit_hooks::has_pending_hooks(&mut conn).await? {
        found.push(PendingKind::Hooks);
    }
    Ok(found)
}

/// The per-database part of the sweeper, in every process role.
struct SweeperLifecycle {
    index: Arc<FleetPendingIndex>,
    /// The outbox relay is installed: each database needs its tables.
    outbox: bool,
}

impl FleetLifecycle for SweeperLifecycle {
    fn on_open(&self, db: &FleetDatabase) -> Result<(), String> {
        let handle = tokio::runtime::Handle::current();
        if self.outbox {
            // Before the first use: an outbox write needs the table.
            handle
                .block_on(crate::outbox::ensure_schema(db.pool()))
                .map_err(|error| error.to_string())?;
        }
        // Work an earlier process left without a row (a restore from a
        // replica, a crash after an autocommit write) gets one now.
        let found = handle.block_on(pending_on_open(db.pool()));
        for kind in found.unwrap_or_default() {
            if let Err(error) = handle.block_on(self.index.mark(&db.key().name(), kind)) {
                tracing::warn!(database = %db.key(), %error, "could not mark fleet work");
            }
        }
        if crate::derivation::has_derivation_descriptors() {
            // One bounded round, so a request never reads a derived column
            // that a missed backfill left wrong (a new database, or one
            // restored from an older replica). The sweeper does the rest.
            let settled = handle.block_on(backfill(db.pool(), 1, &CancellationToken::new()));
            if !matches!(settled, Ok((true, _))) {
                if let Err(error) = &settled {
                    tracing::warn!(database = %db.key(), %error, "fleet derivation backfill failed");
                }
                let name = db.key().name();
                if let Err(error) = handle.block_on(self.index.mark(&name, PendingKind::Backfill)) {
                    tracing::warn!(database = %name, %error, "could not mark a fleet backfill");
                }
            }
        }
        Ok(())
    }
}

// ── Sweeper ─────────────────────────────────────────────────────────────────

/// How the sweeper paces itself.
#[derive(Debug, Clone)]
pub struct FleetSweeperOptions {
    /// Time between passes. Default: 1 s.
    pub interval: Duration,
    /// Databases worked on at once. Default: 4.
    pub concurrency: usize,
    /// Databases visited per pass, at most. Default: 64.
    pub max_databases_per_pass: usize,
    /// Outbox messages, and commit hooks, per database per visit. Default: 256.
    pub max_messages_per_database: usize,
    /// Backfill rounds per database per visit. Default: 4.
    pub max_backfill_rounds: usize,
    /// A closed database's hooks are left alone this long, as the process
    /// that has it open drains them. Default: 5 s.
    pub hook_grace: Duration,
    /// Open closed databases, relay the outbox and run backfills: a worker
    /// process. `false` visits only the databases this process has open.
    /// Default: `true`.
    pub relay: bool,
}

impl Default for FleetSweeperOptions {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(1),
            concurrency: 4,
            max_databases_per_pass: 64,
            max_messages_per_database: 256,
            max_backfill_rounds: 4,
            hook_grace: Duration::from_secs(5),
            relay: true,
        }
    }
}

/// What one pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct SweepReport {
    /// Databases worked on.
    pub visited: usize,
    /// Databases left with no rows.
    pub cleared: usize,
    /// Databases not opened: the fleet had no room.
    pub deferred: usize,
    /// Databases whose visit failed.
    pub failed: usize,
    /// Outbox messages, hooks and backfill batches done.
    pub handled: usize,
}

/// How one visit ended.
enum Visit {
    Done { cleared: bool, handled: usize },
    Deferred,
    Failed,
    Skipped,
}

struct Inner {
    state: AppState,
    fleet: DatabaseFleet,
    index: Arc<FleetPendingIndex>,
    options: FleetSweeperOptions,
    stop: CancellationToken,
    fingerprint_checked: AtomicBool,
    passes: Arc<AtomicU64>,
    last: Arc<Mutex<SweepReport>>,
}

/// Visits fleet databases with pending work. See the module docs.
#[derive(Clone)]
pub struct FleetSweeper {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for FleetSweeper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FleetSweeper")
            .field("options", &self.inner.options)
            .finish_non_exhaustive()
    }
}

/// Marks a state that has a sweeper, so a second install is refused.
struct SweeperInstalled;

impl FleetSweeper {
    /// [`install_with`](Self::install_with) the default options.
    ///
    /// # Errors
    ///
    /// See [`install_with`](Self::install_with).
    pub async fn install(state: &AppState) -> AutumnResult<Option<Self>> {
        Self::install_with(state, FleetSweeperOptions::default()).await
    }

    /// Set up the sweeper of the app's fleet: create the index tables, let
    /// enqueue code mark its database, add the per-database lifecycle and
    /// register the `fleet:sweeper` health indicator. Install before the
    /// first database opens. `None` when the app has no fleet.
    ///
    /// # Errors
    ///
    /// Returns an error when the app has no control database, the index
    /// tables cannot be created, or a sweeper is installed already.
    pub async fn install_with(
        state: &AppState,
        options: FleetSweeperOptions,
    ) -> AutumnResult<Option<Self>> {
        let Some(fleet) = state
            .shards()
            .and_then(crate::sharding::ShardSet::fleet)
            .cloned()
        else {
            return Ok(None);
        };
        if state.extension::<SweeperInstalled>().is_some() {
            return Err(AutumnError::internal_server_error_msg(
                "the fleet sweeper is installed already",
            ));
        }
        let control = state.pool().cloned().ok_or_else(|| {
            AutumnError::internal_server_error_msg("the fleet sweeper needs a control database")
        })?;
        let index = Arc::new(FleetPendingIndex::new(control, state.clock_arc()));
        index.ensure_schema().await?;
        state.insert_extension(SweeperInstalled);
        register(fleet.root(), &index);
        fleet.add_lifecycle(Arc::new(SweeperLifecycle {
            index: Arc::clone(&index),
            outbox: state.extension::<crate::outbox::OutboxRelay>().is_some(),
        }));
        let last = Arc::new(Mutex::new(SweepReport::default()));
        let passes = Arc::new(AtomicU64::new(0));
        let sweeper = Self {
            inner: Arc::new(Inner {
                state: state.clone(),
                fleet,
                index: Arc::clone(&index),
                options,
                stop: CancellationToken::new(),
                fingerprint_checked: AtomicBool::new(false),
                passes: Arc::clone(&passes),
                last: Arc::clone(&last),
            }),
        };
        let indicator = SweeperHealth {
            index,
            last,
            passes,
            relay: sweeper.inner.options.relay,
            clock: state.clock_arc(),
        };
        if let Err(error) = state.health_indicator_registry.register(
            SWEEPER_INDICATOR,
            crate::actuator::IndicatorGroup::HealthOnly,
            Arc::new(indicator),
        ) {
            tracing::warn!("{error}");
        }
        Ok(Some(sweeper))
    }

    /// The index this sweeper reads.
    #[must_use]
    pub fn index(&self) -> &FleetPendingIndex {
        &self.inner.index
    }

    /// Run passes every `interval` until `shutdown` is cancelled. A pass in
    /// progress stops between messages and between hooks.
    #[must_use = "await the handle at shutdown so the last pass ends before the fleet closes"]
    pub fn spawn(&self, shutdown: CancellationToken) -> tokio::task::JoinHandle<()> {
        let sweeper = self.clone();
        let stop = self.inner.stop.clone();
        let watch = shutdown.clone();
        tokio::spawn(async move {
            watch.cancelled().await;
            stop.cancel();
        });
        tokio::spawn(async move {
            let interval = sweeper.inner.options.interval;
            loop {
                tokio::select! {
                    biased;
                    () = shutdown.cancelled() => break,
                    () = tokio::time::sleep(interval) => {}
                }
                let report = sweeper.sweep_once().await;
                if report.failed > 0 {
                    tracing::warn!(?report, "fleet sweeper pass had failures");
                }
            }
        })
    }

    /// Run one pass now. Tests and tools call it; [`spawn`](Self::spawn)
    /// calls it in a loop.
    pub async fn sweep_once(&self) -> SweepReport {
        let inner = &self.inner;
        if inner.options.relay
            && let Err(error) = self.mark_derivation_changes().await
        {
            tracing::warn!(%error, "fleet sweeper could not mark derivation backfills");
        }
        let mut report = SweepReport::default();
        let backfill_current = match self.backfill_current().await {
            Ok(current) => current,
            Err(error) => {
                tracing::warn!(%error, "fleet sweeper could not read the derivation fingerprint");
                false
            }
        };
        match self.candidates().await {
            Ok(candidates) => {
                let concurrency = inner.options.concurrency.max(1);
                let visits: Vec<Visit> = futures::stream::iter(candidates)
                    .map(|key| self.visit(key, backfill_current))
                    .buffer_unordered(concurrency)
                    .collect()
                    .await;
                for visit in visits {
                    match visit {
                        Visit::Done { cleared, handled } => {
                            report.visited += 1;
                            report.cleared += usize::from(cleared);
                            report.handled += handled;
                        }
                        Visit::Deferred => report.deferred += 1,
                        Visit::Failed => report.failed += 1,
                        Visit::Skipped => {}
                    }
                }
            }
            Err(error) => {
                tracing::warn!(%error, "fleet sweeper could not read the index");
                report.failed += 1;
            }
        }
        inner.passes.fetch_add(1, Ordering::Relaxed);
        *inner
            .last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = report;
        report
    }

    /// Whether the control database records this binary's derivations. Only
    /// then may a visit clear a `backfill` row: during a rolling deploy an old
    /// binary does not know the new derivations, and would clear their rows.
    async fn backfill_current(&self) -> AutumnResult<bool> {
        let stored = self.inner.index.state_value(FINGERPRINT_STATE).await?;
        Ok(stored == crate::derivation::registry_fingerprint())
    }

    /// When the derivations this binary declares changed since the last
    /// sweeper recorded them, mark every database for a backfill. Lists
    /// files; opens none.
    async fn mark_derivation_changes(&self) -> AutumnResult<()> {
        let inner = &self.inner;
        if AtomicBool::load(&inner.fingerprint_checked, Ordering::Acquire) {
            return Ok(());
        }
        if let Some(fingerprint) = crate::derivation::registry_fingerprint()
            && inner.index.state_value(FINGERPRINT_STATE).await?.as_deref()
                != Some(fingerprint.as_str())
        {
            let keys: Vec<String> = inner
                .fleet
                .list()
                .await?
                .iter()
                .map(FleetDbKey::name)
                .collect();
            inner.index.mark_many(&keys, PendingKind::Backfill).await?;
            inner
                .index
                .set_state_value(FINGERPRINT_STATE, &fingerprint)
                .await?;
            tracing::info!(
                databases = keys.len(),
                "derivations changed; fleet backfill enqueued"
            );
        }
        inner.fingerprint_checked.store(true, Ordering::Release);
        Ok(())
    }

    /// The databases to visit this pass: the ones open here first, then (in
    /// a worker) those of the oldest rows. Rows in backoff are not read. A
    /// worker leaves young hook rows of a closed database alone; a web process
    /// takes only hook rows, as it relays nothing.
    async fn candidates(&self) -> AutumnResult<Vec<String>> {
        let inner = &self.inner;
        if inner.stop.is_cancelled() {
            return Ok(Vec::new());
        }
        let open: BTreeSet<String> = inner
            .fleet
            .open_keys()
            .iter()
            .map(FleetDbKey::name)
            .collect();
        let open_list: Vec<String> = open.iter().cloned().collect();
        let mut rows = inner.index.due(Some(&open_list), 0).await?;
        let relay = inner.options.relay;
        if relay {
            let limit = inner
                .options
                .max_databases_per_pass
                .saturating_mul(4)
                .max(1);
            rows.extend(inner.index.due(None, limit).await?);
        }
        let now = inner.state.clock().now();
        let grace = chrono::Duration::from_std(inner.options.hook_grace)
            .unwrap_or_else(|_| chrono::Duration::zero());
        let mut chosen: Vec<String> = Vec::new();
        for entry in rows {
            let hooks = entry.kind == PendingKind::Hooks;
            let young_hooks = hooks && now - entry.since < grace;
            let wanted = if relay {
                !young_hooks || open.contains(&entry.key)
            } else {
                hooks
            };
            if wanted && !chosen.contains(&entry.key) {
                chosen.push(entry.key);
            }
        }
        chosen.truncate(inner.options.max_databases_per_pass.max(1));
        Ok(chosen)
    }

    async fn visit(&self, name: String, backfill_current: bool) -> Visit {
        let inner = &self.inner;
        // A pass in progress at shutdown opens nothing more.
        if inner.stop.is_cancelled() {
            return Visit::Skipped;
        }
        let Ok(key) = inner.fleet.key_for_name(&name) else {
            // Not a database of this fleet: drop the stray rows.
            return self.forget_database(&name).await;
        };
        let (db, opened) = if inner.options.relay {
            match inner.fleet.open_background(&key).await {
                Ok(Some(open)) => (open.db, open.opened),
                Ok(None) => return Visit::Deferred,
                // The file is gone: nothing can be pending in it.
                Err(FleetError::NotFound { .. }) => return self.forget_database(&name).await,
                Err(FleetError::Deleting { .. }) => return Visit::Skipped,
                Err(error) => {
                    tracing::warn!(database = %name, %error, "fleet sweeper could not open a database");
                    self.back_off(&name).await;
                    return Visit::Failed;
                }
            }
        } else {
            match inner.fleet.get_open(&key) {
                Some(db) => (db, false),
                None => return Visit::Skipped,
            }
        };
        let worked = self.work(&db, &key).await;
        let remaining = self.settle(&db, &name, backfill_current).await;
        drop(db);
        // Also when an earlier visit opened it and could not close it then.
        if opened || inner.options.relay {
            let _ = inner.fleet.close_if_unused(&key);
        }
        match (worked, remaining) {
            (Ok(handled), Ok(remaining)) => {
                let cleared = remaining.is_empty();
                if cleared || handled > 0 {
                    self.forget_backoff(&name).await;
                } else {
                    self.back_off(&name).await;
                }
                Visit::Done { cleared, handled }
            }
            (worked, remaining) => {
                for error in [worked.err(), remaining.err()].into_iter().flatten() {
                    tracing::warn!(database = %name, %error, "fleet sweeper visit failed");
                }
                self.back_off(&name).await;
                Visit::Failed
            }
        }
    }

    /// Drain hooks, relay the outbox, run the backfill. Returns how much it
    /// did.
    async fn work(&self, db: &FleetDatabase, key: &FleetDbKey) -> AutumnResult<usize> {
        let inner = &self.inner;
        let options = &inner.options;
        let pool = db.pool();
        #[cfg(feature = "ws")]
        let channels = Some(inner.state.channels().clone());
        #[cfg(not(feature = "ws"))]
        let channels = None;
        let mut handled = crate::repository_commit_hooks::sweep_fleet_database_hooks(
            pool,
            channels,
            options.max_messages_per_database,
            &inner.stop,
        )
        .await;
        if !options.relay {
            return Ok(handled);
        }
        if inner
            .state
            .extension::<crate::outbox::OutboxRelay>()
            .is_some()
        {
            let tenant = match key {
                FleetDbKey::Tenant { id, .. } => Some(id.as_str().to_owned()),
                FleetDbKey::Slot(_) => None,
            };
            handled += crate::outbox::relay_fleet_database(
                &inner.state,
                pool,
                options.max_messages_per_database,
                &inner.stop,
                tenant,
            )
            .await?;
        }
        // Cheap when every backfill is complete: one read.
        let (_, batches) = backfill(pool, options.max_backfill_rounds, &inner.stop).await?;
        Ok(handled + batches)
    }

    /// Under the tenant write lock, find the kinds of work the database still
    /// has and make its rows say exactly that.
    async fn settle(
        &self,
        db: &FleetDatabase,
        name: &str,
        backfill_current: bool,
    ) -> AutumnResult<BTreeSet<PendingKind>> {
        let index = Arc::clone(&self.inner.index);
        let name = name.to_owned();
        let mut conn = db
            .pool()
            .get()
            .await
            .map_err(|error| index_error("no connection to the fleet database", error))?;
        crate::db::scoped_immediate_transaction::<_, AutumnError, _>(&mut conn, |conn| {
            async move {
                let mut remaining = BTreeSet::new();
                if crate::outbox::has_pending(conn).await? {
                    remaining.insert(PendingKind::Outbox);
                }
                if crate::repository_commit_hooks::has_pending_hooks(conn).await? {
                    remaining.insert(PendingKind::Hooks);
                }
                if !crate::derivation::derivations_settled(conn).await? {
                    remaining.insert(PendingKind::Backfill);
                }
                // Still under the tenant write lock: no enqueue interleaves.
                let keep = (!backfill_current).then_some(PendingKind::Backfill);
                index.reconcile(&name, &remaining, keep).await?;
                Ok(remaining)
            }
            .scope_boxed()
        })
        .await
    }

    /// Drop the rows of a database that does not exist. When a database
    /// appears at the key meanwhile (a new provision), mark it again: its work
    /// must not lose its rows.
    async fn forget_database(&self, name: &str) -> Visit {
        let index = &self.inner.index;
        let cleared = index.clear_database(name).await;
        let reappeared = self
            .inner
            .fleet
            .key_for_name(name)
            .is_ok_and(|key| self.inner.fleet.exists(&key));
        let remarked = if reappeared {
            futures::future::try_join_all(PendingKind::ALL.map(|kind| index.mark(name, kind)))
                .await
                .map(drop)
        } else {
            Ok(())
        };
        match cleared.and(remarked) {
            Ok(()) => Visit::Done {
                cleared: !reappeared,
                handled: 0,
            },
            Err(error) => {
                tracing::warn!(database = %name, %error, "fleet sweeper could not clear rows");
                Visit::Failed
            }
        }
    }

    /// A visit left work and did nothing: wait before the next one. Only a
    /// worker defers, as only a worker can do all the work.
    async fn back_off(&self, name: &str) {
        if !self.inner.options.relay {
            return;
        }
        if let Err(error) = self.inner.index.defer(name).await {
            tracing::warn!(database = %name, %error, "fleet sweeper could not defer a database");
        }
    }

    async fn forget_backoff(&self, name: &str) {
        if let Err(error) = self.inner.index.undefer(name).await {
            tracing::warn!(database = %name, %error, "fleet sweeper could not reset a backoff");
        }
    }
}

// ── Health ──────────────────────────────────────────────────────────────────

/// `fleet:sweeper`: the backlog and the oldest pending age, read live, and
/// the counts of the last pass.
struct SweeperHealth {
    index: Arc<FleetPendingIndex>,
    last: Arc<Mutex<SweepReport>>,
    passes: Arc<AtomicU64>,
    relay: bool,
    clock: Arc<dyn ClockSource>,
}

impl crate::actuator::HealthIndicator for SweeperHealth {
    fn check(&self) -> futures::future::BoxFuture<'_, crate::actuator::HealthCheckOutput> {
        Box::pin(async move {
            use crate::actuator::HealthCheckOutput;
            let last = *self
                .last
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut details: HashMap<String, serde_json::Value> = HashMap::new();
            details.insert("relay".to_owned(), self.relay.into());
            details.insert(
                "passes_total".to_owned(),
                AtomicU64::load(&self.passes, Ordering::Relaxed).into(),
            );
            details.insert(
                "last_pass".to_owned(),
                serde_json::to_value(last).unwrap_or_default(),
            );
            match self.index.backlog().await {
                Ok(backlog) => {
                    details.insert("backlog".to_owned(), backlog.databases.into());
                    details.insert("entries".to_owned(), backlog.entries.into());
                    let age = backlog.oldest.map(|oldest| {
                        u64::try_from((self.clock.now() - oldest).num_seconds()).unwrap_or(0)
                    });
                    details.insert("oldest_pending_secs".to_owned(), age.into());
                    HealthCheckOutput::up().with_details(details)
                }
                Err(error) => {
                    details.insert("error".to_owned(), error.to_string().into());
                    HealthCheckOutput::down().with_details(details)
                }
            }
        })
    }

    fn group(&self) -> crate::actuator::IndicatorGroup {
        crate::actuator::IndicatorGroup::HealthOnly
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DatabaseFleetConfig;
    use crate::fleet_layout::FleetMode;

    fn control(dir: &Path) -> Pool<RuntimeConnection> {
        crate::db::create_pool(&crate::config::DatabaseConfig {
            url: Some(format!("sqlite://{}", dir.join("control.db").display())),
            ..Default::default()
        })
        .unwrap()
        .unwrap()
    }

    async fn index(dir: &Path, at: i64) -> FleetPendingIndex {
        let clock: Arc<dyn ClockSource> = Arc::new(crate::time::FixedClock::at(from_millis(at)));
        let index = FleetPendingIndex::new(control(dir), clock);
        index.ensure_schema().await.unwrap();
        index
    }

    fn fleet(dir: &Path) -> DatabaseFleet {
        DatabaseFleet::builder(DatabaseFleetConfig {
            mode: FleetMode::Slot,
            root: dir.join("fleet").display().to_string(),
            path: None,
            max_open: 4,
            pool_size: 2,
            create_on_demand: None,
            idle_close_secs: 0,
            restore_missing: false,
        })
        .min_residency(Duration::ZERO)
        .framework_migrations()
        .build()
        .unwrap()
    }

    #[test]
    fn kinds_round_trip_through_their_spelling() {
        for kind in PendingKind::ALL {
            assert_eq!(PendingKind::parse(kind.as_str()), Some(kind));
            assert_eq!(kind.to_string(), kind.as_str());
        }
        assert_eq!(PendingKind::parse("jobs"), None);
    }

    #[tokio::test]
    async fn a_mark_keeps_the_first_time_and_a_reconcile_writes_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let index = index(tmp.path(), 1_000).await;
        index.mark("slot:00001", PendingKind::Outbox).await.unwrap();
        // A later mark of the same kind keeps the row and its time.
        let later = FleetPendingIndex::new(
            index.control.clone(),
            Arc::new(crate::time::FixedClock::at(from_millis(9_000))),
        );
        later.mark("slot:00001", PendingKind::Outbox).await.unwrap();
        later.mark("slot:00002", PendingKind::Hooks).await.unwrap();
        let rows = index.pending(10).await.unwrap();
        assert_eq!(
            rows.iter()
                .map(|e| (e.key.as_str(), e.kind, e.since.timestamp_millis()))
                .collect::<Vec<_>>(),
            vec![
                ("slot:00001", PendingKind::Outbox, 1_000),
                ("slot:00002", PendingKind::Hooks, 9_000)
            ],
            "oldest first"
        );
        let backlog = index.backlog().await.unwrap();
        assert_eq!((backlog.databases, backlog.entries), (2, 2));
        assert_eq!(backlog.oldest.map(|t| t.timestamp_millis()), Some(1_000));

        // Reconcile: outbox done, a backfill found.
        index
            .reconcile("slot:00001", &BTreeSet::from([PendingKind::Backfill]), None)
            .await
            .unwrap();
        let rows = index.pending_for(&["slot:00001".to_owned()]).await.unwrap();
        assert_eq!(
            rows.iter().map(|e| e.kind).collect::<Vec<_>>(),
            vec![PendingKind::Backfill]
        );
        // An older binary (`keep`) never clears a backfill row.
        index
            .reconcile("slot:00001", &BTreeSet::new(), Some(PendingKind::Backfill))
            .await
            .unwrap();
        assert_eq!(
            index
                .pending_for(&["slot:00001".to_owned()])
                .await
                .unwrap()
                .len(),
            1
        );
        index.clear_database("slot:00002").await.unwrap();
        index
            .reconcile("slot:00001", &BTreeSet::new(), None)
            .await
            .unwrap();
        assert!(index.pending(10).await.unwrap().is_empty());
        assert_eq!(index.backlog().await.unwrap().oldest, None);
    }

    #[tokio::test]
    async fn many_marks_and_the_state_value_persist() {
        let tmp = tempfile::tempdir().unwrap();
        let index = index(tmp.path(), 5).await;
        let keys: Vec<String> = (0..1_200).map(|n| format!("slot:{n:05}")).collect();
        index.mark_many(&keys, PendingKind::Backfill).await.unwrap();
        assert_eq!(index.backlog().await.unwrap().databases, 1_200);
        assert_eq!(index.pending_for(&keys).await.unwrap().len(), 1_200);

        assert_eq!(index.state_value("derivations").await.unwrap(), None);
        index.set_state_value("derivations", "a").await.unwrap();
        index.set_state_value("derivations", "b").await.unwrap();
        assert_eq!(
            index.state_value("derivations").await.unwrap().as_deref(),
            Some("b")
        );
    }

    #[test]
    fn the_windows_verbatim_prefix_is_ignored() {
        assert_eq!(
            normalized(Path::new(r"\\?\C:\fleet\000\acme.db")),
            PathBuf::from(r"C:\fleet\000\acme.db")
        );
        assert_eq!(
            normalized(Path::new("/var/fleet/acme.db")),
            PathBuf::from("/var/fleet/acme.db")
        );
    }

    #[tokio::test]
    async fn a_connection_to_a_fleet_database_marks_its_own_row() {
        let tmp = tempfile::tempdir().unwrap();
        let index = Arc::new(index(tmp.path(), 7).await);
        let fleet = fleet(tmp.path());
        register(fleet.root(), &index);

        let db = fleet.open(&FleetDbKey::Slot(3)).await.unwrap();
        let mut conn = db.pool().get().await.unwrap();
        mark_connection(&mut conn, PendingKind::Hooks)
            .await
            .unwrap();
        let rows = index.pending(10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (rows[0].key.as_str(), rows[0].kind),
            ("slot:00003", PendingKind::Hooks)
        );

        // The control database is no fleet database: nothing is marked.
        let mut control = index.control.get().await.unwrap();
        mark_connection(&mut control, PendingKind::Outbox)
            .await
            .unwrap();
        assert_eq!(index.pending(10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn each_fleet_marks_its_own_index_and_an_unknown_root_marks_none() {
        let dirs: Vec<tempfile::TempDir> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
        let (index_a, index_b) = (
            Arc::new(index(dirs[0].path(), 1).await),
            Arc::new(index(dirs[1].path(), 1).await),
        );
        let fleets: Vec<DatabaseFleet> = dirs.iter().map(|dir| fleet(dir.path())).collect();
        register(fleets[0].root(), &index_a);
        register(fleets[1].root(), &index_b);
        for fleet in &fleets {
            let db = fleet.open(&FleetDbKey::Slot(1)).await.unwrap();
            let mut conn = db.pool().get().await.unwrap();
            mark_connection(&mut conn, PendingKind::Outbox)
                .await
                .unwrap();
        }
        // Fleet c registers no index: its database is marked nowhere.
        assert_eq!(index_a.pending(10).await.unwrap().len(), 1, "fleet a's");
        assert_eq!(index_b.pending(10).await.unwrap().len(), 1, "fleet b's");
    }

    #[tokio::test]
    async fn a_deferred_database_is_not_due_until_its_backoff_ends() {
        let tmp = tempfile::tempdir().unwrap();
        let index = index(tmp.path(), 10_000).await;
        index.mark("slot:00001", PendingKind::Outbox).await.unwrap();
        index.mark("slot:00002", PendingKind::Outbox).await.unwrap();
        index.defer("slot:00001").await.unwrap();
        let due: Vec<String> = index
            .due(None, 10)
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.key)
            .collect();
        assert_eq!(due, vec!["slot:00002"], "a stuck database hides no other");
        assert_eq!(
            index.pending(10).await.unwrap().len(),
            2,
            "it is still listed"
        );
        // The clock is pinned: the backoff is 1 s, then 2 s.
        let later = FleetPendingIndex::new(
            index.control.clone(),
            Arc::new(crate::time::FixedClock::at(from_millis(11_000))),
        );
        assert_eq!(later.due(None, 10).await.unwrap().len(), 2, "1 s passed");
        later.defer("slot:00001").await.unwrap();
        assert_eq!(later.due(None, 10).await.unwrap().len(), 1, "now 2 s");
        later.undefer("slot:00001").await.unwrap();
        assert_eq!(
            later
                .due(Some(&["slot:00001".to_owned()]), 0)
                .await
                .unwrap()
                .len(),
            1
        );
    }
}
