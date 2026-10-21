//! Slot-ownership epochs and fencing for a fleet (issue #3233, ADR 0019 §7).
//!
//! A host owns slot ranges. It claims a range on a shared ownership table,
//! which gives it an epoch (a [`FencingToken`]), and renews a lease on it. A
//! fleet database opens only when this host owns its slot, and it carries the
//! epoch it opened under.
//!
//! ```text
//! claim(range)  ─► free, own row or lapsed lease? ─► new epoch = counter + 1
//!                  live lease of another owner    ─► refused
//! renew(range)  ─► same owner and epoch           ─► lease extended
//!                  else                           ─► lost: close the range
//! local lease   ─► valid until renew sent + (ttl - ttl/3); then refuse opens
//! ```
//!
//! The decisions are in [`OwnershipTable`], a pure state machine. Each store
//! loads the rows, applies one decision and writes the result in one write
//! transaction. The model and its proofs are in
//! `verification/fleet_slot_ownership.rs`.
//!
//! # Clocks
//!
//! The lease expiry is wall time of the claiming host. The holder stops at
//! `ttl - ttl/3` after it sent the renew, on its monotonic clock. So the
//! clocks of all hosts must agree within `ttl/3`. The epoch, not the clock,
//! protects the replicas: a restore takes the newest epoch, and a replicator
//! that sees a newer epoch stops.

// autumn-determinism-gate: production code in this module must read time and
// mint identifiers through the framework's injected seams (ClockSource /
// Entropy), never `Instant::now()` / `Utc::now()` / `SystemTime::now()` /
// `Uuid::new_v4()` directly. See CONTRIBUTING.md "Determinism seam gate"
// (issue #1797). Justify exceptions with
// #[allow(clippy::disallowed_methods, reason = "…")] at the narrowest scope.
#![cfg_attr(not(test), deny(clippy::disallowed_methods))]

use std::fmt;
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use diesel::RunQueryDsl as _;
use diesel::sql_types::{BigInt, Integer, Text};
use diesel_async::pooled_connection::deadpool::Pool;
use futures::future::BoxFuture;
use tokio::sync::Notify;

use crate::config::SLOT_COUNT;
use crate::db::RuntimeConnection;
use crate::lock::FencingToken;
use crate::time::ClockSource;

/// The ownership table on the control database.
const OWNERS_TABLE: &str = "_autumn_fleet_slot_owners";

/// The DDL, applied at install. Times are epoch milliseconds.
const SCHEMA_SQL: &str = "\
CREATE TABLE IF NOT EXISTS _autumn_fleet_slot_owners (
    range_start      INTEGER NOT NULL,
    range_end        INTEGER NOT NULL,
    owner_id         TEXT    NOT NULL,
    epoch            BIGINT  NOT NULL UNIQUE CHECK (epoch > 0),
    lease_expires_at BIGINT  NOT NULL,
    PRIMARY KEY (range_start, range_end),
    CHECK (range_start <= range_end)
);
CREATE TABLE IF NOT EXISTS _autumn_fleet_epoch_counter (
    id   INTEGER PRIMARY KEY CHECK (id = 1),
    last BIGINT  NOT NULL
);";

/// The `/actuator/health` indicator of this host's ownership.
pub const OWNERSHIP_INDICATOR: &str = "fleet:ownership";

/// The shortest lease. A shorter one cannot be renewed in time.
pub const MIN_LEASE_TTL: Duration = Duration::from_secs(3);

/// The default lease.
pub const DEFAULT_LEASE_TTL: Duration = Duration::from_secs(30);

/// The longest lease. A longer one stalls a failover for too long.
pub const MAX_LEASE_TTL: Duration = Duration::from_secs(3600);

// ── Errors ──────────────────────────────────────────────────────────────────

/// Why an ownership operation failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum OwnershipError {
    /// A slot range is malformed, inverted or out of `0..16384`.
    #[error("invalid slot range: {0}")]
    InvalidRange(String),
    /// Two configured ranges overlap.
    #[error("slot ranges {0} and {1} overlap")]
    Overlap(SlotRange, SlotRange),
    /// An owner id is empty, too long, has control characters, or starts
    /// or ends with white space.
    #[error(
        "invalid fleet owner id {0:?}: use 1 to 128 bytes, no control characters, no white \
         space at the start or end"
    )]
    InvalidOwner(String),
    /// Another owner holds a live lease on part of the range.
    #[error("slot range {range} is held by {owner} until {lease_expires_ms} ms")]
    Held {
        /// The range that holds the claimed slots.
        range: SlotRange,
        /// Its owner.
        owner: String,
        /// When its lease runs out, epoch milliseconds.
        lease_expires_ms: i64,
    },
    /// The range is no longer this owner's at this epoch.
    #[error("slot range {range} was lost: another host claimed it")]
    Lost {
        /// The range.
        range: SlotRange,
    },
    /// The store failed.
    #[error("ownership store: {0}")]
    Store(String),
}

// ── Values ──────────────────────────────────────────────────────────────────

/// An inclusive range of routing slots, inside `0..16384`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SlotRange {
    start: u16,
    end: u16,
}

impl SlotRange {
    /// Every slot.
    pub const ALL: Self = Self {
        start: 0,
        end: SLOT_COUNT - 1,
    };

    /// The range `start..=end`.
    ///
    /// # Errors
    ///
    /// [`OwnershipError::InvalidRange`] when `start > end` or `end >= 16384`.
    pub fn new(start: u16, end: u16) -> Result<Self, OwnershipError> {
        if start > end || end >= SLOT_COUNT {
            return Err(OwnershipError::InvalidRange(format!("{start}-{end}")));
        }
        Ok(Self { start, end })
    }

    /// Parse `"A-B"` or `"N"`.
    ///
    /// # Errors
    ///
    /// [`OwnershipError::InvalidRange`] for anything else.
    pub fn parse(text: &str) -> Result<Self, OwnershipError> {
        let invalid = || OwnershipError::InvalidRange(text.to_owned());
        let number = |s: &str| s.trim().parse::<u16>().map_err(|_| invalid());
        let (start, end) = match text.trim().split_once('-') {
            None => {
                let slot = number(text)?;
                (slot, slot)
            }
            Some((start, end)) => (number(start)?, number(end)?),
        };
        Self::new(start, end).map_err(|_| invalid())
    }

    /// The first slot.
    #[must_use]
    pub const fn start(self) -> u16 {
        self.start
    }

    /// The last slot.
    #[must_use]
    pub const fn end(self) -> u16 {
        self.end
    }

    /// Whether `slot` is in the range.
    #[must_use]
    pub const fn contains(self, slot: u16) -> bool {
        self.start <= slot && slot <= self.end
    }

    /// Whether the two ranges share a slot.
    #[must_use]
    pub const fn overlaps(self, other: Self) -> bool {
        self.start <= other.end && other.start <= self.end
    }
}

impl fmt::Display for SlotRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.start, self.end)
    }
}

/// The id of a host (a volume) in the ownership table.
///
/// Processes that share one fleet root use one id. Two hosts must never use
/// the same id: the table then cannot tell them apart.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OwnerId(String);

impl OwnerId {
    /// Check and wrap an id.
    ///
    /// # Errors
    ///
    /// [`OwnershipError::InvalidOwner`] for an empty id, one over 128 bytes,
    /// one with control characters, or one with white space at the start or
    /// end (`"a"` and `" a"` would be two owners).
    pub fn new(id: impl Into<String>) -> Result<Self, OwnershipError> {
        let id = id.into();
        if id.is_empty() || id.len() > 128 || id.chars().any(char::is_control) || id.trim() != id {
            return Err(OwnershipError::InvalidOwner(id));
        }
        Ok(Self(id))
    }

    /// The id.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for OwnerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One row of the ownership table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnershipRow {
    /// The slots.
    pub range: SlotRange,
    /// The owner.
    pub owner: OwnerId,
    /// The epoch of the claim.
    pub epoch: FencingToken,
    /// When the lease runs out, epoch milliseconds.
    pub lease_expires_ms: i64,
}

/// A granted claim or renewal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grant {
    /// The range.
    pub range: SlotRange,
    /// The epoch.
    pub epoch: FencingToken,
    /// When the lease runs out, epoch milliseconds.
    pub lease_expires_ms: i64,
    /// Whether the claim made a new epoch.
    pub fresh: bool,
}

// ── The state machine ───────────────────────────────────────────────────────

/// The ownership table as a pure state machine. Rows never overlap, and the
/// counter is the highest epoch ever granted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OwnershipTable {
    rows: Vec<OwnershipRow>,
    last_epoch: u64,
}

impl OwnershipTable {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A table from stored rows and the stored counter. The counter is raised
    /// to the highest row epoch if it is lower.
    #[must_use]
    pub fn from_parts(mut rows: Vec<OwnershipRow>, last_epoch: u64) -> Self {
        rows.sort_by_key(|row| row.range);
        let highest = rows.iter().map(|row| row.epoch.get()).max().unwrap_or(0);
        Self {
            rows,
            last_epoch: last_epoch.max(highest),
        }
    }

    /// The rows, by range.
    #[must_use]
    pub fn rows(&self) -> &[OwnershipRow] {
        &self.rows
    }

    /// The highest epoch ever granted.
    #[must_use]
    pub const fn last_epoch(&self) -> u64 {
        self.last_epoch
    }

    /// The row that holds `slot`.
    #[must_use]
    pub fn row_for(&self, slot: u16) -> Option<&OwnershipRow> {
        self.rows.iter().find(|row| row.range.contains(slot))
    }

    /// Whether a write by `owner` under `epoch` to `slot` is current.
    #[must_use]
    pub fn admits(&self, slot: u16, owner: &OwnerId, epoch: FencingToken) -> bool {
        self.row_for(slot)
            .is_some_and(|row| row.owner == *owner && row.epoch == epoch)
    }

    /// Claim `range` for `owner`. See [`claim_above`](Self::claim_above).
    ///
    /// # Errors
    ///
    /// [`OwnershipError::Held`] when a live lease that is not the same claim
    /// covers a slot of the range.
    pub fn claim(
        &mut self,
        range: SlotRange,
        owner: &OwnerId,
        now_ms: i64,
        ttl: Duration,
    ) -> Result<Grant, OwnershipError> {
        self.claim_above(range, owner, now_ms, ttl, 0)
    }

    /// Claim `range` for `owner`, at an epoch above `floor`.
    ///
    /// - A live lease on any slot of the range refuses the claim, unless it
    ///   is the owner's own row with the same bounds. So a range of another
    ///   shape is never taken while it is live, even from the same owner.
    /// - The owner's own row with the same bounds keeps its epoch, if the
    ///   epoch is above `floor`: a restart, or a second process on the same
    ///   volume, is not a new owner.
    /// - Otherwise the rows that overlap the range go (their leases ended),
    ///   and one row with a new epoch replaces them. The new epoch is above
    ///   the counter and above `floor`.
    ///
    /// `floor` is the newest epoch a replicator saw at the destination. A
    /// counter that went back (a control database restored to an earlier
    /// time) then cannot issue an epoch that the replicas fence.
    ///
    /// # Errors
    ///
    /// [`OwnershipError::Held`] when a live lease that is not the same claim
    /// covers a slot of the range.
    pub fn claim_above(
        &mut self,
        range: SlotRange,
        owner: &OwnerId,
        now_ms: i64,
        ttl: Duration,
        floor: u64,
    ) -> Result<Grant, OwnershipError> {
        if let Some(row) = self.rows.iter().find(|row| {
            row.range.overlaps(range)
                && !(row.owner == *owner && row.range == range)
                && row.lease_expires_ms > now_ms
        }) {
            return Err(OwnershipError::Held {
                range: row.range,
                owner: row.owner.to_string(),
                lease_expires_ms: row.lease_expires_ms,
            });
        }
        let lease_expires_ms = lease_end(now_ms, ttl);
        // Rows never overlap, so a row with these bounds is the only one
        // that overlaps the range.
        if let Some(row) = self
            .rows
            .iter_mut()
            .find(|row| row.range == range && row.owner == *owner && row.epoch.get() > floor)
        {
            // Never earlier: a call that timed out may land after a newer one.
            // So a shorter `ttl` takes effect when the old lease ends.
            row.lease_expires_ms = row.lease_expires_ms.max(lease_expires_ms);
            return Ok(Grant {
                range,
                epoch: row.epoch,
                lease_expires_ms: row.lease_expires_ms,
                fresh: false,
            });
        }
        let next = self.last_epoch.max(floor).saturating_add(1);
        let epoch = i64::try_from(next)
            .ok()
            .and_then(|n| FencingToken::try_from(n).ok())
            .ok_or_else(|| OwnershipError::Store("the epoch counter is full".to_owned()))?;
        self.rows.retain(|row| !row.range.overlaps(range));
        self.rows.push(OwnershipRow {
            range,
            owner: owner.clone(),
            epoch,
            lease_expires_ms,
        });
        self.rows.sort_by_key(|row| row.range);
        self.last_epoch = next;
        Ok(Grant {
            range,
            epoch,
            lease_expires_ms,
            fresh: true,
        })
    }

    /// Extend the lease of `owner`'s row for `range` at `epoch`. A lapsed
    /// lease that nobody claimed can be renewed.
    ///
    /// # Errors
    ///
    /// [`OwnershipError::Lost`] when the row is gone or has another owner or
    /// epoch.
    pub fn renew(
        &mut self,
        range: SlotRange,
        owner: &OwnerId,
        epoch: FencingToken,
        now_ms: i64,
        ttl: Duration,
    ) -> Result<Grant, OwnershipError> {
        let row = self
            .held_mut(range, owner, epoch)
            .ok_or(OwnershipError::Lost { range })?;
        // Never earlier: a call that timed out may land after a newer one.
        row.lease_expires_ms = row.lease_expires_ms.max(lease_end(now_ms, ttl));
        Ok(Grant {
            range,
            epoch,
            lease_expires_ms: row.lease_expires_ms,
            fresh: false,
        })
    }

    /// End the lease now, so another owner can claim the range. Returns
    /// whether the row was still `owner`'s at `epoch`.
    pub fn release(
        &mut self,
        range: SlotRange,
        owner: &OwnerId,
        epoch: FencingToken,
        now_ms: i64,
    ) -> bool {
        self.held_mut(range, owner, epoch).is_some_and(|row| {
            row.lease_expires_ms = row.lease_expires_ms.min(now_ms);
            true
        })
    }

    fn held_mut(
        &mut self,
        range: SlotRange,
        owner: &OwnerId,
        epoch: FencingToken,
    ) -> Option<&mut OwnershipRow> {
        self.rows
            .iter_mut()
            .find(|row| row.range == range && row.owner == *owner && row.epoch == epoch)
    }
}

/// `at + by`, or `at` when that overflows.
fn later(at: tokio::time::Instant, by: Duration) -> tokio::time::Instant {
    // `by` is at most the lease, so this does not overflow in practice. If
    // it did, the lease would end now: the safe side.
    at.checked_add(by).unwrap_or(at)
}

/// `now + ttl` in epoch milliseconds, saturating.
fn lease_end(now_ms: i64, ttl: Duration) -> i64 {
    now_ms.saturating_add(i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX))
}

// ── Stores ──────────────────────────────────────────────────────────────────

/// Where the ownership table lives. Every host that serves the fleet must use
/// the same store. Each call is one atomic decision of [`OwnershipTable`].
pub trait SlotOwnershipStore: Send + Sync + 'static {
    /// [`OwnershipTable::claim_above`] at the store's time.
    fn claim<'a>(
        &'a self,
        range: SlotRange,
        owner: &'a OwnerId,
        ttl: Duration,
        floor: u64,
    ) -> BoxFuture<'a, Result<Grant, OwnershipError>>;

    /// [`OwnershipTable::renew`] at the store's time.
    fn renew<'a>(
        &'a self,
        range: SlotRange,
        owner: &'a OwnerId,
        epoch: FencingToken,
        ttl: Duration,
    ) -> BoxFuture<'a, Result<Grant, OwnershipError>>;

    /// [`OwnershipTable::release`] at the store's time.
    fn release<'a>(
        &'a self,
        range: SlotRange,
        owner: &'a OwnerId,
        epoch: FencingToken,
    ) -> BoxFuture<'a, Result<bool, OwnershipError>>;

    /// Every row.
    fn rows(&self) -> BoxFuture<'_, Result<Vec<OwnershipRow>, OwnershipError>>;
}

/// The ownership table on a `SQLite` pool, normally the control database.
///
/// Each call runs in one `BEGIN IMMEDIATE` transaction: it loads the rows,
/// applies one [`OwnershipTable`] decision and writes the changed rows. So two
/// processes that race a claim are serialized, and one wins.
#[derive(Clone)]
pub struct SqlSlotOwnership {
    pool: Pool<RuntimeConnection>,
    clock: Arc<dyn ClockSource>,
}

impl fmt::Debug for SqlSlotOwnership {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SqlSlotOwnership").finish_non_exhaustive()
    }
}

impl SqlSlotOwnership {
    /// A store on `pool`. Lease times come from `clock`.
    #[must_use]
    pub fn new(pool: Pool<RuntimeConnection>, clock: Arc<dyn ClockSource>) -> Self {
        Self { pool, clock }
    }

    /// Create the tables when they do not exist.
    ///
    /// # Errors
    ///
    /// [`OwnershipError::Store`] when the DDL fails.
    pub async fn ensure_schema(&self) -> Result<(), OwnershipError> {
        use diesel_async::SimpleAsyncConnection as _;
        self.pool
            .get()
            .await
            .map_err(store_error)?
            .batch_execute(SCHEMA_SQL)
            .await
            .map_err(store_error)
    }

    /// Load the table, apply `decide` at the store's time, and write what
    /// changed, in one write transaction. A refused decision writes nothing.
    async fn decide<T: Send + 'static>(
        &self,
        decide: impl FnOnce(&mut OwnershipTable, i64) -> Result<T, OwnershipError> + Send + 'static,
    ) -> Result<T, OwnershipError> {
        let now_ms = self.clock.now().timestamp_millis();
        let mut conn = self.pool.get().await.map_err(store_error)?;
        conn.spawn_blocking(move |conn| {
            conn.immediate_transaction(|conn| {
                let before = load_table(conn)?;
                let mut after = before.clone();
                let decided = decide(&mut after, now_ms);
                if decided.is_ok() {
                    store_changes(conn, &before, &after)?;
                }
                Ok(decided)
            })
        })
        .await
        .map_err(store_error)?
    }
}

fn store_error(error: impl fmt::Display) -> OwnershipError {
    OwnershipError::Store(error.to_string())
}

#[derive(diesel::QueryableByName)]
struct RowSql {
    #[diesel(sql_type = Integer)]
    range_start: i32,
    #[diesel(sql_type = Integer)]
    range_end: i32,
    #[diesel(sql_type = Text)]
    owner_id: String,
    #[diesel(sql_type = BigInt)]
    epoch: i64,
    #[diesel(sql_type = BigInt)]
    lease_expires_at: i64,
}

#[derive(diesel::QueryableByName)]
struct LastSql {
    #[diesel(sql_type = BigInt)]
    last: i64,
}

fn corrupt(detail: impl fmt::Display) -> diesel::result::Error {
    diesel::result::Error::DeserializationError(
        format!("{OWNERS_TABLE}: invalid row: {detail}").into(),
    )
}

fn load_table(conn: &mut diesel::SqliteConnection) -> diesel::QueryResult<OwnershipTable> {
    let rows: Vec<RowSql> = diesel::sql_query(format!(
        "SELECT range_start, range_end, owner_id, epoch, lease_expires_at FROM {OWNERS_TABLE}"
    ))
    .load(conn)?;
    let last: Vec<LastSql> =
        diesel::sql_query("SELECT last FROM _autumn_fleet_epoch_counter WHERE id = 1")
            .load(conn)?;
    let rows = rows
        .into_iter()
        .map(|row| {
            let bound = |n: i32| u16::try_from(n).map_err(corrupt);
            Ok(OwnershipRow {
                range: SlotRange::new(bound(row.range_start)?, bound(row.range_end)?)
                    .map_err(corrupt)?,
                owner: OwnerId::new(row.owner_id).map_err(corrupt)?,
                epoch: FencingToken::try_from(row.epoch).map_err(corrupt)?,
                lease_expires_ms: row.lease_expires_at,
            })
        })
        .collect::<diesel::QueryResult<Vec<_>>>()?;
    let last = last
        .first()
        .map_or(0, |row| u64::try_from(row.last).unwrap_or(0));
    Ok(OwnershipTable::from_parts(rows, last))
}

/// Write the rows of `after` that differ from `before`, and the counter.
fn store_changes(
    conn: &mut diesel::SqliteConnection,
    before: &OwnershipTable,
    after: &OwnershipTable,
) -> diesel::QueryResult<()> {
    for gone in before
        .rows()
        .iter()
        .filter(|old| !after.rows().iter().any(|new| new.range == old.range))
    {
        diesel::sql_query(format!(
            "DELETE FROM {OWNERS_TABLE} WHERE range_start = ? AND range_end = ?"
        ))
        .bind::<Integer, _>(i32::from(gone.range.start()))
        .bind::<Integer, _>(i32::from(gone.range.end()))
        .execute(conn)?;
    }
    for row in after
        .rows()
        .iter()
        .filter(|new| !before.rows().contains(new))
    {
        diesel::sql_query(format!(
            "INSERT OR REPLACE INTO {OWNERS_TABLE} \
             (range_start, range_end, owner_id, epoch, lease_expires_at) VALUES (?, ?, ?, ?, ?)"
        ))
        .bind::<Integer, _>(i32::from(row.range.start()))
        .bind::<Integer, _>(i32::from(row.range.end()))
        .bind::<Text, _>(row.owner.as_str())
        .bind::<BigInt, _>(row.epoch.as_i64())
        .bind::<BigInt, _>(row.lease_expires_ms)
        .execute(conn)?;
    }
    if after.last_epoch() != before.last_epoch() {
        diesel::sql_query(
            "INSERT INTO _autumn_fleet_epoch_counter (id, last) VALUES (1, ?) \
             ON CONFLICT (id) DO UPDATE SET last = excluded.last",
        )
        .bind::<BigInt, _>(i64::try_from(after.last_epoch()).map_err(corrupt)?)
        .execute(conn)?;
    }
    Ok(())
}

impl SlotOwnershipStore for SqlSlotOwnership {
    fn claim<'a>(
        &'a self,
        range: SlotRange,
        owner: &'a OwnerId,
        ttl: Duration,
        floor: u64,
    ) -> BoxFuture<'a, Result<Grant, OwnershipError>> {
        let owner = owner.clone();
        Box::pin(self.decide(move |table, now| table.claim_above(range, &owner, now, ttl, floor)))
    }

    fn renew<'a>(
        &'a self,
        range: SlotRange,
        owner: &'a OwnerId,
        epoch: FencingToken,
        ttl: Duration,
    ) -> BoxFuture<'a, Result<Grant, OwnershipError>> {
        let owner = owner.clone();
        Box::pin(self.decide(move |table, now| table.renew(range, &owner, epoch, now, ttl)))
    }

    fn release<'a>(
        &'a self,
        range: SlotRange,
        owner: &'a OwnerId,
        epoch: FencingToken,
    ) -> BoxFuture<'a, Result<bool, OwnershipError>> {
        let owner = owner.clone();
        Box::pin(self.decide(move |table, now| Ok(table.release(range, &owner, epoch, now))))
    }

    fn rows(&self) -> BoxFuture<'_, Result<Vec<OwnershipRow>, OwnershipError>> {
        Box::pin(self.decide(|table, _| Ok(table.rows().to_vec())))
    }
}

// ── This host's ownership ───────────────────────────────────────────────────

/// A range this host holds, and until when it may serve it.
#[derive(Debug, Clone, Copy)]
struct Held {
    range: SlotRange,
    epoch: FencingToken,
    /// Renew sent + `ttl - ttl/3`, on the monotonic clock.
    valid_until: tokio::time::Instant,
}

/// A store call that runs on its own task.
type PendingCall = tokio::task::JoinHandle<()>;

/// The `failed` text of a call that took longer than one renew interval.
const TIMED_OUT: &str = "the store call timed out";

/// The `failed` text of a range whose last call still runs.
const STILL_RUNNING: &str = "the last store call is still running";

/// What one range did in a refresh.
enum Outcome {
    Claimed(FencingToken),
    Renewed,
    Lost,
    Waiting,
    Failed(String),
    TimedOut,
    Busy,
}

/// What one [`FleetOwnership::refresh`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefreshReport {
    /// Ranges claimed now.
    pub claimed: Vec<(SlotRange, FencingToken)>,
    /// Ranges renewed.
    pub renewed: Vec<SlotRange>,
    /// Ranges another host took. Their open databases must close.
    pub lost: Vec<SlotRange>,
    /// Ranges another host still holds.
    pub waiting: Vec<SlotRange>,
    /// Ranges whose call failed, with the error. A call that takes longer
    /// than one renew interval fails with a time-out.
    pub failed: Vec<(SlotRange, String)>,
}

/// The ranges one host claims, and the epochs it holds. Started with
/// `DatabaseFleet::start_ownership`.
pub struct FleetOwnership {
    store: Arc<dyn SlotOwnershipStore>,
    owner: OwnerId,
    ranges: Vec<SlotRange>,
    ttl: Duration,
    held: RwLock<Vec<Held>>,
    last: Mutex<RefreshReport>,
    changed: Notify,
    /// Per range of `ranges`: the newest epoch a replicator saw at the
    /// destination for a slot in it. Claims of the range go above it.
    fence_floors: Vec<std::sync::atomic::AtomicU64>,
    /// Per range of `ranges`: a store call that outlived its time limit. It
    /// runs on, so the range makes no new call until it ends.
    pending: Vec<Mutex<Option<PendingCall>>>,
    /// A store call outlived the shutdown wait. It may still land and
    /// extend a lease, so the ranges lapse instead of being released.
    unsettled: std::sync::atomic::AtomicBool,
}

impl fmt::Debug for FleetOwnership {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FleetOwnership")
            .field("owner", &self.owner)
            .field("ranges", &self.ranges)
            .finish_non_exhaustive()
    }
}

impl FleetOwnership {
    /// Ownership of `ranges` by `owner` on `store`, with leases of `ttl`.
    ///
    /// # Errors
    ///
    /// [`OwnershipError::Overlap`] when two ranges overlap, or
    /// [`OwnershipError::InvalidRange`] for no range or a `ttl` outside
    /// [`MIN_LEASE_TTL`]..=[`MAX_LEASE_TTL`].
    pub fn new(
        store: Arc<dyn SlotOwnershipStore>,
        owner: OwnerId,
        ranges: Vec<SlotRange>,
        ttl: Duration,
    ) -> Result<Self, OwnershipError> {
        if ranges.is_empty() {
            return Err(OwnershipError::InvalidRange("no slot range".to_owned()));
        }
        if !(MIN_LEASE_TTL..=MAX_LEASE_TTL).contains(&ttl) {
            return Err(OwnershipError::InvalidRange(format!(
                "the lease must be {}s to {}s",
                MIN_LEASE_TTL.as_secs(),
                MAX_LEASE_TTL.as_secs()
            )));
        }
        let mut sorted = ranges.clone();
        sorted.sort_unstable();
        if let Some(pair) = sorted.windows(2).find(|pair| pair[0].overlaps(pair[1])) {
            return Err(OwnershipError::Overlap(pair[0], pair[1]));
        }
        let pending = ranges.iter().map(|_| Mutex::new(None)).collect();
        let fence_floors = ranges
            .iter()
            .map(|_| std::sync::atomic::AtomicU64::new(0))
            .collect();
        Ok(Self {
            store,
            owner,
            ranges,
            ttl,
            held: RwLock::new(Vec::new()),
            last: Mutex::new(RefreshReport::default()),
            changed: Notify::new(),
            fence_floors,
            pending,
            unsettled: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// This host's id.
    #[must_use]
    pub const fn owner(&self) -> &OwnerId {
        &self.owner
    }

    /// The ranges this host claims.
    #[must_use]
    pub fn ranges(&self) -> &[SlotRange] {
        &self.ranges
    }

    /// The lease length.
    #[must_use]
    pub const fn lease_ttl(&self) -> Duration {
        self.ttl
    }

    /// How often to renew: a third of the lease. It is also the time limit
    /// of one store call.
    #[must_use]
    pub fn renew_interval(&self) -> Duration {
        self.ttl / 3
    }

    /// Claim the ranges this host does not hold, renew the others. Run it
    /// before the first open, then every [`renew_interval`](Self::renew_interval).
    ///
    /// The ranges run at the same time, and each call has one renew interval.
    /// A range whose store call is slow fails alone: the other ranges still
    /// renew. The report keeps the order of the configured ranges. The order
    /// of the epochs between ranges is not fixed.
    pub async fn refresh(&self) -> RefreshReport {
        let calls = self
            .ranges
            .iter()
            .zip(&self.fence_floors)
            .zip(&self.pending)
            .map(|((&range, floor), pending)| async move {
                (range, self.refresh_range(range, floor, pending).await)
            });
        let mut report = RefreshReport::default();
        for (range, outcome) in futures::future::join_all(calls).await {
            match outcome {
                Outcome::TimedOut => report.failed.push((range, TIMED_OUT.to_owned())),
                Outcome::Busy => report.failed.push((range, STILL_RUNNING.to_owned())),
                Outcome::Claimed(epoch) => report.claimed.push((range, epoch)),
                Outcome::Renewed => report.renewed.push(range),
                Outcome::Lost => report.lost.push(range),
                Outcome::Waiting => report.waiting.push(range),
                Outcome::Failed(error) => report.failed.push((range, error)),
            }
        }
        *self.last.lock().unwrap_or_else(PoisonError::into_inner) = report.clone();
        report
    }

    /// Claim or renew one range, within one renew interval.
    ///
    /// The call runs on its own task. A call that is too slow keeps running
    /// (its transaction and pool connection cannot be cancelled), so the range
    /// makes no new call until it ends.
    async fn refresh_range(
        &self,
        range: SlotRange,
        floor: &std::sync::atomic::AtomicU64,
        pending: &Mutex<Option<PendingCall>>,
    ) -> Outcome {
        {
            let mut slot = pending.lock().unwrap_or_else(PoisonError::into_inner);
            if slot.as_ref().is_some_and(|call| !call.is_finished()) {
                return Outcome::Busy;
            }
            // An ended call is stale: a newer call replaces its answer.
            *slot = None;
        }
        let held = self.held_epoch(range);
        // Sent before the call: the local lease must not outlive the store's.
        let sent = tokio::time::Instant::now();
        let (store, owner, ttl) = (Arc::clone(&self.store), self.owner.clone(), self.ttl);
        let floor_now = floor.load(std::sync::atomic::Ordering::Acquire);
        let (answer, answered) = tokio::sync::oneshot::channel();
        let call = tokio::spawn(async move {
            let result = if let Some(epoch) = held {
                store.renew(range, &owner, epoch, ttl).await
            } else {
                store.claim(range, &owner, ttl, floor_now).await
            };
            let _ = answer.send(result);
        });
        // Tracked at once: if this refresh is dropped, the call still runs and
        // `unsettled` must say so.
        *pending.lock().unwrap_or_else(PoisonError::into_inner) = Some(call);
        let result = match tokio::time::timeout(self.renew_interval(), answered).await {
            Ok(Ok(result)) => {
                // The call ended. Nothing runs for this range now.
                *pending.lock().unwrap_or_else(PoisonError::into_inner) = None;
                result
            }
            Ok(Err(_closed)) => {
                *pending.lock().unwrap_or_else(PoisonError::into_inner) = None;
                return Outcome::Failed("the store call stopped without an answer".to_owned());
            }
            Err(_elapsed) => {
                // The call may still extend the lease at the store. The local
                // lease does not grow, so serving stays safe.
                tracing::warn!(range = %range, "fleet slot ownership call timed out");
                return Outcome::TimedOut;
            }
        };
        match result {
            // A replicator saw a newer epoch in this range while the call ran.
            Ok(grant) if grant.epoch.get() < floor.load(std::sync::atomic::Ordering::Acquire) => {
                tracing::warn!(range = %range, epoch = %grant.epoch, "fleet slot range fenced while the store call ran");
                self.drop_range(range);
                Outcome::Lost
            }
            Ok(grant) => {
                let valid_for = self.ttl.saturating_sub(self.renew_interval());
                self.hold(grant, later(sent, valid_for));
                if held.is_some() {
                    Outcome::Renewed
                } else {
                    tracing::info!(range = %range, epoch = %grant.epoch, owner = %self.owner, "fleet slot range claimed");
                    Outcome::Claimed(grant.epoch)
                }
            }
            Err(OwnershipError::Lost { .. }) => {
                tracing::warn!(range = %range, owner = %self.owner, "fleet slot range lost to another host");
                self.drop_range(range);
                Outcome::Lost
            }
            Err(OwnershipError::Held { .. }) => Outcome::Waiting,
            Err(error) => {
                tracing::warn!(range = %range, %error, "fleet slot ownership call failed");
                Outcome::Failed(error.to_string())
            }
        }
    }

    fn held_epoch(&self, range: SlotRange) -> Option<FencingToken> {
        self.read_held()
            .iter()
            .find(|held| held.range == range)
            .map(|held| held.epoch)
    }

    fn hold(&self, grant: Grant, valid_until: tokio::time::Instant) {
        let mut held = self.write_held();
        held.retain(|held| held.range != grant.range);
        held.push(Held {
            range: grant.range,
            epoch: grant.epoch,
            valid_until,
        });
    }

    fn drop_range(&self, range: SlotRange) {
        self.write_held().retain(|held| held.range != range);
    }

    fn read_held(&self) -> std::sync::RwLockReadGuard<'_, Vec<Held>> {
        self.held.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write_held(&self) -> std::sync::RwLockWriteGuard<'_, Vec<Held>> {
        self.held.write().unwrap_or_else(PoisonError::into_inner)
    }

    /// The epoch this host may serve `slot` under now. `None` when it does
    /// not hold the slot, or its local lease ran out (it could not renew).
    #[must_use]
    pub fn epoch_for(&self, slot: u16) -> Option<FencingToken> {
        let now = tokio::time::Instant::now();
        self.read_held()
            .iter()
            .find(|held| held.range.contains(slot) && now < held.valid_until)
            .map(|held| held.epoch)
    }

    /// The held ranges and their epochs, valid or not.
    #[must_use]
    pub fn held(&self) -> Vec<(SlotRange, FencingToken)> {
        let mut held: Vec<_> = self
            .read_held()
            .iter()
            .map(|held| (held.range, held.epoch))
            .collect();
        held.sort_unstable();
        held
    }

    /// Drop the range that holds `slot` when its epoch is older than
    /// `newer` (a replicator saw `newer` at the destination). Returns whether
    /// a range was dropped.
    pub fn lose(&self, slot: u16, newer: FencingToken) -> bool {
        for (range, floor) in self.ranges.iter().zip(&self.fence_floors) {
            if range.contains(slot) {
                floor.fetch_max(newer.get(), std::sync::atomic::Ordering::AcqRel);
            }
        }
        let range = self
            .read_held()
            .iter()
            .find(|held| held.range.contains(slot) && held.epoch < newer)
            .map(|held| held.range);
        if let Some(range) = range {
            tracing::warn!(range = %range, newer = %newer, "fleet slot range fenced by a newer epoch at the replica");
            self.drop_range(range);
            // Wake the ownership loop: it closes the range's databases.
            self.changed.notify_waiters();
        }
        range.is_some()
    }

    /// Release every held range, so another host can claim it at once. Call
    /// it after the fleet closed its databases.
    pub async fn release_all(&self) {
        let held: Vec<Held> = std::mem::take(&mut *self.write_held());
        for held in held {
            if let Err(error) = self
                .store
                .release(held.range, &self.owner, held.epoch)
                .await
            {
                tracing::warn!(range = %held.range, %error, "fleet slot range not released; it lapses with its lease");
            }
        }
    }

    /// The report of the last refresh.
    #[must_use]
    pub fn last_report(&self) -> RefreshReport {
        self.last
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// When the ownership loop must run again: the renew interval, or the
    /// first local lease that runs out before it.
    #[must_use]
    pub fn next_wake(&self) -> tokio::time::Instant {
        self.next_wake_since(tokio::time::Instant::now())
    }

    /// As [`next_wake`](Self::next_wake), but the renew interval counts from
    /// `started`, when the last refresh began. A slow range then does not
    /// delay the renewal of the others.
    #[must_use]
    pub fn next_wake_since(&self, started: tokio::time::Instant) -> tokio::time::Instant {
        let renew = later(started, self.renew_interval());
        self.next_lapse().map_or(renew, |lapse| lapse.min(renew))
    }

    /// When the first local lease that still runs will end. `None` when no
    /// local lease runs.
    #[must_use]
    pub fn next_lapse(&self) -> Option<tokio::time::Instant> {
        let now = tokio::time::Instant::now();
        self.read_held()
            .iter()
            .map(|held| held.valid_until)
            .filter(|until| *until > now)
            .min()
    }

    /// Mark that a store call outlived the shutdown wait.
    pub(crate) fn mark_unsettled(&self) {
        self.unsettled
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Whether a store call may still land: no release then. True while a
    /// timed-out call runs, and for good after the shutdown wait ran out.
    pub(crate) fn unsettled(&self) -> bool {
        self.unsettled.load(std::sync::atomic::Ordering::Acquire)
            || self.pending.iter().any(|slot| {
                slot.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .as_ref()
                    .is_some_and(|call| !call.is_finished())
            })
    }

    /// Wakes when a range is lost.
    pub(crate) const fn changed(&self) -> &Notify {
        &self.changed
    }

    /// The `fleet:ownership` health indicator. `Down` when the last refresh
    /// could not reach the store: the host stops serving when its local
    /// leases run out. A range another host holds is not an error.
    #[must_use]
    pub fn indicator(self: &Arc<Self>) -> Arc<dyn crate::actuator::HealthIndicator> {
        Arc::new(OwnershipIndicator(Arc::clone(self)))
    }

    fn health(&self) -> crate::actuator::HealthCheckOutput {
        use crate::actuator::HealthCheckOutput;
        let report = self.last_report();
        let now = tokio::time::Instant::now();
        let mut ranges: Vec<Held> = self.read_held().clone();
        ranges.sort_unstable_by_key(|held| held.range);
        let held: Vec<serde_json::Value> = ranges
            .iter()
            .map(|held| {
                serde_json::json!({
                    "range": held.range.to_string(),
                    "epoch": held.epoch.get(),
                    "serving": now < held.valid_until,
                })
            })
            .collect();
        let names = |ranges: &[SlotRange]| -> Vec<String> {
            ranges.iter().map(ToString::to_string).collect()
        };
        let mut details = std::collections::HashMap::new();
        details.insert("owner".to_owned(), self.owner.as_str().into());
        details.insert("held".to_owned(), held.into());
        details.insert("waiting".to_owned(), names(&report.waiting).into());
        details.insert("lost".to_owned(), names(&report.lost).into());
        details.insert(
            "failed".to_owned(),
            report
                .failed
                .iter()
                .map(|(range, error)| format!("{range}: {error}"))
                .collect::<Vec<_>>()
                .into(),
        );
        // A held range whose local lease ran out was not renewed in time:
        // the last call failed, or it still hangs.
        let lapsed = self.read_held().iter().any(|held| now >= held.valid_until);
        if report.failed.is_empty() && !lapsed {
            HealthCheckOutput::up().with_details(details)
        } else {
            HealthCheckOutput::down().with_details(details)
        }
    }
}

struct OwnershipIndicator(Arc<FleetOwnership>);

impl crate::actuator::HealthIndicator for OwnershipIndicator {
    fn check(&self) -> BoxFuture<'_, crate::actuator::HealthCheckOutput> {
        Box::pin(std::future::ready(self.0.health()))
    }

    fn group(&self) -> crate::actuator::IndicatorGroup {
        crate::actuator::IndicatorGroup::HealthOnly
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TTL: Duration = Duration::from_secs(30);

    fn owner(id: &str) -> OwnerId {
        OwnerId::new(id).unwrap()
    }

    fn range(start: u16, end: u16) -> SlotRange {
        SlotRange::new(start, end).unwrap()
    }

    fn token(n: u64) -> FencingToken {
        FencingToken::try_from(i64::try_from(n).unwrap()).unwrap()
    }

    #[test]
    fn slot_ranges_parse_and_refuse_bad_input() {
        assert_eq!(SlotRange::parse("0-8191").unwrap(), range(0, 8191));
        assert_eq!(SlotRange::parse(" 7 ").unwrap(), range(7, 7));
        for bad in ["", "9-1", "0-16384", "a-b", "1-2-3"] {
            assert!(SlotRange::parse(bad).is_err(), "{bad:?}");
        }
        assert!(range(0, 10).overlaps(range(10, 20)));
        assert!(!range(0, 9).overlaps(range(10, 20)));
        assert_eq!(SlotRange::ALL.to_string(), "0-16383");
    }

    #[test]
    fn owner_ids_refuse_empty_long_and_control_text() {
        assert!(OwnerId::new("host-a").is_ok());
        for bad in [
            String::new(),
            "x".repeat(129),
            "a\nb".to_owned(),
            " host-a".to_owned(),
            "host-a ".to_owned(),
        ] {
            assert!(OwnerId::new(bad).is_err());
        }
    }

    #[test]
    fn a_claim_on_an_empty_table_grants_epoch_one() {
        let mut table = OwnershipTable::new();
        let grant = table.claim(range(0, 99), &owner("a"), 1_000, TTL).unwrap();
        assert_eq!(grant.epoch, token(1));
        assert!(grant.fresh);
        assert_eq!(grant.lease_expires_ms, 31_000);
        assert!(table.admits(50, &owner("a"), token(1)));
        assert!(!table.admits(100, &owner("a"), token(1)));
    }

    #[test]
    fn two_hosts_racing_a_claim_get_one_owner() {
        let mut table = OwnershipTable::new();
        table.claim(range(0, 99), &owner("a"), 1_000, TTL).unwrap();
        let refused = table.claim(range(0, 99), &owner("b"), 1_001, TTL);
        assert_eq!(
            refused,
            Err(OwnershipError::Held {
                range: range(0, 99),
                owner: "a".to_owned(),
                lease_expires_ms: 31_000,
            })
        );
        // Part of the range is enough to refuse.
        assert!(
            table
                .claim(range(50, 200), &owner("b"), 1_001, TTL)
                .is_err()
        );
        assert_eq!(table.rows().len(), 1);
    }

    #[test]
    fn a_lapsed_lease_is_claimed_at_a_new_epoch_and_fences_the_old_owner() {
        let mut table = OwnershipTable::new();
        let old = table.claim(range(0, 99), &owner("a"), 0, TTL).unwrap();
        let new = table.claim(range(0, 99), &owner("b"), 30_000, TTL).unwrap();
        assert!(new.epoch > old.epoch);
        assert!(new.fresh);
        assert_eq!(
            table.renew(range(0, 99), &owner("a"), old.epoch, 30_001, TTL),
            Err(OwnershipError::Lost {
                range: range(0, 99)
            })
        );
        assert!(!table.release(range(0, 99), &owner("a"), old.epoch, 30_001));
        assert!(!table.admits(5, &owner("a"), old.epoch));
        assert!(table.admits(5, &owner("b"), new.epoch));
    }

    #[test]
    fn the_same_owner_reclaims_without_a_new_epoch() {
        let mut table = OwnershipTable::new();
        let first = table.claim(range(0, 99), &owner("a"), 0, TTL).unwrap();
        // A restart of the owner, or a second process on the same volume.
        let again = table.claim(range(0, 99), &owner("a"), 60_000, TTL).unwrap();
        assert_eq!(again.epoch, first.epoch);
        assert!(!again.fresh);
        assert_eq!(again.lease_expires_ms, 90_000);
    }

    #[test]
    fn renew_extends_the_lease_and_works_after_a_lapse_nobody_used() {
        let mut table = OwnershipTable::new();
        let grant = table.claim(range(0, 99), &owner("a"), 0, TTL).unwrap();
        let renewed = table
            .renew(range(0, 99), &owner("a"), grant.epoch, 40_000, TTL)
            .unwrap();
        assert_eq!(renewed.epoch, grant.epoch);
        assert_eq!(renewed.lease_expires_ms, 70_000);
        assert!(!renewed.fresh);
        // Another range of the same owner is not this one.
        assert!(
            table
                .renew(range(0, 98), &owner("a"), grant.epoch, 40_000, TTL)
                .is_err()
        );
    }

    #[test]
    fn a_late_renew_does_not_shorten_a_newer_lease() {
        let mut table = OwnershipTable::new();
        let grant = table.claim(range(0, 99), &owner("a"), 0, TTL).unwrap();
        table
            .renew(range(0, 99), &owner("a"), grant.epoch, 40_000, TTL)
            .unwrap();
        // An older call, sent at 10 s, lands after the one sent at 40 s.
        let late = table
            .renew(range(0, 99), &owner("a"), grant.epoch, 10_000, TTL)
            .unwrap();
        assert_eq!(late.lease_expires_ms, 70_000);
        assert_eq!(table.rows()[0].lease_expires_ms, 70_000);
    }

    #[test]
    fn a_late_reclaim_does_not_shorten_a_newer_lease() {
        let mut table = OwnershipTable::new();
        table.claim(range(0, 99), &owner("a"), 40_000, TTL).unwrap();
        let late = table.claim(range(0, 99), &owner("a"), 10_000, TTL).unwrap();
        assert_eq!(late.lease_expires_ms, 70_000);
        assert_eq!(table.rows()[0].lease_expires_ms, 70_000);
    }

    #[test]
    fn release_lets_another_owner_claim_at_once() {
        let mut table = OwnershipTable::new();
        let grant = table.claim(range(0, 99), &owner("a"), 0, TTL).unwrap();
        assert!(table.release(range(0, 99), &owner("a"), grant.epoch, 1_000));
        let next = table.claim(range(0, 99), &owner("b"), 1_000, TTL).unwrap();
        assert_eq!(next.epoch, token(2));
    }

    #[test]
    fn a_live_range_of_another_shape_is_refused_even_to_its_owner() {
        let mut table = OwnershipTable::new();
        table.claim(range(0, 199), &owner("a"), 0, TTL).unwrap();
        // An old process of owner "a" may still serve 100-199.
        assert!(matches!(
            table.claim(range(0, 99), &owner("a"), 1_000, TTL),
            Err(OwnershipError::Held { .. })
        ));
        assert_eq!(table.last_epoch(), 1);
    }

    #[test]
    fn a_reshaped_range_takes_a_new_epoch_and_frees_the_rest() {
        let mut table = OwnershipTable::new();
        let wide = table.claim(range(0, 199), &owner("a"), 0, TTL).unwrap();
        // The old shape's lease ends (released at shutdown, or lapsed).
        assert!(table.release(range(0, 199), &owner("a"), wide.epoch, 1_000));
        let narrow = table.claim(range(0, 99), &owner("a"), 1_000, TTL).unwrap();
        assert!(narrow.fresh);
        assert_eq!(narrow.epoch, token(2));
        assert!(table.row_for(150).is_none());
        // The freed half is claimable at once by another host.
        let other = table
            .claim(range(100, 199), &owner("b"), 1_000, TTL)
            .unwrap();
        assert_eq!(other.epoch, token(3));
    }

    #[test]
    fn a_lapsed_range_of_another_shape_is_absorbed() {
        let mut table = OwnershipTable::new();
        table.claim(range(0, 99), &owner("a"), 0, TTL).unwrap();
        table.claim(range(100, 199), &owner("c"), 0, TTL).unwrap();
        let grant = table
            .claim(range(0, 199), &owner("b"), 30_000, TTL)
            .unwrap();
        assert_eq!(grant.epoch, token(3));
        assert_eq!(table.rows().len(), 1);
    }

    #[test]
    fn a_claim_goes_above_the_epoch_a_replicator_saw() {
        let mut table = OwnershipTable::new();
        let first = table.claim(range(0, 99), &owner("a"), 0, TTL).unwrap();
        assert_eq!(first.epoch, token(1));
        // The counter went back (a restored control database), and a replica
        // shows epoch 7. The owner's own row does not keep its epoch.
        let grant = table
            .claim_above(range(0, 99), &owner("a"), 1_000, TTL, 7)
            .unwrap();
        assert!(grant.fresh);
        assert_eq!(grant.epoch, token(8));
        assert_eq!(table.last_epoch(), 8);
        // Above the floor, the own row keeps its epoch as before.
        let again = table
            .claim_above(range(0, 99), &owner("a"), 2_000, TTL, 7)
            .unwrap();
        assert_eq!(again.epoch, token(8));
        assert!(!again.fresh);
    }

    #[test]
    fn epochs_are_never_reused_after_rows_are_removed() {
        let mut table = OwnershipTable::new();
        let first = table.claim(range(0, 99), &owner("a"), 0, TTL).unwrap();
        table.release(range(0, 99), &owner("a"), first.epoch, 0);
        table.claim(range(0, 49), &owner("a"), 0, TTL).unwrap();
        let rebuilt = OwnershipTable::from_parts(table.rows().to_vec(), table.last_epoch());
        let mut rebuilt = rebuilt;
        let grant = rebuilt.claim(range(50, 99), &owner("b"), 0, TTL).unwrap();
        assert_eq!(grant.epoch, token(3));
        // A counter below the rows is raised to them.
        let raised = OwnershipTable::from_parts(table.rows().to_vec(), 0);
        assert_eq!(raised.last_epoch(), 2);
    }

    // ── SQL store ───────────────────────────────────────────────────────────

    fn control(dir: &std::path::Path) -> Pool<RuntimeConnection> {
        crate::db::create_pool(&crate::config::DatabaseConfig {
            url: Some(format!("sqlite://{}", dir.join("control.db").display())),
            ..Default::default()
        })
        .unwrap()
        .unwrap()
    }

    fn clock_at(ms: i64) -> crate::time::TickingClock {
        crate::time::TickingClock::starting_at(chrono::DateTime::from_timestamp_millis(ms).unwrap())
    }

    async fn store(dir: &std::path::Path, clock: &crate::time::TickingClock) -> SqlSlotOwnership {
        let store = SqlSlotOwnership::new(control(dir), Arc::new(clock.clone()));
        store.ensure_schema().await.unwrap();
        store
    }

    #[tokio::test]
    async fn two_hosts_racing_a_claim_on_one_store_get_one_owner() {
        let dir = tempfile::tempdir().unwrap();
        let clock = clock_at(1_000_000);
        // Two pools on one file: two processes.
        let (a, b) = (
            store(dir.path(), &clock).await,
            store(dir.path(), &clock).await,
        );
        let (ha, hb) = (owner("a"), owner("b"));
        let (ra, rb) = tokio::join!(
            a.claim(SlotRange::ALL, &ha, TTL, 0),
            b.claim(SlotRange::ALL, &hb, TTL, 0)
        );
        assert!(ra.is_ok() != rb.is_ok(), "{ra:?} {rb:?}");
        let loser = if ra.is_ok() { rb } else { ra };
        assert!(
            matches!(loser, Err(OwnershipError::Held { .. })),
            "the loser is refused, not busy: {loser:?}"
        );
        let rows = a.rows().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].epoch, token(1));
    }

    #[tokio::test]
    async fn the_store_fences_the_old_owner_after_a_lapse() {
        let dir = tempfile::tempdir().unwrap();
        let clock = clock_at(1_000_000);
        let s = store(dir.path(), &clock).await;
        let old = s.claim(range(0, 99), &owner("a"), TTL, 0).await.unwrap();
        clock.advance(TTL);
        let new = s.claim(range(0, 99), &owner("b"), TTL, 0).await.unwrap();
        assert_eq!(new.epoch, token(2));
        assert_eq!(
            s.renew(range(0, 99), &owner("a"), old.epoch, TTL).await,
            Err(OwnershipError::Lost {
                range: range(0, 99)
            })
        );
        assert!(
            !s.release(range(0, 99), &owner("a"), old.epoch)
                .await
                .unwrap()
        );
        assert!(
            s.release(range(0, 99), &owner("b"), new.epoch)
                .await
                .unwrap()
        );
        // Re-shaping removes the row; the counter keeps the epochs unique.
        let c = s.claim(range(0, 49), &owner("c"), TTL, 0).await.unwrap();
        assert_eq!(c.epoch, token(3));
        let reopened = store(dir.path(), &clock).await;
        let d = reopened
            .claim(range(50, 99), &owner("d"), TTL, 0)
            .await
            .unwrap();
        assert_eq!(d.epoch, token(4));
    }

    // ── This host ───────────────────────────────────────────────────────────

    /// A store a test can cut off from one host.
    struct Partitionable {
        inner: SqlSlotOwnership,
        cut: std::sync::atomic::AtomicBool,
    }

    impl Partitionable {
        fn check(&self) -> Result<(), OwnershipError> {
            if self.cut.load(std::sync::atomic::Ordering::SeqCst) {
                Err(OwnershipError::Store("partitioned".to_owned()))
            } else {
                Ok(())
            }
        }
    }

    impl SlotOwnershipStore for Partitionable {
        fn claim<'a>(
            &'a self,
            range: SlotRange,
            owner: &'a OwnerId,
            ttl: Duration,
            floor: u64,
        ) -> BoxFuture<'a, Result<Grant, OwnershipError>> {
            Box::pin(async move {
                self.check()?;
                self.inner.claim(range, owner, ttl, floor).await
            })
        }
        fn renew<'a>(
            &'a self,
            range: SlotRange,
            owner: &'a OwnerId,
            epoch: FencingToken,
            ttl: Duration,
        ) -> BoxFuture<'a, Result<Grant, OwnershipError>> {
            Box::pin(async move {
                self.check()?;
                self.inner.renew(range, owner, epoch, ttl).await
            })
        }
        fn release<'a>(
            &'a self,
            range: SlotRange,
            owner: &'a OwnerId,
            epoch: FencingToken,
        ) -> BoxFuture<'a, Result<bool, OwnershipError>> {
            Box::pin(async move {
                self.check()?;
                self.inner.release(range, owner, epoch).await
            })
        }
        fn rows(&self) -> BoxFuture<'_, Result<Vec<OwnershipRow>, OwnershipError>> {
            self.inner.rows()
        }
    }

    /// A store a test can script: hang every call, or run a step inside one.
    struct Scripted {
        inner: SqlSlotOwnership,
        hang: std::sync::atomic::AtomicBool,
        /// Milliseconds a renewal waits, on another task: dropping the call
        /// does not stop it, as with work handed to `spawn_blocking`. 0 is
        /// no wait.
        slow_ms: std::sync::atomic::AtomicU64,
        /// Runs once, before the next renew reaches the store.
        before_renew: Mutex<Option<Box<dyn FnOnce() + Send>>>,
        /// Runs once, after the next claim reached the store.
        after_claim: Mutex<Option<Box<dyn FnOnce() -> BoxFuture<'static, ()> + Send>>>,
    }

    impl Scripted {
        fn new(inner: SqlSlotOwnership) -> Self {
            Self {
                inner,
                hang: std::sync::atomic::AtomicBool::new(false),
                slow_ms: std::sync::atomic::AtomicU64::new(0),
                before_renew: Mutex::new(None),
                after_claim: Mutex::new(None),
            }
        }

        async fn gate(&self) {
            if self.hang.load(std::sync::atomic::Ordering::SeqCst) {
                std::future::pending::<()>().await;
            }
        }
    }

    impl SlotOwnershipStore for Scripted {
        fn claim<'a>(
            &'a self,
            range: SlotRange,
            owner: &'a OwnerId,
            ttl: Duration,
            floor: u64,
        ) -> BoxFuture<'a, Result<Grant, OwnershipError>> {
            Box::pin(async move {
                self.gate().await;
                let grant = self.inner.claim(range, owner, ttl, floor).await;
                let step = self.after_claim.lock().unwrap().take();
                if let Some(step) = step {
                    step().await;
                }
                grant
            })
        }
        fn renew<'a>(
            &'a self,
            range: SlotRange,
            owner: &'a OwnerId,
            epoch: FencingToken,
            ttl: Duration,
        ) -> BoxFuture<'a, Result<Grant, OwnershipError>> {
            Box::pin(async move {
                self.gate().await;
                let step = self.before_renew.lock().unwrap().take();
                if let Some(step) = step {
                    step();
                }
                let slow = self.slow_ms.load(std::sync::atomic::Ordering::SeqCst);
                if slow > 0 {
                    let (inner, owner) = (self.inner.clone(), owner.clone());
                    return tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(slow)).await;
                        inner.renew(range, &owner, epoch, ttl).await
                    })
                    .await
                    .unwrap();
                }
                self.inner.renew(range, owner, epoch, ttl).await
            })
        }
        fn release<'a>(
            &'a self,
            range: SlotRange,
            owner: &'a OwnerId,
            epoch: FencingToken,
        ) -> BoxFuture<'a, Result<bool, OwnershipError>> {
            Box::pin(async move {
                self.gate().await;
                self.inner.release(range, owner, epoch).await
            })
        }
        fn rows(&self) -> BoxFuture<'_, Result<Vec<OwnershipRow>, OwnershipError>> {
            self.inner.rows()
        }
    }

    /// A store that never answers for the ranges in `stuck`.
    struct StuckRanges {
        inner: SqlSlotOwnership,
        stuck: Mutex<Vec<SlotRange>>,
        /// Calls that reached the store.
        calls: std::sync::atomic::AtomicUsize,
    }

    impl StuckRanges {
        async fn gate(&self, range: SlotRange) {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            // Stuck until the test clears the range.
            while self.stuck.lock().unwrap().contains(&range) {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }

    impl SlotOwnershipStore for StuckRanges {
        fn claim<'a>(
            &'a self,
            range: SlotRange,
            owner: &'a OwnerId,
            ttl: Duration,
            floor: u64,
        ) -> BoxFuture<'a, Result<Grant, OwnershipError>> {
            Box::pin(async move {
                self.gate(range).await;
                self.inner.claim(range, owner, ttl, floor).await
            })
        }
        fn renew<'a>(
            &'a self,
            range: SlotRange,
            owner: &'a OwnerId,
            epoch: FencingToken,
            ttl: Duration,
        ) -> BoxFuture<'a, Result<Grant, OwnershipError>> {
            Box::pin(async move {
                self.gate(range).await;
                self.inner.renew(range, owner, epoch, ttl).await
            })
        }
        fn release<'a>(
            &'a self,
            range: SlotRange,
            owner: &'a OwnerId,
            epoch: FencingToken,
        ) -> BoxFuture<'a, Result<bool, OwnershipError>> {
            Box::pin(async move {
                self.gate(range).await;
                self.inner.release(range, owner, epoch).await
            })
        }
        fn rows(&self) -> BoxFuture<'_, Result<Vec<OwnershipRow>, OwnershipError>> {
            self.inner.rows()
        }
    }

    async fn two_ranges_one_stuck(
        dir: &std::path::Path,
    ) -> (
        Arc<StuckRanges>,
        Arc<FleetOwnership>,
        crate::time::TickingClock,
    ) {
        let clock = clock_at(1_000_000);
        let stuck = Arc::new(StuckRanges {
            inner: store(dir, &clock).await,
            stuck: Mutex::new(Vec::new()),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let a = Arc::new(
            FleetOwnership::new(
                stuck.clone(),
                owner("a"),
                vec![range(0, 9), range(10, 19)],
                TTL,
            )
            .unwrap(),
        );
        (stuck, a, clock)
    }

    #[tokio::test(start_paused = true)]
    async fn a_stuck_renewal_on_one_range_does_not_lapse_the_other() {
        let dir = tempfile::tempdir().unwrap();
        let (stuck, a, clock) = two_ranges_one_stuck(dir.path()).await;
        assert_eq!(a.refresh().await.claimed.len(), 2);
        stuck.stuck.lock().unwrap().push(range(0, 9));

        // Three cycles: more than one local lease.
        let step = a.renew_interval();
        let mut report = RefreshReport::default();
        for _ in 0..3 {
            clock.advance(step);
            tokio::time::advance(step).await;
            report = a.refresh().await;
            assert_eq!(report.renewed, vec![range(10, 19)]);
        }
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].0, range(0, 9));
        assert_eq!(a.epoch_for(15), Some(token(2)), "the live range serves");
        assert_eq!(a.epoch_for(3), None, "the stuck range lapsed");
        assert!(a.unsettled(), "a timed out call may still land");

        let health = a.indicator().check().await;
        assert_eq!(health.status, crate::actuator::HealthStatus::Down);
        let held = health.details["held"].as_array().unwrap();
        assert_eq!(held.len(), 2, "each range stays listed");
        assert_eq!(held[0]["serving"], false);
        assert_eq!(held[1]["serving"], true);
        assert_eq!(health.details["failed"].as_array().unwrap().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stuck_claim_on_one_range_does_not_block_the_other() {
        let dir = tempfile::tempdir().unwrap();
        let (stuck, a, _clock) = two_ranges_one_stuck(dir.path()).await;
        stuck.stuck.lock().unwrap().push(range(0, 9));
        let report = a.refresh().await;
        assert_eq!(report.claimed.len(), 1);
        assert_eq!(report.claimed[0].0, range(10, 19));
        assert_eq!(report.failed[0].0, range(0, 9));
        assert!(a.epoch_for(15).is_some());
        assert_eq!(a.epoch_for(3), None);
    }

    #[tokio::test(start_paused = true)]
    async fn the_report_keeps_range_order_whatever_the_call_order() {
        let dir = tempfile::tempdir().unwrap();
        let (_stuck, a, _clock) = two_ranges_one_stuck(dir.path()).await;
        let report = a.refresh().await;
        let ranges: Vec<_> = report.claimed.iter().map(|(range, _)| *range).collect();
        assert_eq!(ranges, vec![range(0, 9), range(10, 19)]);
    }

    #[tokio::test(start_paused = true)]
    async fn the_calls_run_at_the_same_time_and_a_range_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let (stuck, a, _clock) = two_ranges_one_stuck(dir.path()).await;
        stuck.stuck.lock().unwrap().push(range(0, 9));
        let start = tokio::time::Instant::now();
        let report = a.refresh().await;
        assert_eq!(
            start.elapsed(),
            a.renew_interval(),
            "one interval, not one per range"
        );
        assert_eq!(
            report.failed,
            vec![(range(0, 9), "the store call timed out".to_owned())]
        );
        assert!(a.unsettled());
        let health = a.indicator().check().await;
        assert_eq!(health.details["failed"][0], "0-9: the store call timed out");

        // The stuck call still runs, so the range makes no new call.
        let report = a.refresh().await;
        assert_eq!(
            report.failed,
            vec![(
                range(0, 9),
                "the last store call is still running".to_owned()
            )]
        );

        // The store answers; the next refresh claims the range.
        stuck.stuck.lock().unwrap().clear();
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(!a.unsettled(), "no call runs now");
        let report = a.refresh().await;
        assert!(report.failed.is_empty());
        assert_eq!(report.claimed.len(), 1);
        assert_eq!(report.renewed, vec![range(10, 19)]);
        // 0-9 was claimed after 10-19, but the details keep config order.
        let health = a.indicator().check().await;
        assert_eq!(health.details["held"][0]["range"], "0-9");
        assert_eq!(health.details["held"][1]["range"], "10-19");
    }

    #[tokio::test(start_paused = true)]
    async fn a_call_still_running_is_not_started_again() {
        let dir = tempfile::tempdir().unwrap();
        let (stuck, a, clock) = two_ranges_one_stuck(dir.path()).await;
        a.refresh().await;
        stuck.stuck.lock().unwrap().push(range(0, 9));
        let before = stuck.calls.load(std::sync::atomic::Ordering::SeqCst);
        let step = a.renew_interval();
        for _ in 0..3 {
            clock.advance(step);
            let report = a.refresh().await;
            assert_eq!(report.failed.len(), 1);
            assert_eq!(report.renewed, vec![range(10, 19)]);
        }
        // One call for the stuck range, three for the live one.
        let after = stuck.calls.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(after - before, 4);
    }

    #[tokio::test(start_paused = true)]
    async fn a_refresh_dropped_in_flight_leaves_its_call_tracked() {
        let dir = tempfile::tempdir().unwrap();
        let (stuck, a, _clock) = two_ranges_one_stuck(dir.path()).await;
        stuck.stuck.lock().unwrap().push(range(0, 9));
        let running = Arc::clone(&a);
        let refresh = tokio::spawn(async move { running.refresh().await });
        tokio::time::sleep(Duration::from_secs(1)).await;
        refresh.abort();
        let _ = refresh.await;
        assert!(a.unsettled(), "the call to the store still runs");
        stuck.stuck.lock().unwrap().clear();
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(!a.unsettled(), "it ended");
    }

    #[tokio::test(start_paused = true)]
    async fn a_time_out_and_a_lost_range_report_apart() {
        let dir = tempfile::tempdir().unwrap();
        let (stuck, a, _clock) = two_ranges_one_stuck(dir.path()).await;
        a.refresh().await;
        stuck.stuck.lock().unwrap().push(range(0, 9));
        assert!(a.lose(15, token(9)));
        let report = a.refresh().await;
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].0, range(0, 9));
        assert_eq!(report.claimed, vec![(range(10, 19), token(10))]);
    }

    #[tokio::test(start_paused = true)]
    async fn the_next_wake_counts_from_the_start_of_the_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let (stuck, a, _clock) = two_ranges_one_stuck(dir.path()).await;
        a.refresh().await;
        stuck.stuck.lock().unwrap().push(range(0, 9));
        let started = tokio::time::Instant::now();
        a.refresh().await;
        assert!(
            a.next_wake_since(started) <= tokio::time::Instant::now(),
            "a slow range does not delay the next renewal"
        );
    }

    #[test]
    fn ownership_refuses_overlapping_ranges_and_a_short_lease() {
        let dir = tempfile::tempdir().unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let clock = clock_at(0);
        let s: Arc<dyn SlotOwnershipStore> = Arc::new(rt.block_on(store(dir.path(), &clock)));
        assert_eq!(
            FleetOwnership::new(
                Arc::clone(&s),
                owner("a"),
                vec![range(0, 9), range(9, 20)],
                TTL
            )
            .err(),
            Some(OwnershipError::Overlap(range(0, 9), range(9, 20)))
        );
        assert!(FleetOwnership::new(Arc::clone(&s), owner("a"), vec![], TTL).is_err());
        assert!(
            FleetOwnership::new(s, owner("a"), vec![range(0, 9)], Duration::from_secs(1)).is_err()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_host_claims_waits_for_a_lapse_and_loses_on_a_newer_claim() {
        let dir = tempfile::tempdir().unwrap();
        let clock = clock_at(1_000_000);
        let shared = Arc::new(store(dir.path(), &clock).await);
        let a = FleetOwnership::new(shared.clone(), owner("a"), vec![range(0, 99)], TTL).unwrap();
        let b = FleetOwnership::new(shared.clone(), owner("b"), vec![range(0, 99)], TTL).unwrap();

        let report = a.refresh().await;
        assert_eq!(report.claimed, vec![(range(0, 99), token(1))]);
        assert_eq!(a.epoch_for(5), Some(token(1)));
        assert_eq!(a.epoch_for(100), None);
        assert_eq!(b.refresh().await.waiting, vec![range(0, 99)]);
        assert_eq!(b.epoch_for(5), None);

        // A renews, so B keeps waiting.
        clock.advance(Duration::from_secs(10));
        tokio::time::advance(Duration::from_secs(10)).await;
        assert_eq!(a.refresh().await.renewed, vec![range(0, 99)]);
        assert_eq!(b.refresh().await.waiting, vec![range(0, 99)]);

        // A stops renewing; the lease lapses and B claims at epoch 2.
        clock.advance(TTL);
        tokio::time::advance(TTL).await;
        assert_eq!(a.epoch_for(5), None, "A's local lease ran out first");
        assert_eq!(b.refresh().await.claimed, vec![(range(0, 99), token(2))]);
        assert_eq!(b.epoch_for(5), Some(token(2)));

        // A comes back: its renew is refused and it drops the range.
        let report = a.refresh().await;
        assert_eq!(report.lost, vec![range(0, 99)]);
        assert!(a.held().is_empty());
        assert_eq!(a.refresh().await.waiting, vec![range(0, 99)]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_partitioned_host_stops_serving_before_another_can_claim() {
        let dir = tempfile::tempdir().unwrap();
        let clock = clock_at(1_000_000);
        let inner = store(dir.path(), &clock).await;
        let cut = Arc::new(Partitionable {
            inner: inner.clone(),
            cut: std::sync::atomic::AtomicBool::new(false),
        });
        let a = FleetOwnership::new(cut.clone(), owner("a"), vec![SlotRange::ALL], TTL).unwrap();
        a.refresh().await;
        assert!(a.epoch_for(7).is_some());
        cut.cut.store(true, std::sync::atomic::Ordering::SeqCst);
        // Renewals fail; the local lease ends at ttl - ttl/3.
        let step = a.renew_interval();
        for _ in 0..2 {
            clock.advance(step);
            tokio::time::advance(step).await;
            assert!(!a.refresh().await.failed.is_empty());
        }
        assert_eq!(a.epoch_for(7), None);
        // The store still holds A's lease here: nobody else serves yet.
        let b =
            FleetOwnership::new(Arc::new(inner), owner("b"), vec![SlotRange::ALL], TTL).unwrap();
        assert_eq!(b.refresh().await.waiting, vec![SlotRange::ALL]);
    }

    #[tokio::test]
    async fn lose_and_release_drop_the_range() {
        let dir = tempfile::tempdir().unwrap();
        let clock = clock_at(1_000_000);
        let shared = Arc::new(store(dir.path(), &clock).await);
        let a = FleetOwnership::new(
            shared.clone(),
            owner("a"),
            vec![range(0, 9), range(10, 19)],
            TTL,
        )
        .unwrap();
        a.refresh().await;
        let a = Arc::new(a);
        let health = a.indicator().check().await;
        assert_eq!(health.status, crate::actuator::HealthStatus::Up);
        assert_eq!(health.details["owner"], "a");
        assert_eq!(health.details["held"][0]["range"], "0-9");
        assert_eq!(health.details["held"][0]["serving"], true);
        assert!(!a.lose(3, token(1)), "the same epoch is not newer");
        assert!(a.lose(3, token(9)));
        assert_eq!(a.epoch_for(3), None);
        assert_eq!(a.held(), vec![(range(10, 19), token(2))]);
        a.release_all().await;
        assert!(a.held().is_empty());
        // A store it cannot reach turns the indicator down.
        let cut = Arc::new(Partitionable {
            inner: (*shared).clone(),
            cut: std::sync::atomic::AtomicBool::new(true),
        });
        let c = Arc::new(FleetOwnership::new(cut, owner("c"), vec![range(30, 39)], TTL).unwrap());
        c.refresh().await;
        let health = c.indicator().check().await;
        assert_eq!(health.status, crate::actuator::HealthStatus::Down);
        let b = FleetOwnership::new(shared, owner("b"), vec![range(10, 19)], TTL).unwrap();
        assert_eq!(
            b.refresh().await.claimed.len(),
            1,
            "released, so claimable at once"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_indicator_is_down_while_a_hung_renewal_lets_the_lease_lapse() {
        let dir = tempfile::tempdir().unwrap();
        let clock = clock_at(1_000_000);
        let scripted = Arc::new(Scripted::new(store(dir.path(), &clock).await));
        let a = Arc::new(
            FleetOwnership::new(scripted.clone(), owner("a"), vec![SlotRange::ALL], TTL).unwrap(),
        );
        a.refresh().await;
        assert_eq!(
            a.indicator().check().await.status,
            crate::actuator::HealthStatus::Up
        );
        scripted
            .hang
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let hung = Arc::clone(&a);
        let renewal = tokio::spawn(async move { hung.refresh().await });
        tokio::time::sleep(TTL).await;
        assert_eq!(a.epoch_for(7), None);
        assert_eq!(
            a.indicator().check().await.status,
            crate::actuator::HealthStatus::Down
        );
        renewal.abort();
    }

    #[tokio::test]
    async fn a_fence_on_one_range_leaves_the_others_held() {
        let dir = tempfile::tempdir().unwrap();
        let clock = clock_at(1_000_000);
        let shared = Arc::new(store(dir.path(), &clock).await);
        let a =
            FleetOwnership::new(shared, owner("a"), vec![range(0, 9), range(10, 19)], TTL).unwrap();
        assert_eq!(a.refresh().await.claimed.len(), 2);
        assert!(a.lose(3, token(9)));
        let report = a.refresh().await;
        assert_eq!(report.renewed, vec![range(10, 19)]);
        assert!(report.lost.is_empty());
        assert_eq!(report.claimed, vec![(range(0, 9), token(10))]);
        assert_eq!(a.epoch_for(15), Some(token(2)));
    }

    #[tokio::test]
    async fn a_renewal_that_returns_after_a_fence_does_not_bring_the_range_back() {
        let dir = tempfile::tempdir().unwrap();
        let clock = clock_at(1_000_000);
        let scripted = Arc::new(Scripted::new(store(dir.path(), &clock).await));
        let a = Arc::new(
            FleetOwnership::new(scripted.clone(), owner("a"), vec![SlotRange::ALL], TTL).unwrap(),
        );
        assert_eq!(a.refresh().await.claimed, vec![(SlotRange::ALL, token(1))]);
        // A replicator sees epoch 5 while the renewal is in flight.
        let fenced = Arc::clone(&a);
        *scripted.before_renew.lock().unwrap() = Some(Box::new(move || {
            assert!(fenced.lose(7, token(5)));
        }));
        assert_eq!(a.refresh().await.lost, vec![SlotRange::ALL]);
        assert_eq!(a.epoch_for(7), None, "the renewed epoch is under the floor");
        // The next refresh claims above the floor.
        assert_eq!(a.refresh().await.claimed, vec![(SlotRange::ALL, token(6))]);
    }

    // ── The fleet ───────────────────────────────────────────────────────────

    mod fleet {
        use super::*;
        use crate::config::DatabaseFleetConfig;
        use crate::db::fleet::{DatabaseFleet, FleetDatabase, FleetError};
        use crate::db::fleet_replication::FleetReplication;
        use crate::fleet_layout::{FleetDbKey, FleetMode};
        use crate::replication::{FileDestination, ReplicaDestination, ReplicationSettings};

        struct Host {
            fleet: DatabaseFleet,
            replication: Arc<FleetReplication>,
            ownership: Arc<FleetOwnership>,
            /// The replicator's clock: step it to make a fence check due.
            clock: crate::time::TickingClock,
        }

        /// A host with its own volume, on the shared replicas and store.
        async fn host(
            dir: &std::path::Path,
            name: &str,
            store_clock: &crate::time::TickingClock,
            ranges: Vec<SlotRange>,
        ) -> Host {
            host_on(dir, name, name, store_clock, ranges).await
        }

        /// A process of owner `name` on volume `volume`.
        async fn host_on(
            dir: &std::path::Path,
            volume: &str,
            name: &str,
            store_clock: &crate::time::TickingClock,
            ranges: Vec<SlotRange>,
        ) -> Host {
            let (fleet, replication, clock) = plain(dir, volume);
            let store = Arc::new(store(dir, store_clock).await);
            let ownership = Arc::new(FleetOwnership::new(store, owner(name), ranges, TTL).unwrap());
            fleet.start_ownership(Arc::clone(&ownership)).await.unwrap();
            Host {
                fleet,
                replication,
                ownership,
                clock,
            }
        }

        /// A replicated fleet on volume `volume`, with no ownership.
        fn plain(
            dir: &std::path::Path,
            volume: &str,
        ) -> (
            DatabaseFleet,
            Arc<FleetReplication>,
            crate::time::TickingClock,
        ) {
            let fleet = DatabaseFleet::builder(DatabaseFleetConfig {
                mode: FleetMode::Tenant,
                root: dir.join(volume).display().to_string(),
                path: None,
                max_open: 8,
                pool_size: 2,
                create_on_demand: Some(true),
                idle_close_secs: 0,
                restore_missing: true,
                ownership: None,
            })
            .build()
            .unwrap();
            let destination: Arc<dyn ReplicaDestination> =
                Arc::new(FileDestination::new(dir.join("replicas")).unwrap());
            let clock = clock_at(1_000_000);
            let replication = Arc::new(FleetReplication::new(
                destination,
                ReplicationSettings {
                    database_path: std::path::PathBuf::new(),
                    root: "app/test".to_owned(),
                    sync_interval: Duration::from_secs(1),
                    snapshot_interval: Duration::from_secs(3600),
                    max_wal_bytes: 4 * 1024 * 1024,
                    retention: Duration::from_secs(24 * 3600),
                    verify_interval: None,
                },
                Duration::from_secs(30),
                true,
                Arc::new(clock.clone()),
            ));
            assert!(fleet.install_replication(Arc::clone(&replication)).is_ok());
            (fleet, replication, clock)
        }

        async fn exec(db: &FleetDatabase, sql: &str) -> Result<usize, String> {
            let mut conn = db.pool().get().await.map_err(|e| e.to_string())?;
            diesel_async::RunQueryDsl::execute(diesel::sql_query(sql), &mut *conn)
                .await
                .map_err(|e| e.to_string())
        }

        async fn bodies(db: &FleetDatabase) -> Vec<String> {
            #[derive(diesel::QueryableByName)]
            struct Body {
                #[diesel(sql_type = Text)]
                body: String,
            }
            let mut conn = db.pool().get().await.unwrap();
            diesel_async::RunQueryDsl::load::<Body>(
                diesel::sql_query("SELECT body FROM notes ORDER BY rowid"),
                &mut *conn,
            )
            .await
            .unwrap()
            .into_iter()
            .map(|b| b.body)
            .collect()
        }

        async fn tick(replication: &Arc<FleetReplication>) {
            let replication = Arc::clone(replication);
            crate::time::spawn_blocking(move || replication.tick_all())
                .await
                .unwrap();
        }

        fn acme(fleet: &DatabaseFleet) -> FleetDbKey {
            fleet.key_for("acme").unwrap()
        }

        #[tokio::test]
        async fn a_database_opens_under_the_epoch_of_its_slot() {
            let dir = tempfile::tempdir().unwrap();
            let clock = clock_at(1_000_000);
            let a = host(dir.path(), "a", &clock, vec![SlotRange::ALL]).await;
            let key = acme(&a.fleet);
            assert!(a.fleet.owns(&key));
            let db = a.fleet.open(&key).await.unwrap();
            assert_eq!(db.epoch(), Some(token(1)));
            a.fleet.close_all().await;
        }

        #[tokio::test]
        async fn an_unowned_key_is_refused_with_421_and_never_restored() {
            let dir = tempfile::tempdir().unwrap();
            let clock = clock_at(1_000_000);
            let a = host(dir.path(), "a", &clock, vec![SlotRange::ALL]).await;
            let key = acme(&a.fleet);
            let db = a.fleet.open(&key).await.unwrap();
            exec(&db, "CREATE TABLE notes (body TEXT NOT NULL)")
                .await
                .unwrap();
            drop(db);
            a.fleet.close(&key).await; // ships a replica

            // B's volume is empty, and A still holds the range.
            let b = host(dir.path(), "b", &clock, vec![SlotRange::ALL]).await;
            assert!(!b.fleet.owns(&key));
            let refused = b.fleet.open(&key).await.unwrap_err();
            assert!(matches!(refused, FleetError::NotOwner { .. }), "{refused}");
            assert_eq!(refused.http_status(), http::StatusCode::MISDIRECTED_REQUEST);
            assert!(
                !b.fleet.exists(&key),
                "restore_missing ran for an unowned key"
            );
            assert!(matches!(
                b.fleet.provision(&key).await,
                Err(FleetError::NotOwner { .. })
            ));
            assert!(matches!(
                b.fleet.open_background(&key).await,
                Err(FleetError::NotOwner { .. })
            ));
        }

        #[tokio::test]
        async fn an_open_database_is_refused_once_its_range_is_lost() {
            let dir = tempfile::tempdir().unwrap();
            let clock = clock_at(1_000_000);
            let a = host(dir.path(), "a", &clock, vec![SlotRange::ALL]).await;
            let key = acme(&a.fleet);
            a.fleet.open(&key).await.unwrap();
            assert!(a.ownership.lose(key.routing_slot(), token(2)));
            // The cached entry is not served under a lost epoch.
            assert!(matches!(
                a.fleet.open(&key).await,
                Err(FleetError::NotOwner { .. })
            ));
            a.fleet.close_all().await;
        }

        #[tokio::test]
        async fn the_old_owner_cannot_write_after_another_host_takes_the_epoch() {
            let dir = tempfile::tempdir().unwrap();
            let clock = clock_at(1_000_000);
            let a = host(dir.path(), "a", &clock, vec![SlotRange::ALL]).await;
            let key = acme(&a.fleet);
            let held = a.fleet.open(&key).await.unwrap();
            exec(&held, "CREATE TABLE notes (body TEXT NOT NULL)")
                .await
                .unwrap();

            // A cannot renew in time (a partition, a pause). Its lease
            // lapses in the store, and B claims the range at epoch 2.
            clock.advance(TTL);
            let b = host(dir.path(), "b", &clock, vec![SlotRange::ALL]).await;
            assert_eq!(b.ownership.held(), vec![(SlotRange::ALL, token(2))]);

            // A comes back: it loses the range and closes the database.
            assert_eq!(a.ownership.refresh().await.lost, vec![SlotRange::ALL]);
            assert_eq!(a.fleet.fence_unowned(), 1);
            a.fleet.close(&key).await;
            assert!(held.pool().is_closed());
            assert!(
                exec(&held, "INSERT INTO notes VALUES ('late')")
                    .await
                    .is_err()
            );
            assert!(matches!(
                a.fleet.open(&key).await,
                Err(FleetError::NotOwner { .. })
            ));
        }

        #[tokio::test]
        async fn a_restarting_process_locks_the_volume_before_it_claims() {
            let dir = tempfile::tempdir().unwrap();
            let clock = clock_at(1_000_000);
            // The old process of owner "a" serves the volume.
            let old = host_on(dir.path(), "vol", "a", &clock, vec![SlotRange::ALL]).await;
            // The new process starts. Right after its claim reaches the
            // store, the old process stops and tries to release.
            let (fleet, _replication, _) = plain(dir.path(), "vol");
            let scripted = Arc::new(Scripted::new(store(dir.path(), &clock).await));
            let released = Arc::new(Mutex::new(None));
            let (stopping, seen) = (old.fleet.clone(), Arc::clone(&released));
            *scripted.after_claim.lock().unwrap() = Some(Box::new(move || {
                Box::pin(async move {
                    stopping.close_all().await;
                    let last = stopping.release_ownership().await;
                    *seen.lock().unwrap() = Some(last);
                })
            }));
            let ownership = Arc::new(
                FleetOwnership::new(scripted, owner("a"), vec![SlotRange::ALL], TTL).unwrap(),
            );
            fleet.start_ownership(Arc::clone(&ownership)).await.unwrap();
            assert_eq!(
                *released.lock().unwrap(),
                Some(false),
                "the new process holds the volume"
            );
            assert_eq!(ownership.held(), vec![(SlotRange::ALL, token(1))]);
            let b = host(dir.path(), "b", &clock, vec![SlotRange::ALL]).await;
            assert!(b.ownership.held().is_empty(), "nobody else may claim it");
        }

        #[tokio::test(start_paused = true)]
        async fn shutdown_ends_the_ownership_loop_while_the_store_hangs() {
            let dir = tempfile::tempdir().unwrap();
            let clock = clock_at(1_000_000);
            let scripted = Arc::new(Scripted::new(store(dir.path(), &clock).await));
            let (fleet, _replication, _) = plain(dir.path(), "vol");
            let ownership = Arc::new(
                FleetOwnership::new(scripted.clone(), owner("a"), vec![SlotRange::ALL], TTL)
                    .unwrap(),
            );
            fleet.start_ownership(Arc::clone(&ownership)).await.unwrap();
            scripted
                .hang
                .store(true, std::sync::atomic::Ordering::SeqCst);
            let stop = tokio_util::sync::CancellationToken::new();
            let handle = fleet.spawn_ownership(stop.clone());
            // Past the local lease: the loop waits on a renewal that never
            // ends.
            tokio::time::sleep(TTL).await;
            assert_eq!(ownership.epoch_for(7), None);
            stop.cancel();
            // It gives the call a renew interval, then stops.
            tokio::time::timeout(ownership.renew_interval() * 2, handle)
                .await
                .expect("the loop ends at shutdown")
                .unwrap();
            // The release cannot hold shutdown either: the leases lapse.
            let released = tokio::time::timeout(TTL, fleet.release_ownership())
                .await
                .expect("the release ends at shutdown");
            assert!(!released);
        }

        #[tokio::test(start_paused = true)]
        async fn shutdown_lets_a_running_renewal_finish_before_the_release() {
            let dir = tempfile::tempdir().unwrap();
            let clock = clock_at(1_000_000);
            let scripted = Arc::new(Scripted::new(store(dir.path(), &clock).await));
            let (fleet, _replication, _) = plain(dir.path(), "vol");
            let ownership = Arc::new(
                FleetOwnership::new(scripted.clone(), owner("a"), vec![SlotRange::ALL], TTL)
                    .unwrap(),
            );
            fleet.start_ownership(Arc::clone(&ownership)).await.unwrap();
            scripted
                .slow_ms
                .store(1_000, std::sync::atomic::Ordering::SeqCst);
            let stop = tokio_util::sync::CancellationToken::new();
            let handle = fleet.spawn_ownership(stop.clone());
            // The loop's renewal is running when shutdown starts.
            tokio::time::sleep(Duration::from_millis(10)).await;
            stop.cancel();
            tokio::time::timeout(Duration::from_secs(5), handle)
                .await
                .expect("the loop ends at shutdown")
                .unwrap();
            assert!(fleet.release_ownership().await);
            // No renewal lands after the release: another host claims.
            tokio::time::sleep(Duration::from_secs(2)).await;
            let b = FleetOwnership::new(
                Arc::new(store(dir.path(), &clock).await),
                owner("b"),
                vec![SlotRange::ALL],
                TTL,
            )
            .unwrap();
            assert_eq!(b.refresh().await.claimed.len(), 1);
        }

        #[tokio::test(start_paused = true)]
        async fn a_renewal_still_running_after_the_shutdown_wait_blocks_the_release() {
            let dir = tempfile::tempdir().unwrap();
            let clock = clock_at(1_000_000);
            let scripted = Arc::new(Scripted::new(store(dir.path(), &clock).await));
            let (fleet, _replication, _) = plain(dir.path(), "vol");
            let ownership = Arc::new(
                FleetOwnership::new(scripted.clone(), owner("a"), vec![SlotRange::ALL], TTL)
                    .unwrap(),
            );
            fleet.start_ownership(Arc::clone(&ownership)).await.unwrap();
            // Slower than the shutdown wait, but it does land.
            let slow = ownership.renew_interval() + Duration::from_secs(5);
            scripted.slow_ms.store(
                u64::try_from(slow.as_millis()).unwrap(),
                std::sync::atomic::Ordering::SeqCst,
            );
            let stop = tokio_util::sync::CancellationToken::new();
            let handle = fleet.spawn_ownership(stop.clone());
            tokio::time::sleep(Duration::from_millis(10)).await;
            stop.cancel();
            handle.await.unwrap();
            // The renewal may land after a release and extend the lease
            // again: the ranges lapse instead.
            assert!(!fleet.release_ownership().await);
        }

        #[tokio::test]
        async fn ownership_does_not_start_without_the_volume_lock() {
            let dir = tempfile::tempdir().unwrap();
            let clock = clock_at(1_000_000);
            let (fleet, _replication, _) = plain(dir.path(), "vol");
            // The lock directory cannot be made: a file has its name.
            let root = dir.path().join("vol");
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(root.join(crate::fleet_layout::RESERVED_DIR), b"").unwrap();
            let shared = Arc::new(store(dir.path(), &clock).await);
            let ownership = Arc::new(
                FleetOwnership::new(shared.clone(), owner("a"), vec![SlotRange::ALL], TTL).unwrap(),
            );
            assert!(fleet.start_ownership(Arc::clone(&ownership)).await.is_err());
            assert!(fleet.ownership().is_none());
            // Nothing was claimed: a process that cannot hold the lock could
            // have its epoch released by the one it replaces.
            assert!(shared.rows().await.unwrap().is_empty());
        }

        #[tokio::test]
        async fn only_the_last_process_on_a_volume_releases() {
            let dir = tempfile::tempdir().unwrap();
            let clock = clock_at(1_000_000);
            // A web and a worker process of owner "a" share one volume.
            let web = host_on(dir.path(), "vol", "a", &clock, vec![SlotRange::ALL]).await;
            let worker = host_on(dir.path(), "vol", "a", &clock, vec![SlotRange::ALL]).await;
            assert_eq!(worker.ownership.held(), web.ownership.held(), "one epoch");
            let b = host(dir.path(), "b", &clock, vec![SlotRange::ALL]).await;

            // The worker stops: the web process still serves, so no release.
            worker.fleet.close_all().await;
            assert!(!worker.fleet.release_ownership().await);
            assert_eq!(b.ownership.refresh().await.waiting, vec![SlotRange::ALL]);

            // The last process stops and releases: B claims at once.
            web.fleet.close_all().await;
            assert!(web.fleet.release_ownership().await);
            assert_eq!(
                b.ownership.refresh().await.claimed,
                vec![(SlotRange::ALL, token(2))]
            );
        }

        #[tokio::test]
        async fn generations_keep_the_newest_epoch_when_ownership_is_turned_off() {
            let dir = tempfile::tempdir().unwrap();
            let clock = clock_at(1_000_000);
            let a = host(dir.path(), "a", &clock, vec![SlotRange::ALL]).await;
            let key = acme(&a.fleet);
            let db = a.fleet.open(&key).await.unwrap();
            exec(&db, "CREATE TABLE notes (body TEXT NOT NULL)")
                .await
                .unwrap();
            exec(&db, "INSERT INTO notes VALUES ('epoch 1')")
                .await
                .unwrap();
            drop(db);
            a.fleet.close_all().await;

            // The same data, later, on a host with ownership turned off.
            let (plain_fleet, replication, _) = plain(dir.path(), "plain");
            let db = plain_fleet.open(&key).await.unwrap();
            assert_eq!(db.epoch(), None);
            exec(&db, "INSERT INTO notes VALUES ('no ownership')")
                .await
                .unwrap();
            tick(&replication).await;
            drop(db);
            plain_fleet.close_all().await;

            // A restore takes the newer generation, not the old epoch-1 one.
            let (fresh, _, _) = plain(dir.path(), "fresh");
            let db = fresh.open(&key).await.unwrap();
            assert_eq!(bodies(&db).await, vec!["epoch 1", "no ownership"]);
            fresh.close_all().await;
        }

        #[tokio::test]
        async fn fan_out_skips_slots_this_host_does_not_own() {
            let dir = tempfile::tempdir().unwrap();
            let clock = clock_at(1_000_000);
            let halves = vec![range(0, 8191), range(8192, 16383)];
            let a = host(dir.path(), "a", &clock, halves).await;
            // One tenant in each half.
            let names: Vec<String> = (0..64).map(|i| format!("t{i}")).collect();
            let low = names
                .iter()
                .find(|n| a.fleet.key_for(n).unwrap().routing_slot() < 8192)
                .unwrap();
            let high = names
                .iter()
                .find(|n| a.fleet.key_for(n).unwrap().routing_slot() >= 8192)
                .unwrap();
            for tenant in [low, high] {
                a.fleet
                    .provision(&a.fleet.key_for(tenant).unwrap())
                    .await
                    .unwrap();
            }
            assert!(a.ownership.lose(0, token(9)));
            let visited = a
                .fleet
                .each(2, |db| async move { Ok(db.key().name()) })
                .await
                .unwrap();
            assert_eq!(visited.len(), 1);
            assert_eq!(visited[0].0, a.fleet.key_for(high).unwrap());
            a.fleet.close_all().await;
        }

        #[tokio::test]
        async fn a_host_that_gets_its_range_back_serves_the_newer_owners_data() {
            let dir = tempfile::tempdir().unwrap();
            let clock = clock_at(1_000_000);
            let a = host(dir.path(), "a", &clock, vec![SlotRange::ALL]).await;
            let key = acme(&a.fleet);
            let db_a = a.fleet.open(&key).await.unwrap();
            exec(&db_a, "CREATE TABLE notes (body TEXT NOT NULL)")
                .await
                .unwrap();
            exec(&db_a, "INSERT INTO notes VALUES ('a1')")
                .await
                .unwrap();
            tick(&a.replication).await;
            drop(db_a);
            a.fleet.close(&key).await;

            // A cannot renew. B takes over from the replica and writes.
            clock.advance(TTL);
            let b = host(dir.path(), "b", &clock, vec![SlotRange::ALL]).await;
            assert_eq!(a.ownership.refresh().await.lost, vec![SlotRange::ALL]);
            let db_b = b.fleet.open(&key).await.unwrap();
            exec(&db_b, "INSERT INTO notes VALUES ('b1')")
                .await
                .unwrap();
            tick(&b.replication).await;
            drop(db_b);
            b.fleet.close_all().await;
            assert!(b.fleet.release_ownership().await);

            // A gets the range back. Its file on disk is older than B's
            // generation, so it serves B's data, not its own.
            assert_eq!(
                a.ownership.refresh().await.claimed,
                vec![(SlotRange::ALL, token(3))]
            );
            let db_a = a.fleet.open(&key).await.unwrap();
            assert_eq!(bodies(&db_a).await, vec!["a1", "b1"]);
            exec(&db_a, "INSERT INTO notes VALUES ('a3')")
                .await
                .unwrap();
            drop(db_a);
            a.fleet.close(&key).await;
            // Under the same epoch, its own file is current.
            let db_a = a.fleet.open(&key).await.unwrap();
            assert_eq!(bodies(&db_a).await, vec!["a1", "b1", "a3"]);
            drop(db_a);
            a.fleet.close_all().await;
        }

        #[tokio::test]
        async fn a_new_database_is_not_restored_over_its_unshipped_writes() {
            let dir = tempfile::tempdir().unwrap();
            let clock = clock_at(1_000_000);
            // A web and a worker process of owner "a" on one volume.
            let web = host_on(dir.path(), "vol", "a", &clock, vec![SlotRange::ALL]).await;
            let worker = host_on(dir.path(), "vol", "a", &clock, vec![SlotRange::ALL]).await;
            let key = acme(&web.fleet);
            let db = web.fleet.open(&key).await.unwrap();
            exec(&db, "CREATE TABLE notes (body TEXT NOT NULL)")
                .await
                .unwrap();
            exec(&db, "INSERT INTO notes VALUES ('shipped')")
                .await
                .unwrap();
            tick(&web.replication).await;
            exec(&db, "INSERT INTO notes VALUES ('not shipped yet')")
                .await
                .unwrap();
            // The replica holds this epoch's generation, not a newer owner's.
            let other = worker.fleet.open(&key).await.unwrap();
            assert_eq!(bodies(&other).await, vec!["shipped", "not shipped yet"]);
            drop((db, other));
            web.fleet.close_all().await;
            worker.fleet.close_all().await;
        }

        #[tokio::test]
        async fn a_fenced_replicator_stops_and_restore_takes_the_newest_epoch() {
            let dir = tempfile::tempdir().unwrap();
            let clock = clock_at(1_000_000);
            let a = host(dir.path(), "a", &clock, vec![SlotRange::ALL]).await;
            let key = acme(&a.fleet);
            let db_a = a.fleet.open(&key).await.unwrap();
            exec(&db_a, "CREATE TABLE notes (body TEXT NOT NULL)")
                .await
                .unwrap();
            exec(&db_a, "INSERT INTO notes VALUES ('a1')")
                .await
                .unwrap();
            tick(&a.replication).await;

            // A is partitioned from the store but not from the replicas.
            // B claims after the lapse and takes over from the replica.
            clock.advance(TTL);
            let b = host(dir.path(), "b", &clock, vec![SlotRange::ALL]).await;
            let db_b = b.fleet.open(&key).await.unwrap();
            assert_eq!(db_b.epoch(), Some(token(2)));
            assert_eq!(bodies(&db_b).await, vec!["a1"]);
            exec(&db_b, "INSERT INTO notes VALUES ('b1')")
                .await
                .unwrap();
            tick(&b.replication).await;

            // A still writes and ships, after B: a divergent copy, in A's
            // generation of epoch 1.
            exec(&db_a, "INSERT INTO notes VALUES ('a2 stale')")
                .await
                .unwrap();
            tick(&a.replication).await;
            // Its next fence check sees epoch 2 and stops.
            exec(&db_a, "INSERT INTO notes VALUES ('a3 stale')")
                .await
                .unwrap();
            a.clock.advance(Duration::from_secs(120));
            tick(&a.replication).await;
            assert_eq!(a.ownership.epoch_for(key.routing_slot()), None);
            assert_eq!(a.fleet.fence_unowned(), 1);
            drop(db_a);
            a.fleet.close(&key).await;

            // A third host restores B's generation, not A's newer one.
            drop(db_b);
            b.fleet.close(&key).await;
            b.ownership.release_all().await;
            let c = host(dir.path(), "c", &clock, vec![SlotRange::ALL]).await;
            let db_c = c.fleet.open(&key).await.unwrap();
            assert_eq!(db_c.epoch(), Some(token(3)));
            assert_eq!(bodies(&db_c).await, vec!["a1", "b1"]);
            c.fleet.close_all().await;
        }
    }

    mod props {
        use super::*;
        use proptest::prelude::*;

        #[derive(Debug, Clone)]
        enum Op {
            Claim(u8, u16, u16, i64),
            Renew(u8, u16, u16, i64),
            Release(u8, u16, u16, i64),
            /// A renew with any epoch, stale ones included.
            RenewAt(u8, u16, u16, i64, u64),
        }

        fn op() -> impl Strategy<Value = Op> {
            let r = (0_u16..64, 0_u16..64).prop_map(|(a, b)| (a.min(b), a.max(b)));
            prop_oneof![
                (0_u8..3, r.clone(), 0_i64..200).prop_map(|(o, (a, b), t)| Op::Claim(o, a, b, t)),
                (0_u8..3, r.clone(), 0_i64..200).prop_map(|(o, (a, b), t)| Op::Renew(o, a, b, t)),
                (0_u8..3, r.clone(), 0_i64..200).prop_map(|(o, (a, b), t)| Op::Release(o, a, b, t)),
                (0_u8..3, r, 0_i64..200, 1_u64..8)
                    .prop_map(|(o, (a, b), t, e)| Op::RenewAt(o, a, b, t, e)),
            ]
        }

        proptest! {
            /// The invariants of `verification/fleet_slot_ownership.rs`, on
            /// the runtime table: rows never overlap, epochs are unique and
            /// at most the counter, the counter and every slot's epoch never
            /// go down, and a live lease of another owner is never taken.
            #[test]
            fn the_table_keeps_its_invariants(ops in proptest::collection::vec(op(), 1..60)) {
                let owners = [owner("a"), owner("b"), owner("c")];
                let ttl = Duration::from_millis(50);
                let mut table = OwnershipTable::new();
                let mut clock = 0_i64;
                for op in ops {
                    let before = table.clone();
                    match op {
                        Op::Claim(o, a, b, dt) => {
                            clock += dt;
                            let who = &owners[usize::from(o)];
                            let result = table.claim(range(a, b), who, clock, ttl);
                            let blocked = before.rows().iter().any(|row| {
                                row.range.overlaps(range(a, b))
                                    && !(row.owner == *who && row.range == range(a, b))
                                    && row.lease_expires_ms > clock
                            });
                            prop_assert_eq!(result.is_err(), blocked);
                            if let Ok(grant) = result
                                && grant.fresh
                            {
                                prop_assert_eq!(grant.epoch.get(), before.last_epoch() + 1);
                            }
                        }
                        Op::Renew(o, a, b, dt) => {
                            clock += dt;
                            let who = &owners[usize::from(o)];
                            if let Some(row) = before.rows().iter().find(|r| r.range == range(a, b)) {
                                let ok = table.renew(range(a, b), who, row.epoch, clock, ttl).is_ok();
                                prop_assert_eq!(ok, row.owner == *who);
                            }
                        }
                        Op::RenewAt(o, a, b, dt, e) => {
                            clock += dt;
                            let who = &owners[usize::from(o)];
                            let ok = table.renew(range(a, b), who, token(e), clock, ttl).is_ok();
                            let holds = before.rows().iter().any(|row| {
                                row.range == range(a, b) && row.owner == *who && row.epoch == token(e)
                            });
                            prop_assert_eq!(ok, holds, "a stale epoch never renews");
                        }
                        Op::Release(o, a, b, dt) => {
                            clock += dt;
                            let who = &owners[usize::from(o)];
                            if let Some(row) = before.rows().iter().find(|r| r.range == range(a, b)) {
                                let released = table.release(range(a, b), who, row.epoch, clock);
                                prop_assert_eq!(released, row.owner == *who);
                            }
                        }
                    }
                    // Rows never overlap.
                    for (i, x) in table.rows().iter().enumerate() {
                        for y in &table.rows()[i + 1..] {
                            prop_assert!(!x.range.overlaps(y.range));
                            prop_assert_ne!(x.epoch, y.epoch);
                        }
                        prop_assert!(x.epoch.get() <= table.last_epoch());
                    }
                    prop_assert!(table.last_epoch() >= before.last_epoch());
                    // A slot's epoch never goes down, and a stale epoch is
                    // never admitted.
                    for slot in 0..64_u16 {
                        if let (Some(old), Some(new)) = (before.row_for(slot), table.row_for(slot)) {
                            prop_assert!(new.epoch >= old.epoch);
                            if new.epoch > old.epoch {
                                prop_assert!(!table.admits(slot, &old.owner, old.epoch));
                            }
                        }
                    }
                }
            }
        }
    }
}
