//! Server side of the sync protocol: backends + mountable router.
//!
//! Mount on the remote Autumn app:
//!
//! ```rust,no_run
//! use std::sync::Arc;
//! use autumn_web::sync::{LwwResolver, PgSyncBackend, server};
//!
//! # async fn wire(database_url: String) {
//! let backend = PgSyncBackend::new(database_url);
//! backend.ensure_schema().expect("sync schema");
//! autumn_web::app()
//!     .nest("/sync", server::router(Arc::new(backend), Arc::new(LwwResolver)))
//!     .run()
//!     .await;
//! # }
//! ```

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::Query;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use chrono::{DateTime, Utc};
use diesel::prelude::*;
use diesel::sql_query;
use diesel::sql_types::{BigInt, Bool, Jsonb, Nullable, Text, Timestamptz};

use super::SyncError;
use super::protocol::{
    Change, ChangeOutcome, MAX_PULL_LIMIT, MAX_PUSH_CHANGES, Op, PullQuery, PullResponse,
    PushRequest, PushResponse, RemoteRow, Version,
};
use super::resolver::{ConflictResolver, Resolution};

fn backend_err(err: impl std::fmt::Display) -> SyncError {
    SyncError::Backend(err.to_string())
}

/// Validate a push batch before applying anything: every upsert must carry
/// `Some(payload)`. An upsert with a missing payload would create a **live**
/// server row with a NULL payload — clients pulling it materialize an
/// invisible row (the store's `get`/`list` treat a `None` payload as absent)
/// while still advancing their cursor past it, so the bad state silently
/// replicates everywhere. Rejected up front as a protocol violation; the
/// batch is atomic, so nothing is applied.
///
/// Shared by both backends so they stay conformant (see the conformance
/// suite), and surfaced by the router as a 4xx response.
fn validate_push(request: &PushRequest) -> Result<(), SyncError> {
    for change in &request.changes {
        if change.op == Op::Upsert && change.payload.is_none() {
            return Err(SyncError::Protocol(format!(
                "upsert change {} ({}/{}) has no payload — upserts must carry \
                 a JSON payload (only deletes may omit it); nothing from this \
                 batch was applied",
                change.change_id, change.collection, change.pk
            )));
        }
    }
    Ok(())
}

/// Build the [`RemoteRow`] a cleanly applied (or client-winning) change
/// produces.
fn row_from_change(change: &Change, device_id: &str, version: Version) -> RemoteRow {
    RemoteRow {
        collection: change.collection.clone(),
        pk: change.pk.clone(),
        payload: if change.op == Op::Delete {
            None
        } else {
            change.payload.clone()
        },
        version,
        deleted: change.op == Op::Delete,
        updated_at: change.updated_at,
        device_id: device_id.to_owned(),
    }
}

/// Build the post-resolution row for a conflicting change. Every branch
/// assigns the fresh `version` so all devices (including the loser)
/// converge on the resolved state via their next pull.
fn resolved_row(
    resolution: Resolution,
    change: &Change,
    device_id: &str,
    server: &RemoteRow,
    version: Version,
) -> RemoteRow {
    match resolution {
        Resolution::KeepServer => RemoteRow {
            version,
            ..server.clone()
        },
        Resolution::TakeClient => row_from_change(change, device_id, version),
        Resolution::Merge(payload) => RemoteRow {
            collection: change.collection.clone(),
            pk: change.pk.clone(),
            payload: Some(payload),
            version,
            deleted: false,
            updated_at: change.updated_at.max(server.updated_at),
            device_id: device_id.to_owned(),
        },
    }
}

/// Storage backend for the server sync endpoints.
///
/// Implementations must apply each push batch atomically (all-or-nothing)
/// and dedup on `(device_id, change_id)` so retries are idempotent.
/// Methods are synchronous; the router runs them on the blocking pool.
pub trait SyncBackend: Send + Sync + 'static {
    /// Apply one push batch atomically, returning one outcome per change
    /// (request order). Conflicts are settled via `resolver` and assigned a
    /// new version.
    ///
    /// # Errors
    ///
    /// Returns [`SyncError::Protocol`] when the batch violates the protocol
    /// — an [`Op::Upsert`] change without `Some(payload)` — with nothing
    /// applied (the router surfaces this as a 4xx response), or
    /// [`SyncError::Backend`] on storage failure (the whole batch rolls
    /// back).
    fn apply_push(
        &self,
        request: &PushRequest,
        resolver: &dyn ConflictResolver,
    ) -> Result<PushResponse, SyncError>;

    /// Return rows with version greater than `cursor` (ascending, at most
    /// `limit`), or `FullResyncRequired` when a non-zero `session_start`
    /// predates the tombstone GC horizon.
    ///
    /// `session_start` is the cursor the client's sync session started
    /// from (all pages of one paginated catch-up pass the same value). The
    /// staleness check keys on it — not on the per-page `cursor` — so a
    /// multi-page catch-up from `0` is never mistaken for a stale client
    /// just because an intermediate page cursor is still below the
    /// horizon. Single-shot callers pass `session_start == cursor`.
    ///
    /// # Errors
    ///
    /// Returns [`SyncError::Backend`] on storage failure.
    fn pull_since(
        &self,
        cursor: Version,
        limit: i64,
        session_start: Version,
    ) -> Result<PullResponse, SyncError>;

    /// Physically drop tombstone rows with version at or below `up_to` and
    /// advance the tombstone horizon. Returns the number of rows removed.
    /// Clients whose cursor then trails the horizon get a full resync.
    /// Never runs implicitly — call it deliberately (e.g. from a scheduled
    /// task) once all active devices are expected to have synced past
    /// `up_to`.
    ///
    /// The **persisted horizon is clamped to the latest assigned version**:
    /// a maintenance job may pass an arbitrarily large `up_to` (e.g.
    /// `i64::MAX`) to mean "everything so far", and without the clamp the
    /// horizon would exceed the version sequence — clients set their cursor
    /// to `max(next_cursor, tombstone_horizon)` after a completed pull, so
    /// an above-sequence horizon would push cursors past versions the
    /// server has yet to assign and those clients would permanently miss
    /// every row created later with a version at or below the horizon.
    ///
    /// # Errors
    ///
    /// Returns [`SyncError::Backend`] on storage failure.
    fn gc_tombstones(&self, up_to: Version) -> Result<u64, SyncError>;

    /// Drop push-dedup records older than `older_than` so the dedup store
    /// (`autumn_sync_applied` on Postgres) does not grow forever. Returns
    /// the number of records removed. Never runs implicitly — pair it with
    /// your [`Self::gc_tombstones`] schedule, and keep the retention window
    /// **longer than any client's plausible offline retry horizon**: a
    /// device retrying a change whose dedup record was GC'd will re-apply
    /// it (content-idempotent for upserts/deletes, but it bumps the row
    /// version and, under a custom resolver, may re-run resolution).
    ///
    /// # Errors
    ///
    /// Returns [`SyncError::Backend`] on storage failure.
    fn gc_applied(&self, older_than: DateTime<Utc>) -> Result<u64, SyncError>;

    /// The current tombstone GC horizon (`0` if GC never ran).
    ///
    /// # Errors
    ///
    /// Returns [`SyncError::Backend`] on storage failure.
    fn tombstone_horizon(&self) -> Result<Version, SyncError>;

    /// The most recently assigned version (`0` if nothing was ever pushed).
    ///
    /// # Errors
    ///
    /// Returns [`SyncError::Backend`] on storage failure.
    fn latest_version(&self) -> Result<Version, SyncError>;
}

/// Push-dedup record: the version a change was assigned when first
/// applied, and when — mirrors a row of `autumn_sync_applied` on Postgres.
#[derive(Debug, Clone, Copy)]
struct AppliedRecord {
    version: Version,
    applied_at: DateTime<Utc>,
}

#[derive(Debug, Default)]
struct MemoryState {
    rows: BTreeMap<(String, String), RemoteRow>,
    applied: HashMap<(String, String), AppliedRecord>,
    next_version: Version,
    horizon: Version,
}

impl MemoryState {
    const fn allocate_version(&mut self) -> Version {
        self.next_version += 1;
        self.next_version
    }
}

/// In-memory [`SyncBackend`] for tests, demos, and single-process setups.
/// Semantically identical to [`PgSyncBackend`] (both pass the same
/// conformance suite) but nothing survives a restart.
#[derive(Debug, Default)]
pub struct MemorySyncBackend {
    state: Mutex<MemoryState>,
}

impl MemorySyncBackend {
    /// An empty in-memory backend.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, MemoryState>, SyncError> {
        self.state
            .lock()
            .map_err(|_| SyncError::Backend("memory backend mutex poisoned".into()))
    }
}

impl SyncBackend for MemorySyncBackend {
    fn apply_push(
        &self,
        request: &PushRequest,
        resolver: &dyn ConflictResolver,
    ) -> Result<PushResponse, SyncError> {
        validate_push(request)?;
        // One lock scope = one atomic batch, mirroring the PG transaction.
        let mut state = self.lock()?;
        let mut outcomes = Vec::with_capacity(request.changes.len());
        for change in &request.changes {
            let dedup_key = (request.device_id.clone(), change.change_id.clone());
            if let Some(record) = state.applied.get(&dedup_key) {
                outcomes.push(ChangeOutcome::AlreadyApplied {
                    version: record.version,
                });
                continue;
            }
            let row_key = (change.collection.clone(), change.pk.clone());
            let current = state.rows.get(&row_key).cloned();
            let (outcome, version) = match current {
                Some(server) if server.version != change.base_version => {
                    let resolution = resolver.resolve(&request.device_id, change, &server);
                    let version = state.allocate_version();
                    let row =
                        resolved_row(resolution, change, &request.device_id, &server, version);
                    state.rows.insert(row_key, row.clone());
                    (ChangeOutcome::Resolved { row }, version)
                }
                _ => {
                    let version = state.allocate_version();
                    let row = row_from_change(change, &request.device_id, version);
                    state.rows.insert(row_key, row);
                    (ChangeOutcome::Applied { version }, version)
                }
            };
            state.applied.insert(
                dedup_key,
                AppliedRecord {
                    version,
                    applied_at: Utc::now(),
                },
            );
            outcomes.push(outcome);
        }
        drop(state);
        Ok(PushResponse { outcomes })
    }

    fn pull_since(
        &self,
        cursor: Version,
        limit: i64,
        session_start: Version,
    ) -> Result<PullResponse, SyncError> {
        let state = self.lock()?;
        if session_start > 0 && session_start < state.horizon {
            return Ok(PullResponse::FullResyncRequired {
                tombstone_horizon: state.horizon,
            });
        }
        let mut rows: Vec<RemoteRow> = state
            .rows
            .values()
            .filter(|row| row.version > cursor)
            .cloned()
            .collect();
        rows.sort_by_key(|row| row.version);
        rows.truncate(usize::try_from(limit.max(0)).unwrap_or(usize::MAX));
        let next_cursor = rows.last().map_or(cursor, |row| row.version);
        Ok(PullResponse::Ok {
            rows,
            next_cursor,
            tombstone_horizon: state.horizon,
        })
    }

    fn gc_tombstones(&self, up_to: Version) -> Result<u64, SyncError> {
        let mut state = self.lock()?;
        // Clamp to the latest assigned version (see the trait docs): a
        // horizon above the sequence would push client cursors past
        // versions the server has yet to assign, permanently hiding every
        // row created later with a version at or below the horizon.
        let up_to = up_to.min(state.next_version);
        let before = state.rows.len();
        state
            .rows
            .retain(|_, row| !(row.deleted && row.version <= up_to));
        let removed = before - state.rows.len();
        state.horizon = state.horizon.max(up_to);
        drop(state);
        Ok(removed as u64)
    }

    fn gc_applied(&self, older_than: DateTime<Utc>) -> Result<u64, SyncError> {
        let mut state = self.lock()?;
        let before = state.applied.len();
        state
            .applied
            .retain(|_, record| record.applied_at >= older_than);
        let removed = before - state.applied.len();
        drop(state);
        Ok(removed as u64)
    }

    fn tombstone_horizon(&self) -> Result<Version, SyncError> {
        Ok(self.lock()?.horizon)
    }

    fn latest_version(&self) -> Result<Version, SyncError> {
        Ok(self.lock()?.next_version)
    }
}

// ── Postgres backend ─────────────────────────────────────────────────────

/// Idempotent DDL for the server-side shadow tables. Deliberately not part
/// of the framework migrations, so apps that never mount the sync router
/// see zero schema churn.
const PG_SCHEMA_DDL: &str = "
CREATE SEQUENCE IF NOT EXISTS autumn_sync_version_seq;
CREATE TABLE IF NOT EXISTS autumn_sync_rows (
    collection TEXT NOT NULL,
    pk TEXT NOT NULL,
    payload JSONB,
    version BIGINT NOT NULL,
    deleted BOOLEAN NOT NULL DEFAULT FALSE,
    updated_at TIMESTAMPTZ NOT NULL,
    device_id TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (collection, pk)
);
CREATE INDEX IF NOT EXISTS autumn_sync_rows_version_idx
    ON autumn_sync_rows (version);
CREATE TABLE IF NOT EXISTS autumn_sync_applied (
    device_id TEXT NOT NULL,
    change_id TEXT NOT NULL,
    version BIGINT NOT NULL DEFAULT 0,
    applied_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (device_id, change_id)
);
CREATE TABLE IF NOT EXISTS autumn_sync_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
";

const META_HORIZON: &str = "tombstone_horizon";

/// Well-known key for the transaction-scoped Postgres advisory lock
/// ([`pg_advisory_xact_lock`]) taken at the top of every
/// [`PgSyncBackend::apply_push`] transaction. The value is the ASCII bytes
/// `"ATMNSYNC"` as a big-endian `i64` — stable, documented, and unlikely to
/// collide with application locks.
///
/// The lock serializes push batches, which is **load-bearing** for two
/// correctness guarantees that Postgres READ COMMITTED semantics alone do
/// not give:
///
/// 1. **No skipped versions on pull.** Versions come from a sequence, and
///    sequences are non-transactional: without the lock, a push holding
///    version 5 can commit *after* a push holding version 6, and a pull in
///    between sees only 6, persists `cursor = 6`, and never receives row 5.
///    With the lock, pushes commit in version order, so any READ COMMITTED
///    pull observes a clean version prefix and `max(version seen)` is a
///    safe cursor.
/// 2. **Concurrent first-inserts of one pk engage the resolver.** `SELECT
///    … FOR UPDATE` locks nothing for a row that does not exist yet, so two
///    concurrent transactions both creating pk X would each take the clean
///    `Applied` path and the later `ON CONFLICT … DO UPDATE` would silently
///    overwrite the earlier write without running the [`ConflictResolver`].
///    Serialized pushes make the second transaction see the first's
///    committed row and route through the conflict path.
///
/// [`pg_advisory_xact_lock`]:
///     https://www.postgresql.org/docs/current/functions-admin.html#FUNCTIONS-ADVISORY-LOCKS
const PG_PUSH_ADVISORY_LOCK_KEY: i64 = 0x4154_4D4E_5359_4E43; // "ATMNSYNC"

#[derive(QueryableByName)]
struct PgRowRecord {
    #[diesel(sql_type = Text)]
    collection: String,
    #[diesel(sql_type = Text)]
    pk: String,
    #[diesel(sql_type = Nullable<Jsonb>)]
    payload: Option<serde_json::Value>,
    #[diesel(sql_type = BigInt)]
    version: i64,
    #[diesel(sql_type = Bool)]
    deleted: bool,
    #[diesel(sql_type = Timestamptz)]
    updated_at: DateTime<Utc>,
    #[diesel(sql_type = Text)]
    device_id: String,
}

impl PgRowRecord {
    fn into_remote_row(self) -> RemoteRow {
        RemoteRow {
            collection: self.collection,
            pk: self.pk,
            payload: self.payload,
            version: self.version,
            deleted: self.deleted,
            updated_at: self.updated_at,
            device_id: self.device_id,
        }
    }
}

#[derive(QueryableByName)]
struct PgVersionRecord {
    #[diesel(sql_type = BigInt)]
    version: i64,
}

#[derive(QueryableByName)]
struct PgMetaRecord {
    #[diesel(sql_type = Text)]
    value: String,
}

#[derive(QueryableByName)]
struct PgSequenceRecord {
    #[diesel(sql_type = BigInt)]
    last_value: i64,
    #[diesel(sql_type = Bool)]
    is_called: bool,
}

fn pg_upsert_row(conn: &mut PgConnection, row: &RemoteRow) -> Result<(), diesel::result::Error> {
    sql_query(
        "INSERT INTO autumn_sync_rows \
         (collection, pk, payload, version, deleted, updated_at, device_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) \
         ON CONFLICT (collection, pk) DO UPDATE SET \
         payload = excluded.payload, version = excluded.version, \
         deleted = excluded.deleted, updated_at = excluded.updated_at, \
         device_id = excluded.device_id",
    )
    .bind::<Text, _>(&row.collection)
    .bind::<Text, _>(&row.pk)
    .bind::<Nullable<Jsonb>, _>(&row.payload)
    .bind::<BigInt, _>(row.version)
    .bind::<Bool, _>(row.deleted)
    .bind::<Timestamptz, _>(row.updated_at)
    .bind::<Text, _>(&row.device_id)
    .execute(conn)
    .map(|_| ())
}

fn pg_next_version(conn: &mut PgConnection) -> Result<Version, diesel::result::Error> {
    sql_query("SELECT nextval('autumn_sync_version_seq') AS version")
        .get_result::<PgVersionRecord>(conn)
        .map(|record| record.version)
}

fn pg_horizon(conn: &mut PgConnection) -> Result<Version, diesel::result::Error> {
    let record = sql_query("SELECT value FROM autumn_sync_meta WHERE key = $1")
        .bind::<Text, _>(META_HORIZON)
        .get_result::<PgMetaRecord>(conn)
        .optional()?;
    Ok(record.and_then(|r| r.value.parse().ok()).unwrap_or(0))
}

/// The most recently assigned version — the sequence's `last_value` once it
/// has been called, `0` on a fresh backend.
fn pg_latest_version(conn: &mut PgConnection) -> Result<Version, diesel::result::Error> {
    let record = sql_query("SELECT last_value, is_called FROM autumn_sync_version_seq")
        .get_result::<PgSequenceRecord>(conn)?;
    Ok(if record.is_called {
        record.last_value
    } else {
        0
    })
}

/// Postgres-backed [`SyncBackend`] persisting into shadow tables.
///
/// State lives in `autumn_sync_rows` / `autumn_sync_applied` /
/// `autumn_sync_meta`. Each push batch applies in one transaction;
/// versions come from the `autumn_sync_version_seq` sequence, making the
/// row table itself the change feed.
#[derive(Debug, Clone)]
pub struct PgSyncBackend {
    database_url: String,
}

impl PgSyncBackend {
    /// A backend connecting to `database_url`. Call [`Self::ensure_schema`]
    /// once at startup before serving.
    #[must_use]
    pub fn new(database_url: impl Into<String>) -> Self {
        Self {
            database_url: database_url.into(),
        }
    }

    fn connect(&self) -> Result<PgConnection, SyncError> {
        PgConnection::establish(&self.database_url).map_err(backend_err)
    }

    /// Create the shadow tables and the version sequence if they do not
    /// exist. Idempotent (`CREATE ... IF NOT EXISTS` throughout);
    /// deliberately not part of the framework migrations so non-sync apps
    /// see zero schema churn. Blocking — run at startup (or inside
    /// `spawn_blocking`).
    ///
    /// # Errors
    ///
    /// Returns [`SyncError::Backend`] if the connection fails or the DDL
    /// cannot be applied.
    pub fn ensure_schema(&self) -> Result<(), SyncError> {
        use diesel::connection::SimpleConnection;
        let mut conn = self.connect()?;
        conn.batch_execute(PG_SCHEMA_DDL).map_err(backend_err)
    }
}

impl SyncBackend for PgSyncBackend {
    fn apply_push(
        &self,
        request: &PushRequest,
        resolver: &dyn ConflictResolver,
    ) -> Result<PushResponse, SyncError> {
        validate_push(request)?;
        let mut conn = self.connect()?;
        conn.transaction::<_, diesel::result::Error, _>(|conn| {
            // Serialize push batches (held until commit). See the doc
            // comment on PG_PUSH_ADVISORY_LOCK_KEY for why this is
            // required for correctness, not just politeness.
            sql_query("SELECT pg_advisory_xact_lock($1)")
                .bind::<BigInt, _>(PG_PUSH_ADVISORY_LOCK_KEY)
                .execute(conn)?;
            let mut outcomes = Vec::with_capacity(request.changes.len());
            for change in &request.changes {
                let duplicate = sql_query(
                    "SELECT version FROM autumn_sync_applied \
                     WHERE device_id = $1 AND change_id = $2",
                )
                .bind::<Text, _>(&request.device_id)
                .bind::<Text, _>(&change.change_id)
                .get_result::<PgVersionRecord>(conn)
                .optional()?;
                if let Some(record) = duplicate {
                    outcomes.push(ChangeOutcome::AlreadyApplied {
                        version: record.version,
                    });
                    continue;
                }

                let current = sql_query(
                    "SELECT collection, pk, payload, version, deleted, updated_at, device_id \
                     FROM autumn_sync_rows WHERE collection = $1 AND pk = $2 FOR UPDATE",
                )
                .bind::<Text, _>(&change.collection)
                .bind::<Text, _>(&change.pk)
                .get_result::<PgRowRecord>(conn)
                .optional()?
                .map(PgRowRecord::into_remote_row);

                let (outcome, version) = match current {
                    Some(server) if server.version != change.base_version => {
                        let resolution = resolver.resolve(&request.device_id, change, &server);
                        let version = pg_next_version(conn)?;
                        let row =
                            resolved_row(resolution, change, &request.device_id, &server, version);
                        pg_upsert_row(conn, &row)?;
                        (ChangeOutcome::Resolved { row }, version)
                    }
                    _ => {
                        let version = pg_next_version(conn)?;
                        let row = row_from_change(change, &request.device_id, version);
                        pg_upsert_row(conn, &row)?;
                        (ChangeOutcome::Applied { version }, version)
                    }
                };

                sql_query(
                    "INSERT INTO autumn_sync_applied (device_id, change_id, version) \
                     VALUES ($1, $2, $3)",
                )
                .bind::<Text, _>(&request.device_id)
                .bind::<Text, _>(&change.change_id)
                .bind::<BigInt, _>(version)
                .execute(conn)?;
                outcomes.push(outcome);
            }
            Ok(PushResponse { outcomes })
        })
        .map_err(backend_err)
    }

    fn pull_since(
        &self,
        cursor: Version,
        limit: i64,
        session_start: Version,
    ) -> Result<PullResponse, SyncError> {
        let mut conn = self.connect()?;
        conn.transaction::<_, diesel::result::Error, _>(|conn| {
            let horizon = pg_horizon(conn)?;
            if session_start > 0 && session_start < horizon {
                return Ok(PullResponse::FullResyncRequired {
                    tombstone_horizon: horizon,
                });
            }
            let rows: Vec<RemoteRow> = sql_query(
                "SELECT collection, pk, payload, version, deleted, updated_at, device_id \
                 FROM autumn_sync_rows WHERE version > $1 ORDER BY version LIMIT $2",
            )
            .bind::<BigInt, _>(cursor)
            .bind::<BigInt, _>(limit.max(0))
            .get_results::<PgRowRecord>(conn)?
            .into_iter()
            .map(PgRowRecord::into_remote_row)
            .collect();
            let next_cursor = rows.last().map_or(cursor, |row| row.version);
            Ok(PullResponse::Ok {
                rows,
                next_cursor,
                tombstone_horizon: horizon,
            })
        })
        .map_err(backend_err)
    }

    fn gc_tombstones(&self, up_to: Version) -> Result<u64, SyncError> {
        let mut conn = self.connect()?;
        conn.transaction::<_, diesel::result::Error, _>(|conn| {
            // Clamp to the latest assigned version (see the trait docs): a
            // horizon above the sequence would push client cursors past
            // versions the server has yet to assign, permanently hiding
            // every row created later with a version at or below the
            // horizon.
            let up_to = up_to.min(pg_latest_version(conn)?);
            let removed = sql_query("DELETE FROM autumn_sync_rows WHERE deleted AND version <= $1")
                .bind::<BigInt, _>(up_to)
                .execute(conn)?;
            let horizon = pg_horizon(conn)?.max(up_to);
            sql_query(
                "INSERT INTO autumn_sync_meta (key, value) VALUES ($1, $2) \
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            )
            .bind::<Text, _>(META_HORIZON)
            .bind::<Text, _>(horizon.to_string())
            .execute(conn)?;
            Ok(removed as u64)
        })
        .map_err(backend_err)
    }

    fn gc_applied(&self, older_than: DateTime<Utc>) -> Result<u64, SyncError> {
        let mut conn = self.connect()?;
        let removed = sql_query("DELETE FROM autumn_sync_applied WHERE applied_at < $1")
            .bind::<Timestamptz, _>(older_than)
            .execute(&mut conn)
            .map_err(backend_err)?;
        Ok(removed as u64)
    }

    fn tombstone_horizon(&self) -> Result<Version, SyncError> {
        let mut conn = self.connect()?;
        pg_horizon(&mut conn).map_err(backend_err)
    }

    fn latest_version(&self) -> Result<Version, SyncError> {
        let mut conn = self.connect()?;
        pg_latest_version(&mut conn).map_err(backend_err)
    }
}

// ── Router ───────────────────────────────────────────────────────────────

/// Build the sync router (`POST /push`, `GET /pull`).
///
/// Generic over the host router's state so it mounts both on an Autumn app
/// (`AppBuilder::nest("/sync", router(...))` shares [`crate::AppState`] and
/// the app's global middleware) and on a bare `axum::Router` in tests.
/// **Mount it behind authentication** — the endpoints trust `device_id` as
/// sent, and anyone who can reach them can read and write every synced row.
///
/// Request bounds and validation: push batches larger than
/// [`MAX_PUSH_CHANGES`](crate::sync::protocol::MAX_PUSH_CHANGES) changes
/// are rejected with `413` (request bodies are additionally capped by
/// axum's default body limit), batches containing an upsert without a
/// payload are rejected with `422` (nothing applied — see
/// [`SyncBackend::apply_push`]), and the pull `limit` is clamped to at most
/// [`MAX_PULL_LIMIT`](crate::sync::protocol::MAX_PULL_LIMIT) (minimum 1).
pub fn router<S>(
    backend: Arc<dyn SyncBackend>,
    resolver: Arc<dyn ConflictResolver>,
) -> axum::Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    let push_backend = Arc::clone(&backend);
    axum::Router::new()
        .route(
            "/push",
            post(move |Json(request): Json<PushRequest>| {
                let backend = Arc::clone(&push_backend);
                let resolver = Arc::clone(&resolver);
                async move {
                    if request.changes.len() > MAX_PUSH_CHANGES {
                        return (
                            StatusCode::PAYLOAD_TOO_LARGE,
                            format!(
                                "push batch of {} changes exceeds the limit of {MAX_PUSH_CHANGES}",
                                request.changes.len()
                            ),
                        )
                            .into_response();
                    }
                    let result = tokio::task::spawn_blocking(move || {
                        backend.apply_push(&request, resolver.as_ref())
                    })
                    .await;
                    respond(result)
                }
            }),
        )
        .route(
            "/pull",
            get(move |Query(query): Query<PullQuery>| {
                let backend = Arc::clone(&backend);
                async move {
                    let limit = query.limit.clamp(1, MAX_PULL_LIMIT);
                    let session_start = query.session_start();
                    let result = tokio::task::spawn_blocking(move || {
                        backend.pull_since(query.cursor, limit, session_start)
                    })
                    .await;
                    respond(result)
                }
            }),
        )
}

fn respond<T: serde::Serialize>(
    result: Result<Result<T, SyncError>, tokio::task::JoinError>,
) -> axum::response::Response {
    match result {
        Ok(Ok(response)) => Json(response).into_response(),
        // Protocol violations (e.g. an upsert without a payload) are CLIENT
        // errors: answer 422 so the pushing device surfaces the bug instead
        // of retrying a batch the server will never accept.
        Ok(Err(err @ SyncError::Protocol(_))) => {
            tracing::warn!(error = %err, "sync request rejected");
            (StatusCode::UNPROCESSABLE_ENTITY, err.to_string()).into_response()
        }
        Ok(Err(err)) => {
            tracing::error!(error = %err, "sync request failed");
            (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response()
        }
        Err(err) => {
            tracing::error!(error = %err, "sync request panicked");
            (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response()
        }
    }
}
