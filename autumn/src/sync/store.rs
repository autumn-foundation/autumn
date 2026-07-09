//! Local offline store: `SQLite` rows + write-through pending journal.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use chrono::{DateTime, Utc};
use diesel::connection::SimpleConnection;
use diesel::prelude::*;
use diesel::sql_query;
use diesel::sql_types::{BigInt, Bool, Nullable, Text};
use diesel::sqlite::SqliteConnection;
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::SyncError;
use super::protocol::{Change, ChangeOutcome, Op, RemoteRow, Version};

const SCHEMA_DDL: &str = "
CREATE TABLE IF NOT EXISTS autumn_sync_rows (
    collection TEXT NOT NULL,
    pk TEXT NOT NULL,
    payload TEXT,
    server_version INTEGER NOT NULL DEFAULT 0,
    deleted INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (collection, pk)
);
CREATE TABLE IF NOT EXISTS autumn_sync_pending (
    change_id TEXT PRIMARY KEY,
    collection TEXT NOT NULL,
    pk TEXT NOT NULL,
    op TEXT NOT NULL CHECK (op IN ('upsert', 'delete')),
    payload TEXT,
    base_version INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL,
    queued_at TEXT NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS autumn_sync_pending_row
    ON autumn_sync_pending (collection, pk);
CREATE TABLE IF NOT EXISTS autumn_sync_state (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
";

const STATE_DEVICE_ID: &str = "device_id";
const STATE_CURSOR: &str = "cursor";
const STATE_IDENTITY: &str = "identity";

fn store_err(err: impl std::fmt::Display) -> SyncError {
    SyncError::Store(err.to_string())
}

#[derive(QueryableByName)]
struct RowRecord {
    #[diesel(sql_type = Nullable<Text>)]
    payload: Option<String>,
    #[diesel(sql_type = Bool)]
    deleted: bool,
}

#[derive(QueryableByName)]
struct VersionRecord {
    #[diesel(sql_type = BigInt)]
    server_version: i64,
}

#[derive(QueryableByName)]
struct ListRecord {
    #[diesel(sql_type = Text)]
    pk: String,
    #[diesel(sql_type = Nullable<Text>)]
    payload: Option<String>,
}

#[derive(QueryableByName)]
struct RowKeyRecord {
    #[diesel(sql_type = Text)]
    collection: String,
    #[diesel(sql_type = Text)]
    pk: String,
}

#[derive(QueryableByName)]
struct CountRecord {
    #[diesel(sql_type = BigInt)]
    count: i64,
}

#[derive(QueryableByName)]
struct StateRecord {
    #[diesel(sql_type = Text)]
    value: String,
}

#[derive(QueryableByName)]
struct PendingRecord {
    #[diesel(sql_type = Text)]
    change_id: String,
    #[diesel(sql_type = Text)]
    collection: String,
    #[diesel(sql_type = Text)]
    pk: String,
    #[diesel(sql_type = Text)]
    op: String,
    #[diesel(sql_type = Nullable<Text>)]
    payload: Option<String>,
    #[diesel(sql_type = BigInt)]
    base_version: i64,
    #[diesel(sql_type = Text)]
    updated_at: String,
}

fn parse_timestamp(value: &str) -> Result<DateTime<Utc>, SyncError> {
    DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(store_err)
}

fn get_state(conn: &mut SqliteConnection, key: &str) -> Result<Option<String>, SyncError> {
    sql_query("SELECT value FROM autumn_sync_state WHERE key = ?")
        .bind::<Text, _>(key)
        .get_result::<StateRecord>(conn)
        .optional()
        .map(|record| record.map(|r| r.value))
        .map_err(store_err)
}

fn set_state(
    conn: &mut SqliteConnection,
    key: &str,
    value: &str,
) -> Result<(), diesel::result::Error> {
    sql_query(
        "INSERT INTO autumn_sync_state (key, value) VALUES (?, ?) \
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
    )
    .bind::<Text, _>(key)
    .bind::<Text, _>(value)
    .execute(conn)
    .map(|_| ())
}

/// The version of `(collection, pk)` the next journaled change must be
/// based on: the base of an already-pending change if there is one (the
/// journal coalesces), otherwise the row's last acknowledged server
/// version, otherwise `0`.
fn base_version_for(
    conn: &mut SqliteConnection,
    collection: &str,
    pk: &str,
) -> Result<Version, diesel::result::Error> {
    let pending = sql_query(
        "SELECT base_version AS server_version FROM autumn_sync_pending \
         WHERE collection = ? AND pk = ?",
    )
    .bind::<Text, _>(collection)
    .bind::<Text, _>(pk)
    .get_result::<VersionRecord>(conn)
    .optional()?;
    if let Some(record) = pending {
        return Ok(record.server_version);
    }
    let row =
        sql_query("SELECT server_version FROM autumn_sync_rows WHERE collection = ? AND pk = ?")
            .bind::<Text, _>(collection)
            .bind::<Text, _>(pk)
            .get_result::<VersionRecord>(conn)
            .optional()?;
    Ok(row.map_or(0, |record| record.server_version))
}

/// Replace (coalesce) the pending journal entry for `(collection, pk)`.
fn replace_pending(
    conn: &mut SqliteConnection,
    collection: &str,
    pk: &str,
    op: &str,
    payload: Option<&str>,
    base_version: Version,
    updated_at: &str,
) -> Result<(), diesel::result::Error> {
    sql_query("DELETE FROM autumn_sync_pending WHERE collection = ? AND pk = ?")
        .bind::<Text, _>(collection)
        .bind::<Text, _>(pk)
        .execute(conn)?;
    sql_query(
        "INSERT INTO autumn_sync_pending \
         (change_id, collection, pk, op, payload, base_version, updated_at, queued_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind::<Text, _>(uuid::Uuid::new_v4().to_string())
    .bind::<Text, _>(collection)
    .bind::<Text, _>(pk)
    .bind::<Text, _>(op)
    .bind::<Nullable<Text>, _>(payload)
    .bind::<BigInt, _>(base_version)
    .bind::<Text, _>(updated_at)
    .bind::<Text, _>(Utc::now().to_rfc3339())
    .execute(conn)
    .map(|_| ())
}

/// Upsert a row's materialized local state.
fn upsert_row(
    conn: &mut SqliteConnection,
    collection: &str,
    pk: &str,
    payload: Option<&str>,
    server_version: Version,
    deleted: bool,
    updated_at: &str,
) -> Result<(), diesel::result::Error> {
    sql_query(
        "INSERT INTO autumn_sync_rows \
         (collection, pk, payload, server_version, deleted, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?) \
         ON CONFLICT (collection, pk) DO UPDATE SET \
         payload = excluded.payload, server_version = excluded.server_version, \
         deleted = excluded.deleted, updated_at = excluded.updated_at",
    )
    .bind::<Text, _>(collection)
    .bind::<Text, _>(pk)
    .bind::<Nullable<Text>, _>(payload)
    .bind::<BigInt, _>(server_version)
    .bind::<Bool, _>(deleted)
    .bind::<Text, _>(updated_at)
    .execute(conn)
    .map(|_| ())
}

/// Materialize a server-side row state (pull page or conflict resolution)
/// into the local row table.
fn upsert_remote_row(
    conn: &mut SqliteConnection,
    row: &RemoteRow,
) -> Result<(), diesel::result::Error> {
    let payload = row.payload.as_ref().map(serde_json::Value::to_string);
    upsert_row(
        conn,
        &row.collection,
        &row.pk,
        payload.as_deref(),
        row.version,
        row.deleted,
        &row.updated_at.to_rfc3339(),
    )
}

fn current_server_version(
    conn: &mut SqliteConnection,
    collection: &str,
    pk: &str,
) -> Result<Version, diesel::result::Error> {
    let row =
        sql_query("SELECT server_version FROM autumn_sync_rows WHERE collection = ? AND pk = ?")
            .bind::<Text, _>(collection)
            .bind::<Text, _>(pk)
            .get_result::<VersionRecord>(conn)
            .optional()?;
    Ok(row.map_or(0, |record| record.server_version))
}

/// Record a server-acknowledged version for `(collection, pk)` without
/// touching the payload. Guarded (`server_version < ?`) so a stale ack —
/// e.g. an `AlreadyApplied` retry answered after a newer pull already
/// advanced the row — can never regress the recorded version.
fn record_acked_version(
    conn: &mut SqliteConnection,
    collection: &str,
    pk: &str,
    version: Version,
) -> Result<(), diesel::result::Error> {
    sql_query(
        "UPDATE autumn_sync_rows SET server_version = ? \
         WHERE collection = ? AND pk = ? AND server_version < ?",
    )
    .bind::<BigInt, _>(version)
    .bind::<Text, _>(collection)
    .bind::<Text, _>(pk)
    .bind::<BigInt, _>(version)
    .execute(conn)
    .map(|_| ())
}

/// After a clean ack (`Applied`/`AlreadyApplied`) for `change`, re-base the
/// surviving coalesced pending entry onto the acknowledged version.
///
/// A local edit made while the push was in flight replaced the journal
/// entry (see [`replace_pending`]) and inherited the in-flight change's
/// `base_version`; once the server acknowledges that change, the inherited
/// base is stale — pushing it would false-conflict against this device's
/// own acknowledged write (harmless under LWW, wrong under custom
/// resolvers). Only the entry created on top of the acked change is
/// touched (same row AND the inherited base), and only forward
/// (`base_version < ?`) so a stale retry ack can never regress a base.
/// `Resolved` outcomes deliberately do NOT re-base: a write made without
/// knowledge of the resolution must still conflict and go through the
/// resolver.
fn rebase_surviving_pending(
    conn: &mut SqliteConnection,
    change: &Change,
    acked_version: Version,
) -> Result<(), diesel::result::Error> {
    sql_query(
        "UPDATE autumn_sync_pending SET base_version = ? \
         WHERE collection = ? AND pk = ? AND base_version = ? AND base_version < ?",
    )
    .bind::<BigInt, _>(acked_version)
    .bind::<Text, _>(&change.collection)
    .bind::<Text, _>(&change.pk)
    .bind::<BigInt, _>(change.base_version)
    .bind::<BigInt, _>(acked_version)
    .execute(conn)
    .map(|_| ())
}

fn has_pending(
    conn: &mut SqliteConnection,
    collection: &str,
    pk: &str,
) -> Result<bool, diesel::result::Error> {
    let count = sql_query(
        "SELECT COUNT(*) AS count FROM autumn_sync_pending WHERE collection = ? AND pk = ?",
    )
    .bind::<Text, _>(collection)
    .bind::<Text, _>(pk)
    .get_result::<CountRecord>(conn)?;
    Ok(count.count > 0)
}

/// Row-application body shared by [`SyncStore::apply_remote_rows`] and
/// [`SyncStore::apply_remote_page`]: idempotent versioned upsert, skipping
/// rows with a pending local change (the local write wins until its push
/// settles the conflict).
fn apply_remote_rows_inner(
    conn: &mut SqliteConnection,
    rows: &[RemoteRow],
) -> Result<usize, diesel::result::Error> {
    let mut applied = 0;
    for row in rows {
        if has_pending(conn, &row.collection, &row.pk)? {
            continue;
        }
        if current_server_version(conn, &row.collection, &row.pk)? > row.version {
            continue;
        }
        upsert_remote_row(conn, row)?;
        applied += 1;
    }
    Ok(applied)
}

/// The local, in-process `SQLite` store for offline data.
///
/// App data is stored as JSON payloads keyed by `(collection, pk)` —
/// anything `Serialize + DeserializeOwned` round-trips. Every write also
/// journals a pending change **in the same `SQLite` transaction** (a crash
/// cannot lose a journal entry), so the [`crate::sync::SyncEngine`] can
/// replay it to the server later. Deletes are recorded as tombstone rows so
/// they replicate too.
///
/// # Concurrency
///
/// Cheap to clone: all clones share one WAL-mode connection behind a
/// mutex, which serializes concurrent writers *within* that instance —
/// **prefer opening the store once and cloning it** (e.g. from a
/// `OnceLock`) over calling [`Self::open`] per use. Separate
/// [`Self::open`] calls on the same path are still safe: each write runs
/// in a `BEGIN IMMEDIATE` transaction, so cross-connection writers queue
/// on the 5 s busy timeout instead of failing mid-transaction with
/// `SQLITE_BUSY_SNAPSHOT`, but every `open` pays connection + schema
/// setup and adds lock contention.
#[derive(Clone)]
pub struct SyncStore {
    conn: Arc<Mutex<SqliteConnection>>,
    device_id: String,
}

impl std::fmt::Debug for SyncStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncStore")
            .field("device_id", &self.device_id)
            .finish_non_exhaustive()
    }
}

impl SyncStore {
    /// Open (creating if needed) the store at `path`, e.g.
    /// `app_data_dir/sync.db`. Missing parent directories are created; the
    /// schema is applied idempotently; a stable device id (UUID v4) is
    /// generated on first open.
    ///
    /// # Errors
    ///
    /// Returns [`SyncError::Store`] if the path is not valid UTF-8, or the
    /// database cannot be opened or its schema cannot be created.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SyncError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(store_err)?;
        }
        let path_str = path
            .to_str()
            .ok_or_else(|| SyncError::Store("store path is not valid UTF-8".into()))?;

        let mut conn = SqliteConnection::establish(path_str).map_err(store_err)?;
        // busy_timeout FIRST: everything after it queues on the timeout
        // instead of failing with "database is locked".
        conn.batch_execute("PRAGMA busy_timeout = 5000;")
            .map_err(store_err)?;
        // Converting a database into WAL mode needs a moment of exclusive
        // access, and SQLite returns SQLITE_BUSY for that WITHOUT
        // consulting the busy handler when another connection holds any
        // lock (a deliberate deadlock-avoidance rule). Concurrent first
        // opens of one file hit exactly this, so retry the conversion +
        // schema DDL briefly instead of failing the open.
        let mut attempts = 0;
        loop {
            let result = conn
                .batch_execute(
                    "PRAGMA journal_mode = WAL; \
                     PRAGMA synchronous = NORMAL; \
                     PRAGMA foreign_keys = ON;",
                )
                .and_then(|()| conn.batch_execute(SCHEMA_DDL));
            match result {
                Ok(()) => break,
                Err(err) if attempts < 100 && err.to_string().contains("database is locked") => {
                    attempts += 1;
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(err) => return Err(store_err(err)),
            }
        }

        // Race-safe first-open id generation: concurrent opens of a fresh
        // file each offer a candidate id, exactly one INSERT wins
        // (`DO NOTHING` on conflict), and every opener reads back the
        // winning value — no instance can end up caching a device id that
        // was never persisted.
        sql_query(
            "INSERT INTO autumn_sync_state (key, value) VALUES (?, ?) \
             ON CONFLICT (key) DO NOTHING",
        )
        .bind::<Text, _>(STATE_DEVICE_ID)
        .bind::<Text, _>(uuid::Uuid::new_v4().to_string())
        .execute(&mut conn)
        .map_err(store_err)?;
        let device_id = get_state(&mut conn, STATE_DEVICE_ID)?
            .ok_or_else(|| SyncError::Store("device id was not persisted".into()))?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            device_id,
        })
    }

    fn lock(&self) -> Result<MutexGuard<'_, SqliteConnection>, SyncError> {
        self.conn
            .lock()
            .map_err(|_| SyncError::Store("sync store mutex poisoned".into()))
    }

    /// Bind the store to the LOCAL identity of the authenticated account
    /// (e.g. your auth layer's user id) — and, when it differs from the
    /// identity previously bound, RESET the local cache first: cached
    /// rows, local tombstones, the pending outbox, and the cursor are all
    /// dropped in one transaction before the new identity is persisted.
    ///
    /// Why this exists: in scoped deployments (see the sync server's
    /// `SyncScope`) one installation can re-authenticate as a different
    /// account, but this store is one `SQLite` file keyed only by
    /// `(collection, pk)`. Without the reset the next account would (1)
    /// SEE the previous account's cached rows before its first pull — a
    /// local data leak — and (2) inherit the previous account's cursor;
    /// versions come from ONE global sequence across scopes, so that
    /// cursor can sit ABOVE the new account's own rows and its first pull
    /// would skip them permanently.
    ///
    /// The **pending outbox is dropped too**: those changes were authored
    /// by the previous account and must not be pushed as the new one.
    /// Unsynced edits of the outgoing account are therefore lost on
    /// switch — sync before switching when they must survive. The device
    /// id is kept (it identifies the installation, not the account).
    ///
    /// Call this on every login (and, to wipe eagerly at logout, with a
    /// sentinel such as `"signed-out"`), BEFORE the next sync pass. The
    /// identity is a client-side key from your auth layer — the
    /// server-side scope string is derived from auth on the server and
    /// never client-supplied, so the two strings need not match; the
    /// identity only has to CHANGE whenever the authenticated account
    /// changes. The first bind on a store that never had one adopts the
    /// existing data without clearing (upgrade continuity), and never
    /// calling this at all preserves today's single-user behavior.
    ///
    /// # Errors
    ///
    /// Returns [`SyncError::Store`] on database failure.
    pub fn set_identity(&self, identity: &str) -> Result<(), SyncError> {
        let mut conn = self.lock()?;
        conn.immediate_transaction::<_, diesel::result::Error, _>(|conn| {
            let previous = sql_query("SELECT value FROM autumn_sync_state WHERE key = ?")
                .bind::<Text, _>(STATE_IDENTITY)
                .get_result::<StateRecord>(conn)
                .optional()?
                .map(|record| record.value);
            if previous.as_deref() == Some(identity) {
                return Ok(());
            }
            if previous.is_some() {
                sql_query("DELETE FROM autumn_sync_rows").execute(conn)?;
                sql_query("DELETE FROM autumn_sync_pending").execute(conn)?;
                sql_query("DELETE FROM autumn_sync_state WHERE key = ?")
                    .bind::<Text, _>(STATE_CURSOR)
                    .execute(conn)?;
            }
            set_state(conn, STATE_IDENTITY, identity)
        })
        .map_err(store_err)
    }

    /// One local write = row upsert + coalesced journal entry, in a single
    /// `SQLite` transaction (the change-tracking invariant of the store).
    fn write_local(
        &self,
        collection: &str,
        pk: &str,
        op: Op,
        payload: Option<&str>,
    ) -> Result<(), SyncError> {
        let now = Utc::now().to_rfc3339();
        let op_name = match op {
            Op::Upsert => "upsert",
            Op::Delete => "delete",
        };
        let mut conn = self.lock()?;
        // BEGIN IMMEDIATE: take the write lock up front so a concurrent
        // writer on ANOTHER connection to the same file (a second
        // SyncStore::open) makes this transaction queue on the busy
        // timeout, instead of failing its read→write snapshot upgrade
        // with SQLITE_BUSY_SNAPSHOT (which bypasses the busy handler).
        conn.immediate_transaction::<_, diesel::result::Error, _>(|conn| {
            let base_version = base_version_for(conn, collection, pk)?;
            let server_version = current_server_version(conn, collection, pk)?;
            upsert_row(
                conn,
                collection,
                pk,
                payload,
                server_version,
                op == Op::Delete,
                &now,
            )?;
            replace_pending(conn, collection, pk, op_name, payload, base_version, &now)
        })
        .map_err(store_err)
    }

    /// Insert or update `value` under `(collection, pk)`, journaling the
    /// change for the next sync in the same transaction.
    ///
    /// # Errors
    ///
    /// Returns [`SyncError::Serde`] if `value` cannot be serialized, or
    /// [`SyncError::Store`] on database failure.
    pub fn put<T: Serialize>(
        &self,
        collection: &str,
        pk: &str,
        value: &T,
    ) -> Result<(), SyncError> {
        let payload = serde_json::to_string(value)?;
        self.write_local(collection, pk, Op::Upsert, Some(&payload))
    }

    /// Delete the row at `(collection, pk)`, recording a local tombstone
    /// and journaling the delete in the same transaction.
    ///
    /// # Errors
    ///
    /// Returns [`SyncError::Store`] on database failure.
    pub fn delete(&self, collection: &str, pk: &str) -> Result<(), SyncError> {
        self.write_local(collection, pk, Op::Delete, None)
    }

    /// Fetch the row at `(collection, pk)`, or `None` if absent or deleted.
    ///
    /// # Errors
    ///
    /// Returns [`SyncError::Serde`] if the payload cannot be deserialized
    /// as `T`, or [`SyncError::Store`] on database failure.
    pub fn get<T: DeserializeOwned>(
        &self,
        collection: &str,
        pk: &str,
    ) -> Result<Option<T>, SyncError> {
        let mut conn = self.lock()?;
        let record = sql_query(
            "SELECT payload, deleted FROM autumn_sync_rows WHERE collection = ? AND pk = ?",
        )
        .bind::<Text, _>(collection)
        .bind::<Text, _>(pk)
        .get_result::<RowRecord>(&mut *conn)
        .optional()
        .map_err(store_err)?;
        drop(conn);
        if let Some(RowRecord {
            payload: Some(payload),
            deleted: false,
        }) = record
        {
            Ok(Some(serde_json::from_str(&payload)?))
        } else {
            Ok(None)
        }
    }

    /// List all live (non-tombstoned) rows in `collection` as
    /// `(pk, value)` pairs, ordered by pk.
    ///
    /// # Errors
    ///
    /// Returns [`SyncError::Serde`] if a payload cannot be deserialized as
    /// `T`, or [`SyncError::Store`] on database failure.
    pub fn list<T: DeserializeOwned>(
        &self,
        collection: &str,
    ) -> Result<Vec<(String, T)>, SyncError> {
        let mut conn = self.lock()?;
        let records = sql_query(
            "SELECT pk, payload FROM autumn_sync_rows \
             WHERE collection = ? AND deleted = 0 ORDER BY pk",
        )
        .bind::<Text, _>(collection)
        .get_results::<ListRecord>(&mut *conn)
        .map_err(store_err)?;
        drop(conn);
        records
            .into_iter()
            .filter_map(|record| record.payload.map(|payload| (record.pk, payload)))
            .map(|(pk, payload)| Ok((pk, serde_json::from_str(&payload)?)))
            .collect()
    }

    /// Number of journaled changes not yet acknowledged by the server.
    ///
    /// # Errors
    ///
    /// Returns [`SyncError::Store`] on database failure.
    pub fn pending_count(&self) -> Result<u64, SyncError> {
        let mut conn = self.lock()?;
        let record = sql_query("SELECT COUNT(*) AS count FROM autumn_sync_pending")
            .get_result::<CountRecord>(&mut *conn)
            .map_err(store_err)?;
        drop(conn);
        Ok(u64::try_from(record.count).unwrap_or(0))
    }

    /// The oldest pending changes, up to `limit`, in queue order.
    ///
    /// # Errors
    ///
    /// Returns [`SyncError::Store`] on database failure, or
    /// [`SyncError::Serde`] if a journaled payload is corrupt.
    pub fn pending_changes(&self, limit: usize) -> Result<Vec<Change>, SyncError> {
        let mut conn = self.lock()?;
        let records = sql_query(
            "SELECT change_id, collection, pk, op, payload, base_version, updated_at \
             FROM autumn_sync_pending ORDER BY rowid LIMIT ?",
        )
        .bind::<BigInt, _>(i64::try_from(limit).unwrap_or(i64::MAX))
        .get_results::<PendingRecord>(&mut *conn)
        .map_err(store_err)?;
        drop(conn);
        records
            .into_iter()
            .map(|record| {
                let op = match record.op.as_str() {
                    "delete" => Op::Delete,
                    _ => Op::Upsert,
                };
                let payload = record
                    .payload
                    .as_deref()
                    .map(serde_json::from_str)
                    .transpose()?;
                Ok(Change {
                    change_id: record.change_id,
                    collection: record.collection,
                    pk: record.pk,
                    op,
                    payload,
                    base_version: record.base_version,
                    updated_at: parse_timestamp(&record.updated_at)?,
                })
            })
            .collect()
    }

    /// This device's stable id (UUID v4, generated on first open).
    ///
    /// # Errors
    ///
    /// Infallible today; kept fallible for API stability.
    pub fn device_id(&self) -> Result<String, SyncError> {
        Ok(self.device_id.clone())
    }

    /// The last server version this store has pulled through.
    ///
    /// # Errors
    ///
    /// Returns [`SyncError::Store`] on database failure.
    pub fn cursor(&self) -> Result<Version, SyncError> {
        let mut conn = self.lock()?;
        let value = get_state(&mut conn, STATE_CURSOR)?;
        drop(conn);
        Ok(value.and_then(|v| v.parse().ok()).unwrap_or(0))
    }

    /// Settle a pushed batch: journal entries are cleared **by change id**
    /// (a newer coalesced write keeps its own entry), acked versions are
    /// recorded — including for `AlreadyApplied` retries, so a later local
    /// edit doesn't push a stale `base_version` and false-conflict — a
    /// surviving coalesced write is re-based onto a clean ack's version
    /// (see [`rebase_surviving_pending`]), and conflict resolutions are
    /// applied locally *unless* the row gained a newer pending local write
    /// in the meantime (the resolved row must not clobber it; the newer
    /// write pushes next and settles through the resolver normally).
    pub(crate) fn confirm_pushed(
        &self,
        changes: &[Change],
        outcomes: &[ChangeOutcome],
    ) -> Result<(), SyncError> {
        let mut conn = self.lock()?;
        conn.immediate_transaction::<_, diesel::result::Error, _>(|conn| {
            for (change, outcome) in changes.iter().zip(outcomes) {
                // Clear this batch's entry first: a newer coalesced write
                // replaced it with a fresh change_id, so `has_pending`
                // below is true exactly when such a newer write exists.
                sql_query("DELETE FROM autumn_sync_pending WHERE change_id = ?")
                    .bind::<Text, _>(&change.change_id)
                    .execute(conn)?;
                match outcome {
                    // AlreadyApplied is the ack a lost response withheld:
                    // record it exactly like a fresh Applied ack.
                    ChangeOutcome::Applied { version }
                    | ChangeOutcome::AlreadyApplied { version } => {
                        record_acked_version(conn, &change.collection, &change.pk, *version)?;
                        // A coalesced write made while this push was in
                        // flight inherited this change's (now acked) base —
                        // re-base it so it doesn't false-conflict against
                        // our own acknowledged write.
                        rebase_surviving_pending(conn, change, *version)?;
                    }
                    ChangeOutcome::Resolved { row } => {
                        if has_pending(conn, &row.collection, &row.pk)? {
                            // Read-your-writes: keep the newer local
                            // payload; just record the resolved version so
                            // future (post-journal) writes base correctly.
                            record_acked_version(conn, &row.collection, &row.pk, row.version)?;
                        } else {
                            upsert_remote_row(conn, row)?;
                        }
                    }
                }
            }
            Ok(())
        })
        .map_err(store_err)
    }

    /// Apply a pulled page of remote rows without touching the cursor.
    /// Idempotent versioned upsert; rows with a pending local change are
    /// skipped (the local write wins until its push settles the conflict).
    /// Returns how many rows applied.
    ///
    /// Test-only: the engine's pull path always goes through
    /// [`Self::apply_remote_page`] so rows and cursor persist atomically;
    /// unit tests use this to materialize rows without a cursor opinion.
    #[cfg(test)]
    pub(crate) fn apply_remote_rows(&self, rows: &[RemoteRow]) -> Result<usize, SyncError> {
        let mut conn = self.lock()?;
        conn.immediate_transaction::<_, diesel::result::Error, _>(|conn| {
            apply_remote_rows_inner(conn, rows)
        })
        .map_err(store_err)
    }

    /// Apply a pulled page of remote rows AND persist the advanced cursor
    /// in the **same transaction** (semantics per row as
    /// [`Self::apply_remote_rows`]).
    ///
    /// The atomicity is load-bearing: applying rows first and persisting
    /// the cursor separately leaves a crash window where synced rows exist
    /// with a stale cursor. At cursor `0` that state is indistinguishable
    /// from a fresh device, so the next pull's `session=0` is exempt from
    /// the server's tombstone-horizon check — tombstones GC'd in the
    /// meantime would never arrive and stale local rows would stay visible
    /// forever (the engine also heals pre-existing states like this; see
    /// `SyncEngine::sync_once`).
    pub(crate) fn apply_remote_page(
        &self,
        rows: &[RemoteRow],
        cursor: Version,
    ) -> Result<usize, SyncError> {
        let mut conn = self.lock()?;
        conn.immediate_transaction::<_, diesel::result::Error, _>(|conn| {
            let applied = apply_remote_rows_inner(conn, rows)?;
            set_state(conn, STATE_CURSOR, &cursor.to_string())?;
            Ok(applied)
        })
        .map_err(store_err)
    }

    /// Whether any **synced** row exists — a materialized row with an
    /// acknowledged server version and no pending journal entry. Together
    /// with a cursor of `0` this is the inconsistent crash state
    /// [`Self::apply_remote_page`] prevents (rows persisted, cursor not),
    /// which `SyncEngine::sync_once` heals with a full resync.
    pub(crate) fn has_synced_rows(&self) -> Result<bool, SyncError> {
        let mut conn = self.lock()?;
        let count = sql_query(
            "SELECT COUNT(*) AS count FROM autumn_sync_rows \
             WHERE server_version > 0 AND NOT EXISTS (\
             SELECT 1 FROM autumn_sync_pending p \
             WHERE p.collection = autumn_sync_rows.collection \
             AND p.pk = autumn_sync_rows.pk)",
        )
        .get_result::<CountRecord>(&mut *conn)
        .map_err(store_err)?;
        drop(conn);
        Ok(count.count > 0)
    }

    /// Reconcile local state against a COMPLETE from-zero server snapshot,
    /// in one transaction: apply every snapshot row (same per-row semantics
    /// as [`Self::apply_remote_rows`]), delete synced (non-pending) rows
    /// absent from the snapshot, and land the cursor. Returns how many rows
    /// applied.
    ///
    /// The all-or-nothing shape is the point (see `SyncEngine`'s
    /// zero-cursor heal): nothing local is touched until the entire
    /// snapshot has been fetched, so a transport failure mid-heal leaves
    /// the pre-heal state — including acknowledged-but-never-pulled local
    /// rows — fully intact, and the cursor stays at 0 so the heal re-runs
    /// on the next pass.
    pub(crate) fn reconcile_snapshot(
        &self,
        rows: &[RemoteRow],
        cursor: Version,
    ) -> Result<usize, SyncError> {
        let mut conn = self.lock()?;
        conn.immediate_transaction::<_, diesel::result::Error, _>(|conn| {
            let applied = apply_remote_rows_inner(conn, rows)?;
            // Drop synced rows the snapshot no longer contains (their
            // deletions may have been tombstone-GC'd server-side). Rows
            // with a pending journal entry are preserved — unsynced local
            // writes always survive and get replayed.
            let snapshot_keys: std::collections::HashSet<(&str, &str)> = rows
                .iter()
                .map(|row| (row.collection.as_str(), row.pk.as_str()))
                .collect();
            let existing = sql_query(
                "SELECT collection, pk FROM autumn_sync_rows \
                 WHERE server_version > 0 AND NOT EXISTS (\
                 SELECT 1 FROM autumn_sync_pending p \
                 WHERE p.collection = autumn_sync_rows.collection \
                 AND p.pk = autumn_sync_rows.pk)",
            )
            .get_results::<RowKeyRecord>(conn)?;
            for record in existing {
                if !snapshot_keys.contains(&(record.collection.as_str(), record.pk.as_str())) {
                    sql_query("DELETE FROM autumn_sync_rows WHERE collection = ? AND pk = ?")
                        .bind::<Text, _>(&record.collection)
                        .bind::<Text, _>(&record.pk)
                        .execute(conn)?;
                }
            }
            set_state(conn, STATE_CURSOR, &cursor.to_string())?;
            Ok(applied)
        })
        .map_err(store_err)
    }

    /// Drop local tombstone rows whose deletion the server has already
    /// GC'd (acked `server_version <= horizon`). Safe because the server
    /// physically removed those tombstones — no future pull can carry a
    /// version at or below the horizon for these pks, and a recreation
    /// arrives as a normal row with a newer version. Tombstones with a
    /// pending journal entry (the delete not yet acked) are kept. Called by
    /// the engine after each completed pull, so local storage stays
    /// proportional to live data plus not-yet-GC'd tombstones.
    pub(crate) fn prune_acked_tombstones(&self, horizon: Version) -> Result<u64, SyncError> {
        let mut conn = self.lock()?;
        conn.immediate_transaction::<_, diesel::result::Error, _>(|conn| {
            let removed = sql_query(
                "DELETE FROM autumn_sync_rows \
                 WHERE deleted = 1 AND server_version > 0 AND server_version <= ? \
                 AND NOT EXISTS (\
                 SELECT 1 FROM autumn_sync_pending p \
                 WHERE p.collection = autumn_sync_rows.collection \
                 AND p.pk = autumn_sync_rows.pk)",
            )
            .bind::<BigInt, _>(horizon)
            .execute(conn)?;
            Ok(removed as u64)
        })
        .map_err(store_err)
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use serde_json::json;

    use super::*;

    fn open_temp() -> (tempfile::TempDir, SyncStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = SyncStore::open(dir.path().join("sync.db")).expect("open");
        (dir, store)
    }

    fn remote_row(collection: &str, pk: &str, version: Version, deleted: bool) -> RemoteRow {
        RemoteRow {
            collection: collection.to_owned(),
            pk: pk.to_owned(),
            payload: (!deleted).then(|| json!({"title": format!("server v{version}")})),
            version,
            deleted,
            updated_at: Utc::now(),
            device_id: "server-device".to_owned(),
        }
    }

    #[test]
    fn already_applied_records_the_original_acked_version() {
        // A retry after a lost response must record the version the change
        // was assigned the first time, so the NEXT local edit is based on
        // it instead of pushing base_version = 0 (a false conflict).
        let (_dir, store) = open_temp();
        store
            .put("notes", "n1", &json!({"title": "v1"}))
            .expect("put");
        let changes = store.pending_changes(10).expect("pending");
        assert_eq!(changes.len(), 1);

        store
            .confirm_pushed(&changes, &[ChangeOutcome::AlreadyApplied { version: 7 }])
            .expect("confirm");
        assert_eq!(store.pending_count().expect("count"), 0);

        store
            .put("notes", "n1", &json!({"title": "v2"}))
            .expect("re-put");
        let next = store.pending_changes(10).expect("pending");
        assert_eq!(
            next[0].base_version, 7,
            "the next edit must be based on the acked version, not 0"
        );
    }

    #[test]
    fn stale_already_applied_ack_never_regresses_the_recorded_version() {
        let (_dir, store) = open_temp();
        store
            .put("notes", "n1", &json!({"title": "v1"}))
            .expect("put");
        let changes = store.pending_changes(10).expect("pending");
        // The row advanced to version 9 (e.g. via a pull that raced the
        // retry); a late AlreadyApplied ack for version 7 must not regress.
        store
            .confirm_pushed(&changes, &[ChangeOutcome::Applied { version: 9 }])
            .expect("confirm applied");
        store
            .put("notes", "n1", &json!({"title": "v2"}))
            .expect("re-put");
        let retry = store.pending_changes(10).expect("pending");
        store
            .confirm_pushed(&retry, &[ChangeOutcome::AlreadyApplied { version: 7 }])
            .expect("stale ack");
        store
            .put("notes", "n1", &json!({"title": "v3"}))
            .expect("third put");
        let next = store.pending_changes(10).expect("pending");
        assert_eq!(next[0].base_version, 9, "acked version must never regress");
    }

    #[test]
    fn identity_switch_clears_rows_cursor_and_pending() {
        let (_dir, store) = open_temp();
        store.set_identity("user:alice").expect("bind alice");
        // Alice's world: a synced row, a pending local edit, and a cursor.
        store
            .apply_remote_page(&[remote_row("notes", "n1", 5, false)], 5)
            .expect("apply page");
        store
            .put("notes", "local", &json!({"title": "alice's draft"}))
            .expect("put");
        assert_eq!(store.cursor().expect("cursor"), 5);
        assert_eq!(store.pending_count().expect("pending"), 1);
        let device_before = store.device_id().expect("device");

        // Re-binding the SAME identity is a no-op: everything survives.
        store.set_identity("user:alice").expect("rebind alice");
        assert_eq!(store.cursor().expect("cursor"), 5);
        assert_eq!(store.pending_count().expect("pending"), 1);

        // A DIFFERENT identity resets the cache: rows, the pending outbox
        // (the outgoing account's edits must not replay as the new one),
        // and the cursor (versions are one global sequence across scopes,
        // so an inherited higher cursor would skip the new account's own
        // rows). The device id identifies the installation and survives.
        store.set_identity("user:bob").expect("bind bob");
        assert_eq!(store.cursor().expect("cursor"), 0, "cursor reset");
        assert_eq!(store.pending_count().expect("pending"), 0, "outbox reset");
        assert!(
            store
                .get::<serde_json::Value>("notes", "n1")
                .expect("get")
                .is_none(),
            "the previous account's cached rows must be gone"
        );
        assert!(
            store
                .get::<serde_json::Value>("notes", "local")
                .expect("get")
                .is_none(),
            "the previous account's local edits must be gone"
        );
        assert_eq!(store.device_id().expect("device"), device_before);
    }

    #[test]
    fn identity_persists_across_reopen_and_first_bind_adopts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("sync.db");
        {
            let store = SyncStore::open(&path).expect("open");
            // Pre-identity data: the FIRST bind adopts it (a formerly
            // single-user store being upgraded keeps its data).
            store
                .put("notes", "n1", &json!({"title": "kept"}))
                .expect("put");
            store.set_identity("user:alice").expect("first bind");
            assert!(
                store
                    .get::<serde_json::Value>("notes", "n1")
                    .expect("get")
                    .is_some(),
                "the first bind must adopt existing single-user data"
            );
        }
        {
            // Reopen: the identity persisted, so the same account keeps
            // its cache — and a different account still wipes it.
            let store = SyncStore::open(&path).expect("reopen");
            store.set_identity("user:alice").expect("rebind");
            assert!(
                store
                    .get::<serde_json::Value>("notes", "n1")
                    .expect("get")
                    .is_some(),
                "the persisted identity must survive a reopen"
            );
            store.set_identity("user:bob").expect("switch");
            assert!(
                store
                    .get::<serde_json::Value>("notes", "n1")
                    .expect("get")
                    .is_none(),
                "an account switch after reopen must still reset"
            );
        }
    }

    #[test]
    fn resolved_outcome_does_not_clobber_a_newer_pending_local_write() {
        // The user edits the row between the engine reading the push batch
        // and confirming it: the journal coalesces to a NEW change_id, and
        // the server's resolved row must not overwrite the newer payload.
        let (_dir, store) = open_temp();
        store
            .put("notes", "n1", &json!({"title": "pushed"}))
            .expect("put");
        let batch = store.pending_changes(10).expect("batch");

        // Concurrent local edit while the push is in flight.
        store
            .put("notes", "n1", &json!({"title": "newer local"}))
            .expect("concurrent put");

        let resolved = remote_row("notes", "n1", 5, false);
        store
            .confirm_pushed(&batch, &[ChangeOutcome::Resolved { row: resolved }])
            .expect("confirm");

        let visible: Option<serde_json::Value> = store.get("notes", "n1").expect("get");
        assert_eq!(
            visible.and_then(|v| v["title"].as_str().map(str::to_owned)),
            Some("newer local".to_owned()),
            "read-your-writes: the newer pending write must stay visible"
        );
        assert_eq!(
            store.pending_count().expect("count"),
            1,
            "the newer write's journal entry must survive the confirmation"
        );
        let next = store.pending_changes(10).expect("pending");
        assert_ne!(
            next[0].change_id, batch[0].change_id,
            "the surviving entry is the newer coalesced write"
        );
        assert_eq!(
            next[0].base_version, batch[0].base_version,
            "a Resolved outcome must NOT re-base the survivor: it was written \
             without knowledge of the resolution and must go through the \
             resolver on its own push"
        );
    }

    #[test]
    fn clean_ack_rebases_the_surviving_coalesced_write() {
        // The user edits the row while a clean push is in flight: the
        // journal coalesces to a NEW change_id that inherited the in-flight
        // change's base_version (0). After the server acks Applied, the
        // survivor must be re-based onto the acked version — pushing the
        // stale base would false-conflict against this device's own
        // acknowledged write (wrong under non-LWW resolvers).
        let (_dir, store) = open_temp();
        store
            .put("notes", "n1", &json!({"title": "pushed"}))
            .expect("put");
        let batch = store.pending_changes(10).expect("batch");
        assert_eq!(batch[0].base_version, 0);

        // Concurrent local edit while the push is in flight.
        store
            .put("notes", "n1", &json!({"title": "newer local"}))
            .expect("concurrent put");

        store
            .confirm_pushed(&batch, &[ChangeOutcome::Applied { version: 5 }])
            .expect("confirm");

        let next = store.pending_changes(10).expect("pending");
        assert_eq!(next.len(), 1, "the coalesced write survives");
        assert_eq!(
            next[0].base_version, 5,
            "the survivor must be re-based onto the acked version so it \
             applies cleanly instead of false-conflicting"
        );
        // A clean apply of the survivor settles without leftovers.
        store
            .confirm_pushed(&next, &[ChangeOutcome::Applied { version: 6 }])
            .expect("confirm survivor");
        assert_eq!(store.pending_count().expect("count"), 0);
        // And the next edit bases on the latest acked version.
        store
            .put("notes", "n1", &json!({"title": "v3"}))
            .expect("third put");
        assert_eq!(
            store.pending_changes(10).expect("pending")[0].base_version,
            6
        );
    }

    #[test]
    fn clean_ack_rebases_only_the_entry_built_on_the_acked_change() {
        // A pending entry whose base does NOT match the acked change's base
        // was not created on top of it — it must keep its own base.
        let (_dir, store) = open_temp();
        // Row already synced at version 9 (e.g. from a pull).
        store
            .apply_remote_rows(&[remote_row("notes", "n1", 9, false)])
            .expect("apply");
        // A pushed change for a DIFFERENT row acked at version 12.
        store
            .put("notes", "other", &json!({"title": "x"}))
            .expect("put other");
        let batch = store.pending_changes(10).expect("batch");
        // Meanwhile n1 gains a pending edit based on 9.
        store
            .put("notes", "n1", &json!({"title": "edit"}))
            .expect("put n1");
        store
            .confirm_pushed(&batch, &[ChangeOutcome::Applied { version: 12 }])
            .expect("confirm");
        let pending = store.pending_changes(10).expect("pending");
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending[0].base_version, 9,
            "an unrelated row's pending base must be untouched"
        );
    }

    #[test]
    fn resolved_outcome_applies_when_no_newer_write_exists() {
        let (_dir, store) = open_temp();
        store
            .put("notes", "n1", &json!({"title": "mine"}))
            .expect("put");
        let batch = store.pending_changes(10).expect("batch");
        let resolved = remote_row("notes", "n1", 5, false);
        store
            .confirm_pushed(&batch, &[ChangeOutcome::Resolved { row: resolved }])
            .expect("confirm");
        let visible: Option<serde_json::Value> = store.get("notes", "n1").expect("get");
        assert_eq!(
            visible.and_then(|v| v["title"].as_str().map(str::to_owned)),
            Some("server v5".to_owned()),
            "with no newer pending write the resolved row applies locally"
        );
        assert_eq!(store.pending_count().expect("count"), 0);
    }

    #[test]
    fn null_documents_round_trip_through_journal_rows_and_pulls() {
        // `put(&None::<T>)` stores the JSON document `null` — TEXT 'null'
        // in the payload column, distinct from the SQL NULL a delete
        // writes. The journal must surface it as Some(Value::Null) (a
        // PRESENT payload the server accepts), reads must see the row, and
        // a pulled remote row with a null payload must materialize.
        let (_dir, store) = open_temp();
        store
            .put("notes", "opt", &None::<String>)
            .expect("put none");
        let pending = store.pending_changes(10).expect("pending");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].op, Op::Upsert);
        assert_eq!(
            pending[0].payload,
            Some(serde_json::Value::Null),
            "the journal must carry a PRESENT null, not an omitted payload"
        );
        let value: Option<serde_json::Value> = store.get("notes", "opt").expect("get");
        assert_eq!(
            value,
            Some(serde_json::Value::Null),
            "a null document is a present row"
        );
        // A DELETE journals an absent payload — the distinction the
        // TEXT-'null'-vs-column-NULL storage preserves.
        store.delete("notes", "opt").expect("delete");
        let pending = store.pending_changes(10).expect("pending");
        assert_eq!(pending[0].op, Op::Delete);
        assert_eq!(pending[0].payload, None);

        // Pulled remote rows with a null payload materialize as present.
        let mut remote = remote_row("notes", "pulled-null", 5, false);
        remote.payload = Some(serde_json::Value::Null);
        store.apply_remote_rows(&[remote]).expect("apply");
        let value: Option<serde_json::Value> = store.get("notes", "pulled-null").expect("get");
        assert_eq!(value, Some(serde_json::Value::Null));
        let listed: Vec<(String, serde_json::Value)> = store.list("notes").expect("list");
        assert!(
            listed
                .iter()
                .any(|(pk, v)| pk == "pulled-null" && v.is_null()),
            "a null-payload row must appear in listings: {listed:?}"
        );
    }

    #[test]
    fn apply_remote_page_persists_rows_and_cursor_together() {
        let (_dir, store) = open_temp();
        let applied = store
            .apply_remote_page(&[remote_row("notes", "n1", 4, false)], 4)
            .expect("apply page");
        assert_eq!(applied, 1);
        assert_eq!(
            store.cursor().expect("cursor"),
            4,
            "the cursor must advance in the same transaction as the rows"
        );
        // An empty caught-up page still lands the cursor (e.g. at the
        // tombstone horizon).
        store.apply_remote_page(&[], 9).expect("empty page");
        assert_eq!(store.cursor().expect("cursor"), 9);
    }

    #[test]
    fn has_synced_rows_sees_only_acked_rows_without_pending_entries() {
        let (_dir, store) = open_temp();
        assert!(!store.has_synced_rows().expect("fresh store"));
        // A purely local (pending) write is not a synced row.
        store
            .put("notes", "mine", &json!({"title": "local"}))
            .expect("put");
        assert!(!store.has_synced_rows().expect("pending only"));
        // A pulled row is.
        store
            .apply_remote_rows(&[remote_row("notes", "n1", 4, false)])
            .expect("apply");
        assert!(store.has_synced_rows().expect("synced row"));
        // Reconciling against an empty snapshot clears it (the pending
        // write survives) — the shape `SyncEngine::resync_from_snapshot`
        // lands after fetching a full snapshot.
        store.reconcile_snapshot(&[], 0).expect("reconcile");
        assert!(!store.has_synced_rows().expect("after reconcile"));
        assert_eq!(store.pending_count().expect("count"), 1);
    }

    #[test]
    fn prune_acked_tombstones_drops_only_gcd_acked_tombstones() {
        let (_dir, store) = open_temp();
        // An acked tombstone at version 3 (below the horizon): prunable.
        store
            .apply_remote_rows(&[remote_row("notes", "gone", 3, true)])
            .expect("apply tombstone");
        // An acked tombstone above the horizon: kept.
        store
            .apply_remote_rows(&[remote_row("notes", "recent", 9, true)])
            .expect("apply recent tombstone");
        // A local, not-yet-acked delete (pending journal entry): kept.
        store
            .put("notes", "pending", &json!({"title": "x"}))
            .expect("put");
        store.delete("notes", "pending").expect("delete");
        // A live row: kept.
        store
            .apply_remote_rows(&[remote_row("notes", "live", 4, false)])
            .expect("apply live");

        let removed = store.prune_acked_tombstones(5).expect("prune");
        assert_eq!(removed, 1, "only the acked sub-horizon tombstone goes");

        let live: Vec<(String, serde_json::Value)> = store.list("notes").expect("list");
        assert_eq!(live.len(), 1, "live rows survive pruning");
        assert_eq!(live[0].0, "live");
        assert_eq!(
            store.pending_count().expect("count"),
            1,
            "the pending (coalesced put+delete) journal entry is untouched"
        );
        // Pruning is idempotent.
        assert_eq!(store.prune_acked_tombstones(5).expect("re-prune"), 0);
    }

    #[test]
    // The intermediate Vec is load-bearing: all four openers must be
    // spawned (racing) before any is joined.
    #[allow(clippy::needless_collect)]
    fn concurrent_first_opens_agree_on_one_device_id() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("sync.db");
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || {
                    SyncStore::open(&path)
                        .expect("open")
                        .device_id()
                        .expect("device id")
                })
            })
            .collect();
        let ids: Vec<String> = handles
            .into_iter()
            .map(|h| h.join().expect("join"))
            .collect();
        assert!(
            ids.windows(2).all(|w| w[0] == w[1]),
            "every concurrent opener must see the same persisted device id: {ids:?}"
        );
        // And a reopen still agrees.
        let reopened = SyncStore::open(&path).expect("reopen");
        assert_eq!(reopened.device_id().expect("device id"), ids[0]);
    }
}
