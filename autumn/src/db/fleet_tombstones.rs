//! Replica tombstones of a fleet (issue #3235, ADR 0019 §8).
//!
//! A deleted database's replica goes with it. A tombstone row on the control
//! database records the delete until the replica is purged:
//!
//! ```text
//! delete  ─► record row (pending; reset when the file is there)
//!         ─► remove files ─► confirm own row (deleted_at); row gone: stop
//!            └ refused: no file ──► drop the pending row
//!         ─► marker <root>/tombstone ─► retention 0: purge now
//!            purge ok ──► drop row      purge failed ──► backoff, retry task
//! open    ─► row, or a marker not older than the newest generation:
//!            restore_missing restores nothing
//! create  ─► row in its retention: on demand 410, provision purges first
//! restore ─► explicit: take the row, restore, drop the marker; failed: put
//!            the row back
//! retry   ─► every minute, every row: write a missing marker, purge a due
//!            row, drop a row next to a live file
//! ```
//!
//! The rules are pure functions here. The model and its proofs are in
//! `verification/fleet_replica_tombstones.rs`.

// autumn-determinism-gate: production code in this module must read time and
// mint identifiers through the framework's injected seams (ClockSource /
// Entropy), never `Instant::now()` / `Utc::now()` / `SystemTime::now()` /
// `Uuid::new_v4()` directly. See CONTRIBUTING.md "Determinism seam gate"
// (issue #1797). Justify exceptions with
// #[allow(clippy::disallowed_methods, reason = "…")] at the narrowest scope.
#![cfg_attr(not(test), deny(clippy::disallowed_methods))]

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeZone as _, Utc};
use diesel::sql_types::{BigInt, Integer, Nullable, Text};
use diesel_async::RunQueryDsl as _;
use diesel_async::SimpleAsyncConnection as _;
use diesel_async::pooled_connection::deadpool::Pool;

use crate::db::RuntimeConnection;
use crate::time::ClockSource;

/// The tombstone table on the control database.
pub const TOMBSTONE_TABLE: &str = "_autumn_fleet_tombstones";

/// The object name of the marker under a replica root.
pub const MARKER_OBJECT: &str = "tombstone";

/// The first retry delay after a failed purge.
pub const RETRY_BASE: Duration = Duration::from_secs(5);

/// The longest retry delay.
pub const RETRY_CAP: Duration = Duration::from_secs(3600);

/// A pending row older than this, with its file still there, is from a
/// delete that did not finish. The retry task drops it.
pub const STALE_PENDING: Duration = Duration::from_secs(600);

/// The DDL, applied on first use. Times are epoch milliseconds of the app
/// clock.
const SCHEMA_SQL: &str = "\
CREATE TABLE IF NOT EXISTS _autumn_fleet_tombstones (
    key          TEXT    NOT NULL PRIMARY KEY,
    deleted_at   BIGINT  NOT NULL,
    confirmed    INTEGER NOT NULL DEFAULT 0,
    purge_after  BIGINT  NOT NULL,
    attempts     INTEGER NOT NULL DEFAULT 0,
    next_attempt BIGINT  NOT NULL DEFAULT 0,
    last_error   TEXT,
    marked       INTEGER NOT NULL DEFAULT 0,
    owner        TEXT    NOT NULL DEFAULT ''
);";

// ── Rules ───────────────────────────────────────────────────────────────────

/// The tombstone of one deleted database.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Tombstone {
    /// The database name, as [`FleetDbKey::name`](crate::fleet_layout::FleetDbKey::name)
    /// gives it.
    pub key: String,
    /// The purge cutoff: when the files were removed, or the newest
    /// generation of the deleted database when that is later. A generation
    /// created later belongs to a new database with the same key.
    pub deleted_at: DateTime<Utc>,
    /// `false` while the delete still removes the files.
    pub confirmed: bool,
    /// The earliest purge: the time of the removal plus the retention.
    pub purge_after: DateTime<Utc>,
    /// Failed purges so far.
    pub attempts: u32,
    /// The earliest next purge attempt.
    pub next_attempt_at: DateTime<Utc>,
    /// Why the last purge failed.
    pub last_error: Option<String>,
    /// The marker object is written. Until then, the retry task writes it.
    pub marked: bool,
    /// The delete that wrote the pending row. A refused delete drops only
    /// its own row, never the row of another delete that runs at once.
    pub owner: String,
}

impl Tombstone {
    /// A new row for `key`, deleted at `now` (`fresh` in the model).
    #[must_use]
    pub fn new(key: &str, now: DateTime<Utc>, retention: Duration, confirmed: bool) -> Self {
        Self {
            key: key.to_owned(),
            deleted_at: now,
            confirmed,
            purge_after: add(now, retention),
            attempts: 0,
            next_attempt_at: DateTime::<Utc>::UNIX_EPOCH,
            last_error: None,
            marked: false,
            owner: String::new(),
        }
    }

    /// `is_due` in the model. A pending row is never due.
    #[must_use]
    pub fn is_due(&self, now: DateTime<Utc>) -> bool {
        self.confirmed && now >= self.purge_after && now >= self.next_attempt_at
    }

    /// `after_failure` in the model: one more attempt, and a wait.
    #[must_use]
    pub fn after_failure(self, now: DateTime<Utc>, error: &str) -> Self {
        Self {
            next_attempt_at: add(now, retry_delay(self.attempts)),
            attempts: self.attempts.saturating_add(1),
            last_error: Some(error.to_owned()),
            ..self
        }
    }
}

/// `delay` in the model: `min(RETRY_CAP, RETRY_BASE * 2^attempts)`.
#[must_use]
pub fn retry_delay(attempts: u32) -> Duration {
    let millis = |d: Duration| u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
    Duration::from_millis(crate::backoff::ceiling_ms(
        millis(RETRY_BASE),
        millis(RETRY_CAP),
        attempts,
    ))
}

/// `restore_allowed` in the model. `row`: a tombstone row exists. `marker`:
/// the `deleted_at` of the marker object. `newest`: when the generation a
/// restore takes was created.
#[must_use]
pub fn restore_allowed(row: bool, marker: Option<DateTime<Utc>>, newest: DateTime<Utc>) -> bool {
    !row && marker.is_none_or(|deleted_at| newest > deleted_at)
}

/// `purge_deletes` in the model: whether a purge deletes an object.
///
/// `generation`: when the object's generation was created, `None` for an
/// object outside a generation (epochs, the marker). `newer_exists`: a
/// generation newer than `cutoff` is in the replica.
#[must_use]
pub fn purge_deletes(
    generation: Option<DateTime<Utc>>,
    cutoff: DateTime<Utc>,
    newer_exists: bool,
) -> bool {
    generation.map_or(!newer_exists, |created| created <= cutoff)
}

fn add(at: DateTime<Utc>, by: Duration) -> DateTime<Utc> {
    chrono::Duration::from_std(by)
        .ok()
        .and_then(|by| at.checked_add_signed(by))
        .unwrap_or(DateTime::<Utc>::MAX_UTC)
}

/// The body of the marker object `<root>/tombstone`. Hosts that do not
/// share the control database read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Marker {
    /// [`Tombstone::deleted_at`], epoch milliseconds.
    pub deleted_at_ms: i64,
    /// [`Tombstone::purge_after`], epoch milliseconds.
    pub purge_after_ms: i64,
}

impl Marker {
    /// The marker of `tombstone`.
    #[must_use]
    pub const fn of(tombstone: &Tombstone) -> Self {
        Self {
            deleted_at_ms: tombstone.deleted_at.timestamp_millis(),
            purge_after_ms: tombstone.purge_after.timestamp_millis(),
        }
    }

    /// [`Tombstone::deleted_at`].
    #[must_use]
    pub fn deleted_at(&self) -> DateTime<Utc> {
        from_millis(self.deleted_at_ms)
    }

    /// [`Tombstone::purge_after`].
    #[must_use]
    pub fn purge_after(&self) -> DateTime<Utc> {
        from_millis(self.purge_after_ms)
    }
}

/// The result of one purge pass. See
/// [`DatabaseFleet::purge_deleted_replicas`](super::fleet::DatabaseFleet::purge_deleted_replicas).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct PurgeReport {
    /// Replicas purged.
    pub purged: usize,
    /// Purges that failed. Each one waits for its backoff.
    pub failed: usize,
    /// Rows not due yet: the retention, a backoff, a delete in progress, a
    /// slot this host does not own, or another mode's key. A dropped stale
    /// row counts in no field.
    pub waiting: usize,
    /// Rows with at least one failed purge, also the ones that wait for
    /// their backoff.
    pub failing: usize,
    /// The last error of the pass, or else the newest error a row keeps.
    pub last_error: Option<String>,
}

// ── Store ───────────────────────────────────────────────────────────────────

/// A failure of the tombstone store.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("fleet tombstones: {context}: {detail}")]
pub struct TombstoneError {
    context: &'static str,
    detail: String,
}

fn store_error(context: &'static str) -> impl Fn(diesel::result::Error) -> TombstoneError {
    move |error| TombstoneError {
        context,
        detail: error.to_string(),
    }
}

#[derive(diesel::QueryableByName)]
struct Row {
    #[diesel(sql_type = Text)]
    key: String,
    #[diesel(sql_type = BigInt)]
    deleted_at: i64,
    #[diesel(sql_type = Integer)]
    confirmed: i32,
    #[diesel(sql_type = BigInt)]
    purge_after: i64,
    #[diesel(sql_type = Integer)]
    attempts: i32,
    #[diesel(sql_type = BigInt)]
    next_attempt: i64,
    #[diesel(sql_type = Nullable<Text>)]
    last_error: Option<String>,
    #[diesel(sql_type = Integer)]
    marked: i32,
    #[diesel(sql_type = Text)]
    owner: String,
}

impl From<Row> for Tombstone {
    fn from(row: Row) -> Self {
        Self {
            key: row.key,
            deleted_at: from_millis(row.deleted_at),
            confirmed: row.confirmed != 0,
            purge_after: from_millis(row.purge_after),
            attempts: u32::try_from(row.attempts).unwrap_or(0),
            next_attempt_at: from_millis(row.next_attempt),
            last_error: row.last_error,
            marked: row.marked != 0,
            owner: row.owner,
        }
    }
}

fn from_millis(millis: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(millis)
        .single()
        .unwrap_or(DateTime::<Utc>::MIN_UTC)
}

const COLUMNS: &str =
    "key, deleted_at, confirmed, purge_after, attempts, next_attempt, last_error, marked, owner";

/// The tombstone rows on the control database.
pub struct FleetTombstones {
    control: Pool<RuntimeConnection>,
    clock: Arc<dyn ClockSource>,
    schema: tokio::sync::OnceCell<()>,
}

impl std::fmt::Debug for FleetTombstones {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FleetTombstones").finish_non_exhaustive()
    }
}

impl FleetTombstones {
    /// Tombstones on the control database behind `control`. The table is
    /// made on first use.
    #[must_use]
    pub fn new(control: Pool<RuntimeConnection>, clock: Arc<dyn ClockSource>) -> Self {
        Self {
            control,
            clock,
            schema: tokio::sync::OnceCell::new(),
        }
    }

    /// The clock that stamps the rows.
    #[must_use]
    pub fn now(&self) -> DateTime<Utc> {
        self.clock.now()
    }

    async fn conn(
        &self,
    ) -> Result<diesel_async::pooled_connection::deadpool::Object<RuntimeConnection>, TombstoneError>
    {
        let mut conn = self.control.get().await.map_err(|error| TombstoneError {
            context: "no connection",
            detail: error.to_string(),
        })?;
        self.schema
            .get_or_try_init(|| async {
                conn.batch_execute(SCHEMA_SQL)
                    .await
                    .map_err(store_error("schema"))
            })
            .await?;
        Ok(conn)
    }

    /// Write a pending row before a delete removes the files (`Record` in
    /// the model). `present`: the file is on disk. Then an existing row is
    /// stale (a restore or a re-create drops it first), and it is reset.
    /// Else an existing row is an older or a concurrent delete's, and it
    /// stays. `owner` names this delete (see [`remove_pending`](Self::remove_pending)).
    ///
    /// # Errors
    ///
    /// The control database cannot be written.
    pub async fn record(
        &self,
        key: &str,
        retention: Duration,
        present: bool,
        owner: &str,
    ) -> Result<(), TombstoneError> {
        let row = Tombstone::new(key, self.now(), retention, false);
        let conflict = if present {
            "DO UPDATE SET deleted_at = excluded.deleted_at, confirmed = 0, \
             purge_after = excluded.purge_after, attempts = 0, next_attempt = 0, \
             last_error = NULL, marked = 0, owner = excluded.owner"
        } else {
            "DO NOTHING"
        };
        let mut conn = self.conn().await?;
        diesel::sql_query(format!(
            "INSERT INTO {TOMBSTONE_TABLE} ({COLUMNS}) VALUES (?, ?, 0, ?, 0, 0, NULL, 0, ?) \
             ON CONFLICT (key) {conflict}"
        ))
        .bind::<Text, _>(&row.key)
        .bind::<BigInt, _>(row.deleted_at.timestamp_millis())
        .bind::<BigInt, _>(row.purge_after.timestamp_millis())
        .bind::<Text, _>(owner)
        .execute(&mut *conn)
        .await
        .map(drop)
        .map_err(store_error("record"))
    }

    /// Write `row` as it is. An explicit restore puts back the row it took
    /// when the restore fails.
    ///
    /// # Errors
    ///
    /// The control database cannot be written.
    pub async fn put(&self, row: &Tombstone) -> Result<(), TombstoneError> {
        let mut conn = self.conn().await?;
        diesel::sql_query(format!(
            "INSERT INTO {TOMBSTONE_TABLE} ({COLUMNS}) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT (key) DO UPDATE SET deleted_at = excluded.deleted_at, \
             confirmed = excluded.confirmed, purge_after = excluded.purge_after, \
             attempts = excluded.attempts, next_attempt = excluded.next_attempt, \
             last_error = excluded.last_error, marked = excluded.marked, \
             owner = excluded.owner"
        ))
        .bind::<Text, _>(&row.key)
        .bind::<BigInt, _>(row.deleted_at.timestamp_millis())
        .bind::<Integer, _>(i32::from(row.confirmed))
        .bind::<BigInt, _>(row.purge_after.timestamp_millis())
        .bind::<Integer, _>(i32::try_from(row.attempts).unwrap_or(i32::MAX))
        .bind::<BigInt, _>(row.next_attempt_at.timestamp_millis())
        .bind::<Nullable<Text>, _>(row.last_error.as_deref())
        .bind::<Integer, _>(i32::from(row.marked))
        .bind::<Text, _>(&row.owner)
        .execute(&mut *conn)
        .await
        .map(drop)
        .map_err(store_error("put"))
    }

    /// The files are gone: stamp `deleted_at` on the pending row of `key`
    /// (`Confirm` in the model). `deleted_at` is now, or `floor` when that
    /// is later: the newest generation of the deleted database, so a clock
    /// that is behind does not keep it. A confirmed row stays as it is.
    /// `None` when no row is left: a re-create or a restore took the key,
    /// and no cutoff may cover its new database.
    ///
    /// # Errors
    ///
    /// The control database cannot be written or read.
    pub async fn confirm(
        &self,
        key: &str,
        retention: Duration,
        floor: Option<DateTime<Utc>>,
    ) -> Result<Option<Tombstone>, TombstoneError> {
        let now = self.now();
        // The retention runs from now; the cutoff can be later.
        let mut row = Tombstone::new(key, now, retention, true);
        row.deleted_at = floor.map_or(now, |floor| floor.max(now));
        let mut conn = self.conn().await?;
        diesel::sql_query(format!(
            "UPDATE {TOMBSTONE_TABLE} SET deleted_at = ?, confirmed = 1, purge_after = ?, \
             attempts = 0, next_attempt = 0, last_error = NULL \
             WHERE key = ? AND confirmed = 0"
        ))
        .bind::<BigInt, _>(row.deleted_at.timestamp_millis())
        .bind::<BigInt, _>(row.purge_after.timestamp_millis())
        .bind::<Text, _>(&row.key)
        .execute(&mut *conn)
        .await
        .map_err(store_error("confirm"))?;
        drop(conn);
        self.get(key).await
    }

    /// Record that the marker object of `key` is written.
    ///
    /// # Errors
    ///
    /// The control database cannot be written.
    pub async fn set_marked(&self, key: &str) -> Result<(), TombstoneError> {
        let mut conn = self.conn().await?;
        diesel::sql_query(format!(
            "UPDATE {TOMBSTONE_TABLE} SET marked = 1 WHERE key = ?"
        ))
        .bind::<Text, _>(key)
        .execute(&mut *conn)
        .await
        .map(drop)
        .map_err(store_error("mark"))
    }

    /// The row of `key`.
    ///
    /// # Errors
    ///
    /// The control database cannot be read.
    pub async fn get(&self, key: &str) -> Result<Option<Tombstone>, TombstoneError> {
        let mut conn = self.conn().await?;
        let rows: Vec<Row> = diesel::sql_query(format!(
            "SELECT {COLUMNS} FROM {TOMBSTONE_TABLE} WHERE key = ?"
        ))
        .bind::<Text, _>(key)
        .load(&mut *conn)
        .await
        .map_err(store_error("read"))?;
        Ok(rows.into_iter().next().map(Tombstone::from))
    }

    /// At most `limit` rows in key order, after the key `after`. Page with
    /// the last key of the previous page.
    ///
    /// # Errors
    ///
    /// The control database cannot be read.
    pub async fn list_after(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Tombstone>, TombstoneError> {
        let mut conn = self.conn().await?;
        let rows: Vec<Row> = diesel::sql_query(format!(
            "SELECT {COLUMNS} FROM {TOMBSTONE_TABLE} WHERE key > ? ORDER BY key LIMIT ?"
        ))
        .bind::<Text, _>(after.unwrap_or(""))
        .bind::<BigInt, _>(i64::try_from(limit).unwrap_or(i64::MAX))
        .load(&mut *conn)
        .await
        .map_err(store_error("list"))?;
        Ok(rows.into_iter().map(Tombstone::from).collect())
    }

    /// Drop the row of `key` (a purge, a restore or a re-create).
    ///
    /// # Errors
    ///
    /// The control database cannot be written.
    pub async fn remove(&self, key: &str) -> Result<(), TombstoneError> {
        let mut conn = self.conn().await?;
        diesel::sql_query(format!("DELETE FROM {TOMBSTONE_TABLE} WHERE key = ?"))
            .bind::<Text, _>(key)
            .execute(&mut *conn)
            .await
            .map(drop)
            .map_err(store_error("remove"))
    }

    /// Drop the row of `key` only while it is pending (`Refused` in the
    /// model). `owner`: drop it only when this delete wrote it; then a
    /// delete that found no file keeps the row of a delete that runs at
    /// once in another process. `None`: drop any pending row (the retry
    /// task, for a stale one).
    ///
    /// # Errors
    ///
    /// The control database cannot be written.
    pub async fn remove_pending(
        &self,
        key: &str,
        owner: Option<&str>,
    ) -> Result<(), TombstoneError> {
        let mut conn = self.conn().await?;
        let query =
            match owner {
                Some(owner) => diesel::sql_query(format!(
                    "DELETE FROM {TOMBSTONE_TABLE} WHERE key = ? AND confirmed = 0 AND owner = ?"
                ))
                .bind::<Text, _>(key)
                .bind::<Text, _>(owner)
                .execute(&mut *conn)
                .await,
                None => {
                    diesel::sql_query(format!(
                        "DELETE FROM {TOMBSTONE_TABLE} WHERE key = ? AND confirmed = 0"
                    ))
                    .bind::<Text, _>(key)
                    .execute(&mut *conn)
                    .await
                }
            };
        query.map(drop).map_err(store_error("remove"))
    }

    /// Record a failed purge of `tombstone` (`PurgeFailed` in the model).
    /// Returns the new row.
    ///
    /// # Errors
    ///
    /// The control database cannot be written.
    pub async fn record_failure(
        &self,
        tombstone: Tombstone,
        error: &str,
    ) -> Result<Tombstone, TombstoneError> {
        let row = tombstone.after_failure(self.now(), error);
        let mut conn = self.conn().await?;
        diesel::sql_query(format!(
            "UPDATE {TOMBSTONE_TABLE} SET attempts = ?, next_attempt = ?, last_error = ? \
             WHERE key = ?"
        ))
        .bind::<Integer, _>(i32::try_from(row.attempts).unwrap_or(i32::MAX))
        .bind::<BigInt, _>(row.next_attempt_at.timestamp_millis())
        .bind::<Nullable<Text>, _>(row.last_error.as_deref())
        .bind::<Text, _>(&row.key)
        .execute(&mut *conn)
        .await
        .map_err(store_error("record failure"))?;
        Ok(row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn at(ms: i64) -> DateTime<Utc> {
        from_millis(ms)
    }

    #[test]
    fn a_fresh_row_waits_for_the_retention() {
        let row = Tombstone::new("tenant:acme", at(1_000), Duration::from_secs(60), true);
        assert_eq!(row.purge_after, at(61_000));
        assert!(!row.is_due(at(60_999)));
        assert!(row.is_due(at(61_000)));
    }

    #[test]
    fn a_pending_row_is_never_due() {
        let row = Tombstone::new("tenant:acme", at(1_000), Duration::ZERO, false);
        assert!(!row.is_due(at(i64::from(i32::MAX))));
    }

    #[test]
    fn a_failure_backs_off_and_keeps_the_error() {
        let row = Tombstone::new("slot:00001", at(0), Duration::ZERO, true);
        let row = row.after_failure(at(10_000), "down");
        assert_eq!(row.attempts, 1);
        assert_eq!(row.next_attempt_at, at(15_000));
        assert_eq!(row.last_error.as_deref(), Some("down"));
        assert!(!row.is_due(at(14_999)));
        let row = row.after_failure(at(15_000), "down");
        assert_eq!(row.next_attempt_at, at(25_000));
    }

    #[test]
    fn the_retry_delay_is_capped() {
        assert_eq!(retry_delay(0), RETRY_BASE);
        assert_eq!(retry_delay(1), RETRY_BASE * 2);
        assert_eq!(retry_delay(40), RETRY_CAP);
        assert_eq!(retry_delay(u32::MAX), RETRY_CAP);
    }

    #[test]
    fn a_row_or_an_old_marker_blocks_a_restore() {
        assert!(restore_allowed(false, None, at(5)));
        assert!(!restore_allowed(true, None, at(5)));
        assert!(!restore_allowed(false, Some(at(5)), at(5)));
        assert!(!restore_allowed(false, Some(at(6)), at(5)));
        assert!(restore_allowed(false, Some(at(4)), at(5)));
    }

    #[test]
    fn a_purge_keeps_newer_generations_and_their_shared_objects() {
        assert!(purge_deletes(Some(at(5)), at(5), true));
        assert!(!purge_deletes(Some(at(6)), at(5), true));
        assert!(purge_deletes(None, at(5), false));
        assert!(!purge_deletes(None, at(5), true));
    }

    #[test]
    fn the_marker_round_trips() {
        let row = Tombstone::new("tenant:acme", at(1_000), Duration::from_secs(1), true);
        let marker = Marker::of(&row);
        let body = serde_json::to_vec(&marker).unwrap();
        let back: Marker = serde_json::from_slice(&body).unwrap();
        assert_eq!(back, marker);
        assert_eq!(back.deleted_at(), at(1_000));
    }

    proptest! {
        /// The runtime rules keep the model's properties.
        #[test]
        fn the_rules_keep_the_model_properties(
            deleted in 0_i64..1_000_000_000,
            retention_ms in 0_u64..100_000_000,
            attempts in 0_u32..64,
            now in 0_i64..2_000_000_000,
            newest in 0_i64..2_000_000_000,
        ) {
            let retention = Duration::from_millis(retention_ms);
            let row = Tombstone::new("k", at(deleted), retention, true);
            // inv: purge_after = deleted_at + retention.
            prop_assert_eq!(
                row.purge_after.timestamp_millis(),
                deleted + i64::try_from(retention_ms).unwrap()
            );
            // retention_is_kept.
            if row.is_due(at(now)) {
                prop_assert!(at(now) >= row.purge_after);
            }
            // a_failure_keeps_the_row.
            let mut failed = row;
            failed.attempts = attempts;
            let failed = failed.after_failure(at(now), "e");
            prop_assert!(failed.next_attempt_at > at(now));
            prop_assert!(failed.next_attempt_at <= add(at(now), RETRY_CAP));
            prop_assert!(!failed.is_due(at(now)));
            // a_row_blocks_restore and the marker rules.
            prop_assert!(!restore_allowed(true, Some(at(deleted)), at(newest)));
            prop_assert_eq!(
                restore_allowed(false, Some(at(deleted)), at(newest)),
                newest > deleted
            );
            // a_purge_keeps_newer_generations.
            if newest > deleted {
                prop_assert!(!purge_deletes(Some(at(newest)), at(deleted), true));
            }
        }
    }

    fn control(dir: &std::path::Path) -> Pool<RuntimeConnection> {
        crate::db::create_pool(&crate::config::DatabaseConfig {
            url: Some(format!("sqlite://{}", dir.join("control.db").display())),
            ..Default::default()
        })
        .unwrap()
        .unwrap()
    }

    fn store(dir: &std::path::Path, clock: &crate::time::TickingClock) -> FleetTombstones {
        FleetTombstones::new(control(dir), Arc::new(clock.clone()))
    }

    #[tokio::test]
    async fn a_row_goes_from_pending_to_confirmed_to_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let clock = crate::time::TickingClock::starting_at(at(1_000));
        let store = store(tmp.path(), &clock);
        let retention = Duration::from_secs(60);
        store
            .record("tenant:acme", retention, false, "test")
            .await
            .unwrap();
        let pending = store.get("tenant:acme").await.unwrap().unwrap();
        assert!(!pending.confirmed);
        clock.advance(Duration::from_secs(1));
        let confirmed = store
            .confirm("tenant:acme", retention, None)
            .await
            .unwrap()
            .unwrap();
        assert!(confirmed.confirmed);
        assert!(!confirmed.marked);
        assert_eq!(confirmed.deleted_at, at(2_000));
        assert_eq!(confirmed.purge_after, at(62_000));
        // A second confirm keeps the cutoff.
        clock.advance(Duration::from_secs(1));
        assert_eq!(
            store.confirm("tenant:acme", retention, None).await.unwrap(),
            Some(confirmed.clone())
        );
        store.set_marked("tenant:acme").await.unwrap();
        let confirmed = store.get("tenant:acme").await.unwrap().unwrap();
        assert!(confirmed.marked);
        // A second delete that finds no file keeps it too.
        store
            .record("tenant:acme", retention, false, "test")
            .await
            .unwrap();
        store
            .remove_pending("tenant:acme", Some("test"))
            .await
            .unwrap();
        assert_eq!(store.get("tenant:acme").await.unwrap(), Some(confirmed));
        store.remove("tenant:acme").await.unwrap();
        assert_eq!(store.get("tenant:acme").await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_confirm_without_its_pending_row_writes_nothing() {
        // A re-create or a restore in another process took the key between
        // the removal and the confirm: no cutoff may cover the new database.
        let tmp = tempfile::tempdir().unwrap();
        let clock = crate::time::TickingClock::starting_at(at(1_000));
        let store = store(tmp.path(), &clock);
        store
            .record("tenant:acme", Duration::ZERO, false, "test")
            .await
            .unwrap();
        store.remove("tenant:acme").await.unwrap();
        assert_eq!(
            store
                .confirm("tenant:acme", Duration::ZERO, None)
                .await
                .unwrap(),
            None
        );
        assert_eq!(store.get("tenant:acme").await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_refused_delete_keeps_the_pending_row_of_another_delete() {
        // Process A removed the file and has not confirmed yet. Process B
        // finds no file: its refusal must not drop A's row.
        let tmp = tempfile::tempdir().unwrap();
        let clock = crate::time::TickingClock::starting_at(at(1_000));
        let store = store(tmp.path(), &clock);
        store
            .record("tenant:acme", Duration::ZERO, true, "a")
            .await
            .unwrap();
        store
            .record("tenant:acme", Duration::ZERO, false, "b")
            .await
            .unwrap();
        store
            .remove_pending("tenant:acme", Some("b"))
            .await
            .unwrap();
        let row = store.get("tenant:acme").await.unwrap().unwrap();
        assert_eq!(row.owner, "a");
        assert!(
            store
                .confirm("tenant:acme", Duration::ZERO, None)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn a_list_pages_past_the_first_batch() {
        let tmp = tempfile::tempdir().unwrap();
        let clock = crate::time::TickingClock::starting_at(at(1_000));
        let store = store(tmp.path(), &clock);
        for i in 0..5 {
            store
                .record(&format!("tenant:t{i}"), Duration::ZERO, false, "test")
                .await
                .unwrap();
        }
        let first = store.list_after(None, 2).await.unwrap();
        assert_eq!(first.len(), 2);
        let second = store.list_after(Some(&first[1].key), 2).await.unwrap();
        let third = store.list_after(Some(&second[1].key), 2).await.unwrap();
        let keys: Vec<String> = first
            .iter()
            .chain(&second)
            .chain(&third)
            .map(|t| t.key.clone())
            .collect();
        assert_eq!(
            keys,
            (0..5).map(|i| format!("tenant:t{i}")).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn a_failure_is_durable_and_listed() {
        let tmp = tempfile::tempdir().unwrap();
        let clock = crate::time::TickingClock::starting_at(at(1_000));
        let store = store(tmp.path(), &clock);
        store
            .record("slot:00007", Duration::ZERO, false, "test")
            .await
            .unwrap();
        let row = store
            .confirm("slot:00007", Duration::ZERO, None)
            .await
            .unwrap()
            .unwrap();
        let failed = store
            .record_failure(row, "destination is down")
            .await
            .unwrap();
        assert_eq!(failed.attempts, 1);
        // A new store on the same file reads the same row.
        let again = FleetTombstones::new(control(tmp.path()), Arc::new(clock.clone()));
        assert_eq!(again.list_after(None, 10).await.unwrap(), vec![failed]);
        store
            .record("slot:00008", Duration::ZERO, false, "test")
            .await
            .unwrap();
        store
            .remove_pending("slot:00008", Some("test"))
            .await
            .unwrap();
        assert_eq!(again.list_after(None, 10).await.unwrap().len(), 1);
    }

    /// The replica lifecycle against a local destination (issue #3235).
    mod lifecycle {
        use super::*;
        use crate::actuator::HealthStatus;
        use crate::config::DatabaseFleetConfig;
        use crate::db::fleet::{DatabaseFleet, FleetDatabase, FleetError};
        use crate::db::fleet_replication::{FleetReplication, replica_root};
        use crate::fleet_layout::{FleetDbKey, FleetMode};
        use crate::replication::destination::ReplicaDestination;
        use crate::replication::{DestinationError, FileDestination, ReplicationSettings};
        use std::path::Path;
        use std::sync::atomic::{AtomicBool, Ordering};

        const ROOT: &str = "app/test";

        /// A local destination whose deletes, or every call, can fail.
        struct Switchable {
            inner: FileDestination,
            deletes_fail: AtomicBool,
            down: AtomicBool,
            /// Deletes that still succeed; then each delete fails.
            deletes_left: std::sync::atomic::AtomicUsize,
            /// Lists of generations fail (the cutoff lookup).
            lists_fail: AtomicBool,
        }

        impl Switchable {
            fn new(dir: &Path) -> Arc<Self> {
                Arc::new(Self {
                    inner: FileDestination::new(dir).unwrap(),
                    deletes_fail: AtomicBool::new(false),
                    down: AtomicBool::new(false),
                    deletes_left: std::sync::atomic::AtomicUsize::new(usize::MAX),
                    lists_fail: AtomicBool::new(false),
                })
            }

            fn check(&self, delete: bool) -> Result<(), DestinationError> {
                let budget_spent = delete
                    && self
                        .deletes_left
                        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                        .is_err();
                if AtomicBool::load(&self.down, Ordering::SeqCst)
                    || budget_spent
                    || (delete && AtomicBool::load(&self.deletes_fail, Ordering::SeqCst))
                {
                    return Err(DestinationError::Io {
                        op: "delete object",
                        detail: "destination is down".to_owned(),
                    });
                }
                Ok(())
            }

            fn keys(&self, key: &FleetDbKey) -> Vec<String> {
                self.inner
                    .list(&format!("{}/", replica_root(ROOT, key)))
                    .unwrap()
            }
        }

        impl ReplicaDestination for Switchable {
            fn describe(&self) -> String {
                "switchable".to_owned()
            }
            fn put(&self, key: &str, body: &[u8]) -> Result<(), DestinationError> {
                self.check(false)?;
                self.inner.put(key, body)
            }
            fn put_file(&self, key: &str, path: &Path) -> Result<(), DestinationError> {
                self.check(false)?;
                self.inner.put_file(key, path)
            }
            fn get(&self, key: &str) -> Result<Vec<u8>, DestinationError> {
                self.check(false)?;
                self.inner.get(key)
            }
            fn get_to_file(&self, key: &str, path: &Path) -> Result<(), DestinationError> {
                self.check(false)?;
                self.inner.get_to_file(key, path)
            }
            fn list(&self, prefix: &str) -> Result<Vec<String>, DestinationError> {
                self.check(false)?;
                if prefix.ends_with("/generations/")
                    && AtomicBool::load(&self.lists_fail, Ordering::SeqCst)
                {
                    return Err(DestinationError::Io {
                        op: "list",
                        detail: "list failed".to_owned(),
                    });
                }
                self.inner.list(prefix)
            }
            fn delete(&self, key: &str) -> Result<(), DestinationError> {
                self.check(true)?;
                self.inner.delete(key)
            }
        }

        struct Host {
            fleet: DatabaseFleet,
            replication: Arc<FleetReplication>,
            tombstones: Arc<FleetTombstones>,
        }

        struct Setup<'a> {
            volume: &'a Path,
            destination: &'a Arc<Switchable>,
            clock: &'a crate::time::TickingClock,
            restore_missing: bool,
            retention: Duration,
        }

        fn host(setup: &Setup<'_>) -> Host {
            host_with(setup, false, setup.clock)
        }

        /// A host that creates on demand, and whose replicators read
        /// `replica_clock` (a host clock that is ahead, say).
        fn host_with(
            setup: &Setup<'_>,
            create_on_demand: bool,
            replica_clock: &crate::time::TickingClock,
        ) -> Host {
            let fleet = DatabaseFleet::builder(DatabaseFleetConfig {
                mode: FleetMode::Tenant,
                root: setup.volume.join("fleet").display().to_string(),
                path: None,
                max_open: 8,
                pool_size: 2,
                create_on_demand: Some(create_on_demand),
                idle_close_secs: 0,
                restore_missing: setup.restore_missing,
                ownership: None,
                replica_retention_after_delete: setup.retention,
                restore_check_sample: 0,
                replication_workers: 4,
                min_residency: std::time::Duration::from_secs(2),
            })
            .build()
            .unwrap();
            let clock: Arc<dyn ClockSource> = Arc::new(setup.clock.clone());
            let tombstones = Arc::new(FleetTombstones::new(
                control(setup.volume),
                Arc::clone(&clock),
            ));
            let replication = Arc::new(
                FleetReplication::new(
                    Arc::clone(setup.destination) as Arc<dyn ReplicaDestination>,
                    ReplicationSettings {
                        database_path: std::path::PathBuf::new(),
                        root: ROOT.to_owned(),
                        sync_interval: Duration::from_secs(1),
                        snapshot_interval: Duration::from_secs(3600),
                        max_wal_bytes: 4 * 1024 * 1024,
                        retention: Duration::from_secs(24 * 3600),
                        verify_interval: None,
                    },
                    Duration::from_secs(30),
                    setup.restore_missing,
                    Arc::new(replica_clock.clone()),
                )
                .with_tombstones(Arc::clone(&tombstones))
                .with_retention_after_delete(setup.retention),
            );
            assert!(fleet.install_replication(Arc::clone(&replication)).is_ok());
            Host {
                fleet,
                replication,
                tombstones,
            }
        }

        async fn exec(db: &FleetDatabase, sql: &str) {
            let mut conn = db.pool().get().await.unwrap();
            diesel::sql_query(sql).execute(&mut *conn).await.unwrap();
        }

        /// Provision `id`, write a row, and close it, so its replica holds
        /// every commit.
        async fn shipped(host: &Host, id: &str) -> FleetDbKey {
            let key = host.fleet.key_for(id).unwrap();
            let db = host.fleet.provision(&key).await.unwrap();
            exec(&db, "CREATE TABLE notes (body TEXT NOT NULL)").await;
            exec(&db, "INSERT INTO notes VALUES ('kept offsite')").await;
            drop(db);
            host.fleet.close(&key).await;
            key
        }

        fn clock() -> crate::time::TickingClock {
            crate::time::TickingClock::starting_at(at(1_800_000_000_000))
        }

        #[tokio::test]
        async fn delete_removes_the_replica_and_keeps_its_neighbour() {
            let tmp = tempfile::tempdir().unwrap();
            let destination = Switchable::new(&tmp.path().join("replicas"));
            let clock = clock();
            let host = host(&Setup {
                volume: tmp.path(),
                destination: &destination,
                clock: &clock,
                restore_missing: false,
                retention: Duration::ZERO,
            });
            let acme = shipped(&host, "acme").await;
            let neighbour = shipped(&host, "acme2").await;
            assert!(!destination.keys(&acme).is_empty());
            clock.advance(Duration::from_secs(1));

            assert!(host.fleet.delete(&acme).await.unwrap());

            assert_eq!(destination.keys(&acme), Vec::<String>::new());
            assert!(
                !destination.keys(&neighbour).is_empty(),
                "a prefix is not a name"
            );
            assert_eq!(host.tombstones.get(&acme.name()).await.unwrap(), None);
            // A late tick ships nothing for the deleted database.
            let replication = Arc::clone(&host.replication);
            crate::time::spawn_blocking(move || replication.tick_all())
                .await
                .unwrap();
            assert_eq!(destination.keys(&acme), Vec::<String>::new());
        }

        #[tokio::test]
        async fn a_failed_purge_writes_a_tombstone_and_the_retry_purges() {
            let tmp = tempfile::tempdir().unwrap();
            let destination = Switchable::new(&tmp.path().join("replicas"));
            let clock = clock();
            let host = host(&Setup {
                volume: tmp.path(),
                destination: &destination,
                clock: &clock,
                restore_missing: false,
                retention: Duration::ZERO,
            });
            let key = shipped(&host, "acme").await;
            clock.advance(Duration::from_secs(1));
            destination.deletes_fail.store(true, Ordering::SeqCst);

            // The local delete is not rolled back.
            assert!(host.fleet.delete(&key).await.unwrap());
            assert!(!host.fleet.exists(&key));
            let row = host.tombstones.get(&key.name()).await.unwrap().unwrap();
            assert!(row.confirmed);
            assert_eq!(row.attempts, 1);
            assert!(row.last_error.is_some());
            assert!(!destination.keys(&key).is_empty());

            // Not due yet: the pass waits.
            destination.deletes_fail.store(false, Ordering::SeqCst);
            let report = host.fleet.purge_deleted_replicas().await;
            assert_eq!((report.purged, report.waiting), (0, 1));
            assert!(!destination.keys(&key).is_empty());

            clock.advance(retry_delay(0));
            let report = host.fleet.purge_deleted_replicas().await;
            assert_eq!((report.purged, report.failed), (1, 0));
            assert_eq!(destination.keys(&key), Vec::<String>::new());
            assert_eq!(host.tombstones.get(&key.name()).await.unwrap(), None);
        }

        #[tokio::test]
        async fn a_failed_retry_backs_off_and_health_reports_it() {
            let tmp = tempfile::tempdir().unwrap();
            let destination = Switchable::new(&tmp.path().join("replicas"));
            let clock = clock();
            let host = host(&Setup {
                volume: tmp.path(),
                destination: &destination,
                clock: &clock,
                restore_missing: false,
                retention: Duration::ZERO,
            });
            let key = shipped(&host, "acme").await;
            clock.advance(Duration::from_secs(1));
            destination.deletes_fail.store(true, Ordering::SeqCst);
            assert!(host.fleet.delete(&key).await.unwrap());
            clock.advance(retry_delay(0));
            let report = host.fleet.purge_deleted_replicas().await;
            assert_eq!(report.failed, 1);
            let row = host.tombstones.get(&key.name()).await.unwrap().unwrap();
            assert_eq!(row.attempts, 2);
            let health = host.replication.indicator().check().await;
            let tombstones = &health.details["tombstones"];
            assert_eq!(tombstones["failed"], 1, "{tombstones}");
            assert_eq!(tombstones["failing"], 1, "{tombstones}");
            // A failed purge does not turn the indicator Down.
            assert_eq!(health.status, HealthStatus::Up);
            // In its backoff the row waits, and health keeps its error.
            let report = host.fleet.purge_deleted_replicas().await;
            assert_eq!((report.failed, report.failing), (0, 1));
            assert!(report.last_error.is_some());
        }

        #[tokio::test]
        async fn restore_missing_does_not_resurrect_a_tombstoned_key() {
            let tmp = tempfile::tempdir().unwrap();
            let destination = Switchable::new(&tmp.path().join("replicas"));
            let clock = clock();
            let a = host(&Setup {
                volume: &tmp.path().join("a"),
                destination: &destination,
                clock: &clock,
                restore_missing: true,
                retention: Duration::ZERO,
            });
            let key = shipped(&a, "acme").await;
            clock.advance(Duration::from_secs(1));
            destination.deletes_fail.store(true, Ordering::SeqCst);
            assert!(a.fleet.delete(&key).await.unwrap());
            assert!(
                !destination.keys(&key).is_empty(),
                "the replica is still there"
            );

            // Another host with its own control database: the marker blocks
            // the restore.
            let b = host(&Setup {
                volume: &tmp.path().join("b"),
                destination: &destination,
                clock: &clock,
                restore_missing: true,
                retention: Duration::ZERO,
            });
            assert!(a.replication.marker(&key).unwrap().is_some());
            assert!(matches!(
                b.fleet.open(&key).await,
                Err(FleetError::NotFound { .. })
            ));
            assert!(!b.fleet.exists(&key));

            // This host: the row alone blocks the restore.
            destination.deletes_fail.store(false, Ordering::SeqCst);
            destination
                .inner
                .delete(&format!("{}/{MARKER_OBJECT}", replica_root(ROOT, &key)))
                .unwrap();
            assert!(matches!(
                a.fleet.open(&key).await,
                Err(FleetError::NotFound { .. })
            ));
            assert!(!a.fleet.exists(&key));

            // An explicit restore overrides the tombstone and drops it.
            let outcome = a.fleet.restore(&key, None).await.unwrap();
            assert!(outcome.bytes > 0);
            assert_eq!(a.tombstones.get(&key.name()).await.unwrap(), None);
            assert_eq!(a.replication.marker(&key).unwrap(), None);
            let db = a.fleet.open(&key).await.unwrap();
            let mut conn = db.pool().get().await.unwrap();
            diesel::sql_query("SELECT body FROM notes")
                .execute(&mut *conn)
                .await
                .unwrap();
            drop(conn);
            drop(db);
            a.fleet.close(&key).await;
            // Nothing purges the restored replica, and another host restores it.
            let report = a.fleet.purge_deleted_replicas().await;
            assert_eq!(report.purged, 0);
            std::fs::remove_dir_all(b.fleet.root()).unwrap();
            let b = host(&Setup {
                volume: &tmp.path().join("b"),
                destination: &destination,
                clock: &clock,
                restore_missing: true,
                retention: Duration::ZERO,
            });
            b.fleet.open(&key).await.unwrap();
        }

        #[tokio::test]
        async fn a_failed_restore_puts_the_tombstone_back() {
            let tmp = tempfile::tempdir().unwrap();
            let replicas = tmp.path().join("replicas");
            let destination = Switchable::new(&replicas);
            let clock = clock();
            let host = host(&Setup {
                volume: tmp.path(),
                destination: &destination,
                clock: &clock,
                restore_missing: false,
                retention: Duration::from_secs(3600),
            });
            let key = shipped(&host, "acme").await;
            clock.advance(Duration::from_secs(1));
            assert!(host.fleet.delete(&key).await.unwrap());
            let row = host.tombstones.get(&key.name()).await.unwrap().unwrap();
            let snapshot = destination
                .keys(&key)
                .into_iter()
                .find(|k| k.ends_with(crate::replication::segment::SNAPSHOT_OBJECT))
                .unwrap();
            std::fs::write(replicas.join(snapshot), b"not a database").unwrap();
            assert!(host.fleet.restore(&key, None).await.is_err());
            assert_eq!(host.tombstones.get(&key.name()).await.unwrap(), Some(row));
        }

        #[tokio::test]
        async fn a_partial_purge_keeps_the_marker_and_the_next_pass_ends_it() {
            let tmp = tempfile::tempdir().unwrap();
            let destination = Switchable::new(&tmp.path().join("replicas"));
            let clock = clock();
            let a = host(&Setup {
                volume: &tmp.path().join("a"),
                destination: &destination,
                clock: &clock,
                restore_missing: true,
                retention: Duration::ZERO,
            });
            let key = shipped(&a, "acme").await;
            assert!(destination.keys(&key).len() >= 2);
            clock.advance(Duration::from_secs(1));
            // The first delete goes, then the destination fails.
            destination.deletes_left.store(1, Ordering::SeqCst);
            assert!(a.fleet.delete(&key).await.unwrap());
            assert!(
                a.replication.marker(&key).unwrap().is_some(),
                "the marker goes last"
            );
            assert_eq!(
                a.tombstones
                    .get(&key.name())
                    .await
                    .unwrap()
                    .unwrap()
                    .attempts,
                1
            );
            let b = host(&Setup {
                volume: &tmp.path().join("b"),
                destination: &destination,
                clock: &clock,
                restore_missing: true,
                retention: Duration::ZERO,
            });
            assert!(matches!(
                b.fleet.open(&key).await,
                Err(FleetError::NotFound { .. })
            ));

            destination.deletes_left.store(usize::MAX, Ordering::SeqCst);
            clock.advance(retry_delay(0));
            assert_eq!(a.fleet.purge_deleted_replicas().await.purged, 1);
            assert_eq!(destination.keys(&key), Vec::<String>::new());
        }

        #[tokio::test]
        async fn a_create_on_demand_does_not_end_the_retention() {
            let tmp = tempfile::tempdir().unwrap();
            let destination = Switchable::new(&tmp.path().join("replicas"));
            let clock = clock();
            let setup = Setup {
                volume: tmp.path(),
                destination: &destination,
                clock: &clock,
                restore_missing: false,
                retention: Duration::from_secs(3600),
            };
            let host = host_with(&setup, true, &clock);
            let key = shipped(&host, "acme").await;
            clock.advance(Duration::from_secs(1));
            assert!(host.fleet.delete(&key).await.unwrap());
            let kept = destination.keys(&key);
            assert!(matches!(
                host.fleet.open(&key).await,
                Err(FleetError::Deleting { .. })
            ));
            assert_eq!(destination.keys(&key), kept, "the undo window stays");
            // An explicit provision ends it.
            host.fleet.provision(&key).await.unwrap();
            assert_eq!(host.tombstones.get(&key.name()).await.unwrap(), None);
        }

        #[tokio::test]
        async fn a_failed_cutoff_lookup_leaves_the_row_pending() {
            // The replica clock is ahead, so the deleted generation is newer
            // than the app clock. Without the lookup the cutoff would keep it.
            let tmp = tempfile::tempdir().unwrap();
            let destination = Switchable::new(&tmp.path().join("replicas"));
            let clock = clock();
            let ahead =
                crate::time::TickingClock::starting_at(clock.now() + chrono::Duration::hours(1));
            let setup = Setup {
                volume: tmp.path(),
                destination: &destination,
                clock: &clock,
                restore_missing: false,
                retention: Duration::ZERO,
            };
            let host = host_with(&setup, false, &ahead);
            let key = shipped(&host, "acme").await;
            destination.lists_fail.store(true, Ordering::SeqCst);
            assert!(host.fleet.delete(&key).await.unwrap());
            let row = host.tombstones.get(&key.name()).await.unwrap().unwrap();
            assert!(!row.confirmed, "no cutoff without the lookup");
            assert!(!destination.keys(&key).is_empty());

            destination.lists_fail.store(false, Ordering::SeqCst);
            clock.advance(STALE_PENDING + Duration::from_secs(1));
            let report = host.fleet.purge_deleted_replicas().await;
            assert_eq!(report.purged, 1, "{report:?}");
            assert_eq!(destination.keys(&key), Vec::<String>::new());
        }

        #[tokio::test]
        async fn a_create_on_demand_respects_another_hosts_retention() {
            let tmp = tempfile::tempdir().unwrap();
            let destination = Switchable::new(&tmp.path().join("replicas"));
            let clock = clock();
            let a = host(&Setup {
                volume: &tmp.path().join("a"),
                destination: &destination,
                clock: &clock,
                restore_missing: true,
                retention: Duration::from_secs(3600),
            });
            let key = shipped(&a, "acme").await;
            clock.advance(Duration::from_secs(1));
            assert!(a.fleet.delete(&key).await.unwrap());
            // Another host, its own control database, creates on demand.
            let b_setup = Setup {
                volume: &tmp.path().join("b"),
                destination: &destination,
                clock: &clock,
                restore_missing: true,
                retention: Duration::ZERO,
            };
            let b = host_with(&b_setup, true, &clock);
            assert!(matches!(
                b.fleet.open(&key).await,
                Err(FleetError::Deleting { .. })
            ));
            assert!(!b.fleet.exists(&key));
            // After the retention, the create goes through.
            clock.advance(Duration::from_secs(3600));
            b.fleet.open(&key).await.unwrap();
        }

        #[tokio::test]
        async fn a_young_pending_row_waits_and_a_stale_confirmed_row_is_dropped() {
            let tmp = tempfile::tempdir().unwrap();
            let destination = Switchable::new(&tmp.path().join("replicas"));
            let clock = clock();
            let host = host(&Setup {
                volume: tmp.path(),
                destination: &destination,
                clock: &clock,
                restore_missing: false,
                retention: Duration::ZERO,
            });
            let live = shipped(&host, "acme").await;
            // A delete of a missing file runs now: its row is pending.
            host.tombstones
                .record("tenant:ghost", Duration::ZERO, false, "test")
                .await
                .unwrap();
            // A restore left a confirmed row next to a live file.
            host.tombstones
                .record(&live.name(), Duration::ZERO, true, "test")
                .await
                .unwrap();
            host.tombstones
                .confirm(&live.name(), Duration::ZERO, None)
                .await
                .unwrap();

            let report = host.fleet.purge_deleted_replicas().await;
            assert_eq!((report.purged, report.waiting), (0, 1), "{report:?}");
            assert!(
                !host
                    .tombstones
                    .get("tenant:ghost")
                    .await
                    .unwrap()
                    .unwrap()
                    .confirmed
            );
            assert_eq!(host.tombstones.get(&live.name()).await.unwrap(), None);
            assert!(
                !destination.keys(&live).is_empty(),
                "the live replica stays"
            );
        }

        #[tokio::test]
        async fn a_second_delete_resets_a_stale_row() {
            let tmp = tempfile::tempdir().unwrap();
            let destination = Switchable::new(&tmp.path().join("replicas"));
            let clock = clock();
            let host = host(&Setup {
                volume: tmp.path(),
                destination: &destination,
                clock: &clock,
                restore_missing: false,
                retention: Duration::ZERO,
            });
            let key = shipped(&host, "acme").await;
            // A stale confirmed row with a cutoff older than the live
            // database's generation.
            let old = Tombstone::new(
                &key.name(),
                clock.now() - chrono::Duration::hours(1),
                Duration::ZERO,
                true,
            );
            host.tombstones.put(&old).await.unwrap();
            clock.advance(Duration::from_secs(5));
            assert!(host.fleet.delete(&key).await.unwrap());
            assert_eq!(
                destination.keys(&key),
                Vec::<String>::new(),
                "the new cutoff covers it"
            );
            assert_eq!(host.tombstones.get(&key.name()).await.unwrap(), None);
        }

        #[tokio::test]
        async fn a_replica_clock_ahead_of_the_app_clock_is_still_purged() {
            let tmp = tempfile::tempdir().unwrap();
            let destination = Switchable::new(&tmp.path().join("replicas"));
            let clock = clock();
            let ahead =
                crate::time::TickingClock::starting_at(clock.now() + chrono::Duration::hours(1));
            let setup = Setup {
                volume: tmp.path(),
                destination: &destination,
                clock: &clock,
                restore_missing: false,
                retention: Duration::ZERO,
            };
            let host = host_with(&setup, false, &ahead);
            let key = shipped(&host, "acme").await;
            assert!(host.fleet.delete(&key).await.unwrap());
            assert_eq!(destination.keys(&key), Vec::<String>::new());
        }

        #[tokio::test]
        async fn retention_keeps_the_replica_until_it_is_due() {
            let tmp = tempfile::tempdir().unwrap();
            let destination = Switchable::new(&tmp.path().join("replicas"));
            let clock = clock();
            let retention = Duration::from_secs(3600);
            let host = host(&Setup {
                volume: tmp.path(),
                destination: &destination,
                clock: &clock,
                restore_missing: true,
                retention,
            });
            let key = shipped(&host, "acme").await;
            clock.advance(Duration::from_secs(1));
            assert!(host.fleet.delete(&key).await.unwrap());
            let row = host.tombstones.get(&key.name()).await.unwrap().unwrap();
            assert_eq!(row.attempts, 0);
            assert_eq!(row.purge_after, add(row.deleted_at, retention));
            assert!(!destination.keys(&key).is_empty(), "kept for undo");
            assert!(matches!(
                host.fleet.open(&key).await,
                Err(FleetError::NotFound { .. })
            ));

            let report = host.fleet.purge_deleted_replicas().await;
            assert_eq!((report.purged, report.waiting), (0, 1));
            clock.advance(retention);
            let report = host.fleet.purge_deleted_replicas().await;
            assert_eq!(report.purged, 1);
            assert_eq!(destination.keys(&key), Vec::<String>::new());
        }

        #[tokio::test]
        async fn a_marker_that_failed_during_the_retention_is_written_again() {
            let tmp = tempfile::tempdir().unwrap();
            let destination = Switchable::new(&tmp.path().join("replicas"));
            let clock = clock();
            let a = host(&Setup {
                volume: &tmp.path().join("a"),
                destination: &destination,
                clock: &clock,
                restore_missing: true,
                retention: Duration::from_secs(3600),
            });
            let key = shipped(&a, "acme").await;
            clock.advance(Duration::from_secs(1));
            destination.down.store(true, Ordering::SeqCst);
            assert!(a.fleet.delete(&key).await.unwrap());
            assert!(!a.tombstones.get(&key.name()).await.unwrap().unwrap().marked);
            destination.down.store(false, Ordering::SeqCst);
            assert_eq!(a.replication.marker(&key).unwrap(), None);
            // The delete could not read the cutoff: the row is pending. The
            // pass confirms it once it is old.
            clock.advance(STALE_PENDING + Duration::from_secs(1));

            let report = a.fleet.purge_deleted_replicas().await;
            assert_eq!(report.waiting, 1, "the retention still keeps it");
            assert!(a.tombstones.get(&key.name()).await.unwrap().unwrap().marked);
            assert!(a.replication.marker(&key).unwrap().is_some());

            // A host with its own control database does not restore it.
            let b = host(&Setup {
                volume: &tmp.path().join("b"),
                destination: &destination,
                clock: &clock,
                restore_missing: true,
                retention: Duration::ZERO,
            });
            assert!(matches!(
                b.fleet.open(&key).await,
                Err(FleetError::NotFound { .. })
            ));
        }

        #[tokio::test]
        async fn a_pass_reaches_rows_past_a_full_batch_of_waiting_ones() {
            let tmp = tempfile::tempdir().unwrap();
            let destination = Switchable::new(&tmp.path().join("replicas"));
            let clock = clock();
            let host = host(&Setup {
                volume: tmp.path(),
                destination: &destination,
                clock: &clock,
                restore_missing: false,
                retention: Duration::ZERO,
            });
            // Rows that sort first and wait: their retention has not ended.
            for i in 0..300 {
                let name = format!("tenant:a{i:03}");
                host.tombstones
                    .record(&name, Duration::from_secs(3600), false, "test")
                    .await
                    .unwrap();
                host.tombstones
                    .confirm(&name, Duration::from_secs(3600), None)
                    .await
                    .unwrap();
            }
            let key = shipped(&host, "zeta").await;
            clock.advance(Duration::from_secs(1));
            destination.deletes_fail.store(true, Ordering::SeqCst);
            assert!(host.fleet.delete(&key).await.unwrap());
            destination.deletes_fail.store(false, Ordering::SeqCst);
            clock.advance(retry_delay(0));

            let report = host.fleet.purge_deleted_replicas().await;
            assert_eq!((report.purged, report.waiting), (1, 300), "{report:?}");
            assert_eq!(destination.keys(&key), Vec::<String>::new());
        }

        #[tokio::test]
        async fn a_recreated_key_purges_the_old_replica_first() {
            let tmp = tempfile::tempdir().unwrap();
            let destination = Switchable::new(&tmp.path().join("replicas"));
            let clock = clock();
            let host = host(&Setup {
                volume: tmp.path(),
                destination: &destination,
                clock: &clock,
                restore_missing: false,
                retention: Duration::from_secs(3600),
            });
            let key = shipped(&host, "acme").await;
            let old: Vec<String> = destination.keys(&key);
            clock.advance(Duration::from_secs(1));
            assert!(host.fleet.delete(&key).await.unwrap());
            clock.advance(Duration::from_secs(1));

            let db = host.fleet.provision(&key).await.unwrap();
            drop(db);
            host.fleet.close(&key).await;
            assert_eq!(host.tombstones.get(&key.name()).await.unwrap(), None);
            let now: Vec<String> = destination.keys(&key);
            assert!(!now.is_empty(), "the new database replicates");
            assert!(
                old.iter().all(|k| !now.contains(k)),
                "the old replica is gone"
            );
        }

        #[tokio::test]
        async fn a_recreate_is_refused_while_the_old_replica_cannot_be_purged() {
            let tmp = tempfile::tempdir().unwrap();
            let destination = Switchable::new(&tmp.path().join("replicas"));
            let clock = clock();
            let host = host(&Setup {
                volume: tmp.path(),
                destination: &destination,
                clock: &clock,
                restore_missing: false,
                retention: Duration::ZERO,
            });
            let key = shipped(&host, "acme").await;
            clock.advance(Duration::from_secs(1));
            destination.deletes_fail.store(true, Ordering::SeqCst);
            assert!(host.fleet.delete(&key).await.unwrap());
            assert!(matches!(
                host.fleet.provision(&key).await,
                Err(FleetError::Unavailable { .. })
            ));
            assert!(!host.fleet.exists(&key));
            destination.deletes_fail.store(false, Ordering::SeqCst);
            host.fleet.provision(&key).await.unwrap();
        }

        #[tokio::test]
        async fn a_purge_keeps_a_generation_newer_than_the_cutoff() {
            let tmp = tempfile::tempdir().unwrap();
            let destination = Switchable::new(&tmp.path().join("replicas"));
            let clock = clock();
            let host = host(&Setup {
                volume: tmp.path(),
                destination: &destination,
                clock: &clock,
                restore_missing: false,
                retention: Duration::ZERO,
            });
            let cutoff = clock.now();
            clock.advance(Duration::from_secs(1));
            let key = shipped(&host, "acme").await;
            let before = destination.keys(&key);
            let replication = Arc::clone(&host.replication);
            let purge_key = key.clone();
            let deleted =
                crate::time::spawn_blocking(move || replication.purge_replica(&purge_key, cutoff))
                    .await
                    .unwrap()
                    .unwrap();
            assert_eq!(deleted, 0);
            assert_eq!(destination.keys(&key), before);
        }

        #[tokio::test]
        async fn a_delete_is_refused_when_the_tombstone_cannot_be_written() {
            let tmp = tempfile::tempdir().unwrap();
            let destination = Switchable::new(&tmp.path().join("replicas"));
            let clock = clock();
            let host = host(&Setup {
                volume: tmp.path(),
                destination: &destination,
                clock: &clock,
                restore_missing: false,
                retention: Duration::ZERO,
            });
            let key = shipped(&host, "acme").await;
            // Replace the table with a view, so the insert fails.
            let mut conn = control(tmp.path()).get().await.unwrap();
            conn.batch_execute(&format!("DROP TABLE IF EXISTS {TOMBSTONE_TABLE}; CREATE VIEW {TOMBSTONE_TABLE} AS SELECT 1 AS key"))
                .await
                .unwrap();
            drop(conn);
            let error = host.fleet.delete(&key).await.unwrap_err();
            assert!(matches!(error, FleetError::Unavailable { .. }), "{error}");
            assert!(host.fleet.exists(&key), "nothing was removed");
            assert!(!destination.keys(&key).is_empty());
        }

        #[tokio::test]
        async fn the_restore_check_finds_a_corrupt_replica() {
            let tmp = tempfile::tempdir().unwrap();
            let replicas = tmp.path().join("replicas");
            let destination = Switchable::new(&replicas);
            let clock = clock();
            let host = host(&Setup {
                volume: tmp.path(),
                destination: &destination,
                clock: &clock,
                restore_missing: false,
                retention: Duration::ZERO,
            });
            shipped(&host, "acme").await;
            let bad = shipped(&host, "globex").await;

            let report = host.fleet.check_replica_sample(2).await.unwrap();
            assert_eq!(report.passed.len(), 2, "{report:?}");
            assert!(report.failed.is_empty());
            let health = host.replication.indicator().check().await;
            assert_eq!(health.status, HealthStatus::Up);
            assert_eq!(health.details["restore_check"]["passed"], 2);

            // Corrupt the snapshot of one replica.
            let snapshot = destination
                .keys(&bad)
                .into_iter()
                .find(|k| k.ends_with(crate::replication::segment::SNAPSHOT_OBJECT))
                .unwrap();
            let file = replicas.join(&snapshot);
            std::fs::write(&file, b"not a database").unwrap();

            let report = host.fleet.check_replica_sample(2).await.unwrap();
            assert_eq!(report.passed.len(), 1, "{report:?}");
            assert_eq!(report.failed.len(), 1);
            assert_eq!(report.failed[0].0, bad.name());
            let health = host.replication.indicator().check().await;
            assert_eq!(health.status, HealthStatus::Down);
            // A clean check turns it Up again.
            assert!(host.fleet.delete(&bad).await.unwrap());
            let report = host.fleet.check_replica_sample(2).await.unwrap();
            assert_eq!(
                (report.passed.len(), report.failed.len()),
                (1, 0),
                "{report:?}"
            );
            let health = host.replication.indicator().check().await;
            assert_eq!(health.status, HealthStatus::Up);
            // The scratch files are gone.
            let scratch = host.fleet.root().join(crate::fleet_layout::RESERVED_DIR);
            let left: Vec<_> = walk(&scratch)
                .into_iter()
                .filter(|p| !p.to_string_lossy().ends_with(".lock"))
                .collect();
            assert!(left.is_empty(), "{left:?}");
        }

        #[tokio::test]
        async fn the_maintenance_task_retries_the_purge_and_runs_the_restore_check() {
            let tmp = tempfile::tempdir().unwrap();
            let destination = Switchable::new(&tmp.path().join("replicas"));
            let clock = clock();
            let base = host(&Setup {
                volume: tmp.path(),
                destination: &destination,
                clock: &clock,
                restore_missing: false,
                retention: Duration::ZERO,
            });
            let kept = shipped(&base, "globex").await;
            let key = shipped(&base, "acme").await;
            clock.advance(Duration::from_secs(1));
            destination.deletes_fail.store(true, Ordering::SeqCst);
            assert!(base.fleet.delete(&key).await.unwrap());
            destination.deletes_fail.store(false, Ordering::SeqCst);
            clock.advance(retry_delay(0));
            // The same fleet, now with a fast restore check.
            let replication = Arc::new(
                FleetReplication::new(
                    Arc::clone(&destination) as Arc<dyn ReplicaDestination>,
                    ReplicationSettings {
                        database_path: std::path::PathBuf::new(),
                        root: ROOT.to_owned(),
                        sync_interval: Duration::from_secs(1),
                        snapshot_interval: Duration::from_secs(3600),
                        max_wal_bytes: 4 * 1024 * 1024,
                        retention: Duration::from_secs(24 * 3600),
                        verify_interval: None,
                    },
                    Duration::from_secs(30),
                    false,
                    Arc::new(clock.clone()),
                )
                .with_tombstones(Arc::clone(&base.tombstones))
                .with_restore_check(Some(Duration::from_millis(50)), 1),
            );
            let fleet = DatabaseFleet::builder(DatabaseFleetConfig {
                mode: FleetMode::Tenant,
                root: tmp.path().join("fleet").display().to_string(),
                path: None,
                max_open: 8,
                pool_size: 2,
                create_on_demand: Some(false),
                idle_close_secs: 0,
                restore_missing: false,
                ownership: None,
                replica_retention_after_delete: Duration::ZERO,
                restore_check_sample: 1,
                replication_workers: 4,
                min_residency: std::time::Duration::from_secs(2),
            })
            .build()
            .unwrap();
            assert!(fleet.install_replication(Arc::clone(&replication)).is_ok());
            let stop = tokio_util::sync::CancellationToken::new();
            // The first purge tick of the task is at once, on purpose: a
            // later tick would turn this test into a 30-second timeout.
            let maintenance = fleet.spawn_maintenance(stop.clone());
            let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
            loop {
                let purged = base.tombstones.get(&key.name()).await.unwrap().is_none();
                let health = replication.indicator().check().await;
                let checked = health.details.contains_key("restore_check");
                if purged && checked {
                    assert_eq!(health.details["restore_check"]["passed"], 1, "{health:?}");
                    break;
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the task did not run"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            assert_eq!(destination.keys(&key), Vec::<String>::new());
            assert!(!destination.keys(&kept).is_empty());
            stop.cancel();
            maintenance.await.unwrap();
        }

        #[tokio::test]
        async fn the_restore_check_names_a_wrong_identity_and_an_unreplicated_database() {
            let tmp = tempfile::tempdir().unwrap();
            let replicas = tmp.path().join("replicas");
            let destination = Switchable::new(&replicas);
            let clock = clock();
            let host = host(&Setup {
                volume: tmp.path(),
                destination: &destination,
                clock: &clock,
                restore_missing: false,
                retention: Duration::ZERO,
            });
            let acme = shipped(&host, "acme").await;
            // `globex` gets a copy of acme's replica: its identity is wrong.
            let globex = host.fleet.key_for("globex").unwrap();
            for object in destination.keys(&acme) {
                let copy =
                    object.replacen(&replica_root(ROOT, &acme), &replica_root(ROOT, &globex), 1);
                let body = destination.inner.get(&object).unwrap();
                destination.inner.put(&copy, &body).unwrap();
            }
            // Its file is on disk, made by a fleet with no replication.
            let plain = DatabaseFleet::builder(DatabaseFleetConfig {
                mode: FleetMode::Tenant,
                root: tmp.path().join("fleet").display().to_string(),
                path: None,
                max_open: 8,
                pool_size: 2,
                create_on_demand: Some(false),
                idle_close_secs: 0,
                restore_missing: false,
                ownership: None,
                replica_retention_after_delete: Duration::ZERO,
                restore_check_sample: 0,
                replication_workers: 4,
                min_residency: std::time::Duration::from_secs(2),
            })
            .build()
            .unwrap();
            drop(plain.provision(&globex).await.unwrap());
            let initech = plain.key_for("initech").unwrap();
            drop(plain.provision(&initech).await.unwrap());
            plain.close_all().await;

            let report = host.fleet.check_replica_sample(3).await.unwrap();
            assert_eq!(report.passed, vec![acme.name()], "{report:?}");
            assert_eq!(report.unreplicated, vec![initech.name()]);
            assert_eq!(report.failed.len(), 1);
            assert_eq!(report.failed[0].0, globex.name());
            assert!(report.failed[0].1.contains("identity"), "{report:?}");
        }

        #[tokio::test]
        async fn a_huge_retention_round_trips_and_is_never_due() {
            let tmp = tempfile::tempdir().unwrap();
            let store = FleetTombstones::new(control(tmp.path()), Arc::new(clock()));
            let retention = Duration::from_secs(u64::MAX / 2);
            store
                .record("tenant:acme", retention, false, "test")
                .await
                .unwrap();
            let row = store
                .confirm("tenant:acme", retention, None)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(store.get("tenant:acme").await.unwrap(), Some(row.clone()));
            assert!(!row.is_due(DateTime::<Utc>::MAX_UTC - chrono::Duration::days(1)));
        }

        fn walk(dir: &Path) -> Vec<std::path::PathBuf> {
            let mut out = Vec::new();
            let Ok(entries) = std::fs::read_dir(dir) else {
                return out;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    out.extend(walk(&path));
                } else {
                    out.push(path);
                }
            }
            out
        }
    }
}
