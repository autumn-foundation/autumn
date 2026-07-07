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

/// The local, in-process `SQLite` store for offline data.
///
/// App data is stored as JSON payloads keyed by `(collection, pk)` —
/// anything `Serialize + DeserializeOwned` round-trips. Every write also
/// journals a pending change **in the same `SQLite` transaction** (a crash
/// cannot lose a journal entry), so the [`crate::sync::SyncEngine`] can
/// replay it to the server later. Deletes are recorded as tombstone rows so
/// they replicate too.
///
/// Cheap to clone: all clones share one WAL-mode connection behind a mutex,
/// which serializes concurrent in-process writers.
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
        conn.batch_execute(
            "PRAGMA journal_mode = WAL; \
             PRAGMA busy_timeout = 5000; \
             PRAGMA synchronous = NORMAL; \
             PRAGMA foreign_keys = ON;",
        )
        .map_err(store_err)?;
        conn.batch_execute(SCHEMA_DDL).map_err(store_err)?;

        let device_id = if let Some(id) = get_state(&mut conn, STATE_DEVICE_ID)? {
            id
        } else {
            let id = uuid::Uuid::new_v4().to_string();
            set_state(&mut conn, STATE_DEVICE_ID, &id).map_err(store_err)?;
            id
        };

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
        conn.transaction::<_, diesel::result::Error, _>(|conn| {
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

    pub(crate) fn set_cursor(&self, cursor: Version) -> Result<(), SyncError> {
        let mut conn = self.lock()?;
        set_state(&mut conn, STATE_CURSOR, &cursor.to_string()).map_err(store_err)
    }

    /// Settle a pushed batch: journal entries are cleared **by change id**
    /// (a newer coalesced write keeps its own entry), acked versions are
    /// recorded, and conflict resolutions are applied locally.
    pub(crate) fn confirm_pushed(
        &self,
        changes: &[Change],
        outcomes: &[ChangeOutcome],
    ) -> Result<(), SyncError> {
        let mut conn = self.lock()?;
        conn.transaction::<_, diesel::result::Error, _>(|conn| {
            for (change, outcome) in changes.iter().zip(outcomes) {
                match outcome {
                    ChangeOutcome::Applied { version } => {
                        sql_query(
                            "UPDATE autumn_sync_rows SET server_version = ? \
                             WHERE collection = ? AND pk = ?",
                        )
                        .bind::<BigInt, _>(*version)
                        .bind::<Text, _>(&change.collection)
                        .bind::<Text, _>(&change.pk)
                        .execute(conn)?;
                    }
                    ChangeOutcome::AlreadyApplied => {}
                    ChangeOutcome::Resolved { row } => {
                        upsert_remote_row(conn, row)?;
                    }
                }
                sql_query("DELETE FROM autumn_sync_pending WHERE change_id = ?")
                    .bind::<Text, _>(&change.change_id)
                    .execute(conn)?;
            }
            Ok(())
        })
        .map_err(store_err)
    }

    /// Apply a pulled page of remote rows. Idempotent versioned upsert;
    /// rows with a pending local change are skipped (the local write wins
    /// until its push settles the conflict). Returns how many rows applied.
    pub(crate) fn apply_remote_rows(&self, rows: &[RemoteRow]) -> Result<usize, SyncError> {
        let mut conn = self.lock()?;
        conn.transaction::<_, diesel::result::Error, _>(|conn| {
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
        })
        .map_err(store_err)
    }

    /// Drop all synced local state ahead of a full re-pull from cursor `0`.
    /// Rows with a pending journal entry are preserved so unsynced local
    /// writes survive and get replayed.
    pub(crate) fn begin_full_resync(&self) -> Result<(), SyncError> {
        let mut conn = self.lock()?;
        conn.transaction::<_, diesel::result::Error, _>(|conn| {
            sql_query(
                "DELETE FROM autumn_sync_rows WHERE NOT EXISTS (\
                 SELECT 1 FROM autumn_sync_pending p \
                 WHERE p.collection = autumn_sync_rows.collection \
                 AND p.pk = autumn_sync_rows.pk)",
            )
            .execute(conn)?;
            set_state(conn, STATE_CURSOR, "0")
        })
        .map_err(store_err)
    }
}
