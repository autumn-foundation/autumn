//! The replication loop: the in-process worker that ships `SQLite`'s WAL to an
//! offsite destination, continuously, while the app serves traffic (#1628).
//!
//! # The invariant everything else hangs off
//!
//! In WAL journal mode `SQLite` **never writes the main database file except
//! during a checkpoint**. Every committed page goes to the `-wal` sidecar until
//! someone checkpoints it back. So if exactly one component is allowed to
//! checkpoint, that component can also take a byte-faithful copy of the main
//! database file at any other moment and know it is a consistent base for the
//! WAL frames that follow it.
//!
//! That component is this one. Autumn's `SQLite` pool sets
//! `PRAGMA wal_autocheckpoint = 0` when replication is enabled, and the
//! replicator holds one connection open for the process's lifetime — which also
//! stops `SQLite`'s *last connection closing* from checkpointing behind our back.
//!
//! # The tick
//!
//! ```text
//! read the -wal header
//!   no generation yet, or the salt changed unexpectedly, or the WAL shrank
//!       → open a new generation: copy + gzip the database file, upload it,
//!         then write snapshot.json as the commit marker
//! scan the frame chain from the last shipped commit
//!   → ship [shipped_offset, last commit boundary) as one segment
//! everything shipped, and the WAL is over its size budget
//!   → checkpoint(TRUNCATE), then either open the next WAL *index* inside this
//!     generation (cheap) or, once the generation is older than the snapshot
//!     interval, start a new generation with a fresh base snapshot
//! ```
//!
//! The index/generation split is what keeps this affordable. A checkpoint is
//! unavoidable — the `-wal` cannot grow without bound — but treating every
//! checkpoint as a new generation would re-upload the entire database each time
//! `max_wal_bytes` is reached, which on a busy database is gigabytes of write
//! amplification per hour. A checkpoint therefore costs one index bump; a full
//! base snapshot happens on the snapshot interval, which is also what bounds how
//! much WAL a restore has to replay.
//!
//! Two orderings carry the whole durability story:
//!
//! * a segment ends on a **commit boundary**, so a replica is never a
//!   half-transaction;
//! * a checkpoint is attempted **only when nothing is un-shipped**, so
//!   truncating the WAL can never discard bytes the destination has not
//!   acknowledged. A destination that is down therefore stalls checkpoints — the
//!   WAL grows, lag climbs, the health indicator goes `Down`, and data is kept.
//!   Disk is the thing that gets sacrificed, never durability.

// autumn-panic-gate: durability-critical module — production code path must be
// panic-free. See CONTRIBUTING.md "Request-path panic gate". Justify exceptions
// with #[allow(clippy::<lint>, reason = "…")] at the narrowest scope.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
        clippy::string_slice,
        clippy::arithmetic_side_effects,
    )
)]

use std::collections::BTreeMap;
use std::fmt;
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use diesel::SqliteConnection;
use diesel::connection::SimpleConnection as _;
use diesel::prelude::*;
use diesel::sql_types::Text;

use super::destination::{DestinationError, ReplicaDestination};
use super::restore::{self, RestoreError, RestoreOutcome};
use super::segment::{self, SegmentError, SegmentHeader, SnapshotMeta};
use super::sqlite::{self, CheckpointOutcome, SqliteError};
use super::status::ReplicationStatus;
use super::wal::{self, ScanCursor, WalError, WalHeader};
use crate::time::{ClockSource, SystemClock};

/// How long the loop sleeps between shutdown checks while waiting for the next
/// tick. Short enough that shutdown is prompt, long enough to be free.
const SHUTDOWN_POLL: Duration = Duration::from_millis(100);

/// Complete generations the retention sweep always keeps, whatever the clock
/// says. A host whose clock steps forward past the retention window would
/// otherwise expire the entire history in one sweep.
const MIN_KEPT_GENERATIONS: usize = 2;

/// Everything the replication loop needs, already resolved from config.
#[derive(Debug, Clone)]
pub struct ReplicationSettings {
    /// The `SQLite` database file being replicated.
    pub database_path: PathBuf,
    /// Destination key prefix for this app/profile (see
    /// [`segment::root_prefix`]).
    pub root: String,
    /// How often the loop ships. The effective RPO is roughly this plus the
    /// time one upload takes.
    pub sync_interval: Duration,
    /// How old a generation may get before the next checkpoint starts a fresh
    /// one with a new base snapshot, bounding how much WAL a restore replays.
    pub snapshot_interval: Duration,
    /// How large the `-wal` may get before the loop checkpoints it and opens the
    /// next WAL index inside the current generation.
    pub max_wal_bytes: u64,
    /// How far back the replica must stay restorable. Older generations are
    /// pruned, never the one needed to reconstruct the start of the window.
    pub retention: Duration,
    /// How often to prove the replica restorable by actually restoring it.
    /// `None` disables periodic verification.
    pub verify_interval: Option<Duration>,
}

/// Why a replication tick failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum ReplicationError {
    /// The configured database file does not exist.
    DatabaseMissing {
        /// The path that was expected.
        path: String,
    },
    /// The database is not in WAL journal mode, so there is nothing to ship.
    NotWalMode {
        /// The journal mode `SQLite` reported.
        mode: String,
    },
    /// A generation could not be opened because the checkpoint it needs first
    /// was blocked by another connection. Transient: the next tick retries.
    CheckpointBlocked {
        /// Bytes still in the `-wal` file.
        wal_bytes: u64,
    },
    /// The `-wal` file could not be read as a WAL.
    Wal(WalError),
    /// A segment could not be framed.
    Segment(SegmentError),
    /// The destination refused or failed.
    Destination(DestinationError),
    /// Local I/O failed.
    Io {
        /// What was being attempted.
        op: &'static str,
        /// I/O detail.
        detail: String,
    },
    /// `SQLite` refused an operation.
    Sqlite(SqliteError),
}

impl fmt::Display for ReplicationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DatabaseMissing { path } => write!(
                f,
                "the database file {path} does not exist, so there is nothing to replicate"
            ),
            Self::NotWalMode { mode } => write!(
                f,
                "continuous replication needs WAL journal mode, but the database reports \
                 {mode:?}. Autumn's SQLite pool sets `journal_mode = WAL`; a read-only or \
                 in-memory database cannot be replicated."
            ),
            Self::CheckpointBlocked { wal_bytes } => write!(
                f,
                "could not open a replication generation: the checkpoint it needs first was \
                 blocked by another connection ({wal_bytes} byte(s) still in the -wal). \
                 This is transient; the next tick retries."
            ),
            Self::Wal(e) => write!(f, "{e}"),
            Self::Segment(e) => write!(f, "{e}"),
            Self::Destination(e) => write!(f, "{e}"),
            Self::Io { op, detail } => write!(f, "replication {op} failed: {detail}"),
            Self::Sqlite(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ReplicationError {}

impl From<WalError> for ReplicationError {
    fn from(e: WalError) -> Self {
        Self::Wal(e)
    }
}
impl From<SegmentError> for ReplicationError {
    fn from(e: SegmentError) -> Self {
        Self::Segment(e)
    }
}
impl From<DestinationError> for ReplicationError {
    fn from(e: DestinationError) -> Self {
        Self::Destination(e)
    }
}
impl From<SqliteError> for ReplicationError {
    fn from(e: SqliteError) -> Self {
        Self::Sqlite(e)
    }
}

impl ReplicationError {
    fn io(op: &'static str) -> impl Fn(std::io::Error) -> Self {
        move |e| Self::Io {
            op,
            detail: e.to_string(),
        }
    }
}

/// What one tick did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TickReport {
    /// A new generation was opened (a base snapshot was uploaded).
    pub snapshot_taken: bool,
    /// Segments shipped this tick (at most one).
    pub segments: u64,
    /// Uncompressed WAL bytes shipped this tick.
    pub bytes: u64,
    /// WAL bytes written but not yet shipped after this tick.
    pub pending_bytes: u64,
    /// The WAL was checkpointed and truncated this tick.
    pub checkpointed: bool,
    /// The checkpoint opened the next WAL index inside the same generation
    /// (rather than forcing a fresh base snapshot).
    pub index_rotated: bool,
    /// Generations pruned by the retention sweep this tick.
    pub pruned_generations: u64,
}

/// Live state for the generation currently being shipped.
#[derive(Debug)]
struct GenerationState {
    /// Generation id (also its key prefix).
    id: String,
    /// WAL index inside this generation. `0` at the base snapshot; bumped by
    /// each of the replicator's own checkpoints.
    index: u32,
    /// The WAL salt this generation's segments must all carry. `None` until the
    /// first write after the generation opened reveals it.
    salt: Option<(u32, u32)>,
    /// Database page size, from the WAL header.
    page_size: u32,
    /// When the generation opened.
    started_at: DateTime<Utc>,
    /// Sequence number for the next segment.
    next_seq: u64,
    /// `-wal` bytes already shipped. Starts at `0`, so segment `0` carries the
    /// 32-byte WAL header and a restore can rebuild the sidecar from objects
    /// alone.
    shipped_offset: u64,
    /// Where the frame scan resumes, with its rolling checksum. `None` until a
    /// WAL header has been seen.
    cursor: Option<ScanCursor>,
}

impl GenerationState {
    /// Continue this generation after the replicator's own checkpoint.
    ///
    /// The checkpoint folded every shipped frame into the database file and
    /// restarted the WAL at offset `0` under a new salt, so no fresh base
    /// snapshot is needed: a restore replays index by index, checkpointing each
    /// one in before the next is applied.
    const fn begin_next_index(&mut self) {
        self.index = self.index.saturating_add(1);
        self.salt = None;
        self.page_size = 0;
        self.next_seq = 0;
        self.shipped_offset = 0;
        self.cursor = None;
    }
}

/// Row shape for `PRAGMA journal_mode`.
#[derive(QueryableByName)]
struct JournalModeRow {
    #[diesel(sql_type = Text)]
    journal_mode: String,
}

/// The continuous replicator.
///
/// Drive it with [`Replicator::run`] on a dedicated thread, or step it manually
/// with [`Replicator::tick`] (which is what the tests do, so the loop's
/// behaviour is asserted without sleeping).
pub struct Replicator {
    settings: ReplicationSettings,
    destination: Arc<dyn ReplicaDestination>,
    status: Arc<ReplicationStatus>,
    /// Held open for the process's lifetime so `SQLite` never runs its
    /// "last connection closing" checkpoint behind the replicator's back.
    connection: Option<SqliteConnection>,
    state: Option<GenerationState>,
    /// Set while a verification restore is running on its own thread, so a slow
    /// verification cannot pile up behind itself.
    verifying: Arc<AtomicBool>,
    /// The injected wall clock. Every artifact timestamp is read from here at
    /// the moment that artifact's contents are fenced, never earlier.
    clock: Arc<dyn ClockSource>,
}

impl fmt::Debug for Replicator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Replicator")
            .field("settings", &self.settings)
            .field("destination", &self.destination.describe())
            .field("generation", &self.state.as_ref().map(|s| &s.id))
            .finish_non_exhaustive()
    }
}

impl Replicator {
    /// Build a replicator. Nothing happens until [`tick`](Self::tick) or
    /// [`run`](Self::run) is called.
    #[must_use]
    pub fn new(
        settings: ReplicationSettings,
        destination: Arc<dyn ReplicaDestination>,
        status: Arc<ReplicationStatus>,
    ) -> Self {
        Self {
            settings,
            destination,
            status,
            connection: None,
            state: None,
            verifying: Arc::new(AtomicBool::new(false)),
            clock: Arc::new(SystemClock),
        }
    }

    /// Read time from `clock` instead of the system clock.
    ///
    /// The app wires in its injected clock, and a test wires in one it can step,
    /// so a point-in-time restore can be exercised over a compressed timeline.
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn ClockSource>) -> Self {
        self.clock = clock;
        self
    }

    /// The destination this replicator ships to.
    #[must_use]
    pub fn destination(&self) -> &Arc<dyn ReplicaDestination> {
        &self.destination
    }

    /// Run until `shutdown` is cancelled, then ship one final time so a clean
    /// stop leaves nothing behind.
    ///
    /// Blocking: call this on a dedicated thread, never on the async runtime.
    pub fn run(mut self, shutdown: &tokio_util::sync::CancellationToken) {
        let mut last_verify = self.clock.now();
        loop {
            let now = self.clock.now();
            self.tick_and_record();

            if let Some(interval) = self.settings.verify_interval {
                let due = now
                    .signed_duration_since(last_verify)
                    .to_std()
                    .is_ok_and(|elapsed| elapsed >= interval);
                if due {
                    last_verify = now;
                    self.spawn_verification();
                }
            }

            if sleep_or_cancelled(shutdown, self.settings.sync_interval) {
                break;
            }
        }
        // Final flush: the app is stopping, so ship the last committed frames
        // before the process (and possibly the machine) goes away.
        self.tick_and_record();
    }

    /// Run one tick, folding the outcome into the shared status.
    fn tick_and_record(&mut self) {
        match self.tick() {
            Ok(report) => {
                if report.segments > 0 || report.snapshot_taken {
                    tracing::debug!(
                        segments = report.segments,
                        bytes = report.bytes,
                        snapshot = report.snapshot_taken,
                        checkpointed = report.checkpointed,
                        "SQLite replication tick"
                    );
                }
            }
            // `tick` has already folded the failure into the shared status, so
            // lag keeps growing; this arm only surfaces it in the log.
            Err(e) => tracing::warn!(error = %e, "SQLite replication tick failed"),
        }
    }

    /// Perform one replication tick.
    ///
    /// Whatever the outcome, it is recorded in the shared [`ReplicationStatus`]
    /// before returning — success advances the replication point, failure does
    /// not — so every caller (the loop, a test, a future operator command) sees
    /// the same observable state.
    ///
    /// # Errors
    ///
    /// See [`ReplicationError`]. A failed tick never advances the replication
    /// point, so lag keeps growing until shipping works again.
    pub fn tick(&mut self) -> Result<TickReport, ReplicationError> {
        let result = self.tick_inner();
        if let Err(e) = &result {
            self.status
                .record_tick_error(e.to_string(), self.clock.now());
        }
        result
    }

    /// The tick body. Separated so [`tick`](Self::tick) can record the outcome
    /// on every path without threading the status through each `?`.
    fn tick_inner(&mut self) -> Result<TickReport, ReplicationError> {
        let db = self.settings.database_path.clone();
        if !db.is_file() {
            return Err(ReplicationError::DatabaseMissing {
                path: db.display().to_string(),
            });
        }
        self.ensure_connection(&db)?;

        let wal_path = wal::wal_path(&db);
        let wal_len = std::fs::metadata(&wal_path).map_or(0, |m| m.len());
        let header = read_wal_header(&wal_path, wal_len)?;

        let mut report = TickReport::default();
        let mut header = header;
        let mut wal_len = wal_len;
        if self.needs_new_generation(header.as_ref(), wal_len) {
            self.start_generation(&db)?;
            report.snapshot_taken = true;
            // The generation opened from a checkpointed database file, so the
            // WAL was reset underneath us; re-read it before shipping.
            wal_len = std::fs::metadata(&wal_path).map_or(0, |m| m.len());
            header = read_wal_header(&wal_path, wal_len)?;
        }

        if let Some(header) = header {
            let shipped = self.ship(&wal_path, &header, wal_len)?;
            report.segments = shipped.0;
            report.bytes = shipped.1;
        }

        let pending = self
            .state
            .as_ref()
            .map_or(0, |st| wal_len.saturating_sub(st.shipped_offset));
        report.pending_bytes = pending;

        // Read *after* anything this tick may have opened or shipped, never
        // before. Both judgements below compare this instant against a timestamp
        // the tick itself may have just written — a generation's `started_at`,
        // an artifact's creation instant — and each of those is sampled at the
        // moment its contents were fenced, which is later than the top of the
        // tick. A reading taken up there would sit *behind* them, and
        // `generation_expired` reads a negative span as "expired": every fresh
        // generation would be retired on the tick that opened it.
        let now = self.clock.now();
        let mut index_rotated = false;
        report.checkpointed =
            self.maybe_checkpoint(&db, &wal_path, pending, now, &mut index_rotated)?;
        report.index_rotated = index_rotated;

        // Retention runs every tick, not only when a generation opens: on a
        // quiet database a generation can outlive many ticks, and a destination
        // that grows without bound is its own outage. A prune failure (a revoked
        // DeleteObject, a throttle) must not fail the tick — the data is still
        // safely offsite — so it is logged and retried next time.
        match self.prune(now) {
            Ok(pruned) => report.pruned_generations = pruned,
            Err(e) => tracing::warn!(error = %e, "SQLite replica retention sweep failed"),
        }

        self.status.record_tick_ok(pending, self.clock.now());
        Ok(report)
    }

    /// Checkpoint the WAL when it is safe and due, returning whether it happened.
    ///
    /// # The interlock
    ///
    /// A checkpoint folds the WAL into the database file and truncates it, so
    /// anything in the WAL that has not been shipped is gone from every artifact
    /// the replica has. `pending == 0` is computed from a `wal_len` measured
    /// *before* the tick's upload, which can take seconds — so by itself it
    /// proves nothing. Two more things make this safe:
    ///
    /// * the `-wal` is re-measured here, immediately before the checkpoint, and
    ///   must still be exactly what was shipped;
    /// * `PRAGMA data_version` brackets the checkpoint. It moves if — and only
    ///   if — another connection committed, so an unchanged value proves no
    ///   write slipped into the gap between that measurement and the checkpoint
    ///   taking `SQLite`'s write lock.
    ///
    /// When the bracket *does* move, the checkpoint may have folded away a
    /// transaction that was never shipped. That is not treated as loss: the
    /// generation is retired instead, so the next tick takes a fresh base
    /// snapshot of the database file the checkpoint just completed — which
    /// contains that transaction. The cost is one snapshot; the alternative
    /// would be silent data loss.
    fn maybe_checkpoint(
        &mut self,
        db: &Path,
        wal_path: &Path,
        pending: u64,
        now: DateTime<Utc>,
        index_rotated: &mut bool,
    ) -> Result<bool, ReplicationError> {
        let expired = self.generation_expired(now);
        let over_budget = self
            .state
            .as_ref()
            .is_some_and(|state| state.shipped_offset >= self.settings.max_wal_bytes);
        if pending != 0 || !(over_budget || expired) {
            return Ok(false);
        }
        let Some(shipped_offset) = self.state.as_ref().map(|state| state.shipped_offset) else {
            return Ok(false);
        };
        if shipped_offset == 0 {
            // Nothing in this index yet; an expired generation with an empty WAL
            // needs no checkpoint at all, just a fresh snapshot next tick.
            if expired {
                self.state = None;
            }
            return Ok(false);
        }

        let Some(conn) = self.connection.as_mut() else {
            return Ok(false);
        };
        let before = sqlite::data_version(conn)?;
        // Re-measure: the WAL may have grown while this tick was uploading.
        if std::fs::metadata(wal_path).map_or(0, |m| m.len()) != shipped_offset {
            return Ok(false);
        }
        if sqlite::checkpoint_truncate(conn, db)? != CheckpointOutcome::Truncated {
            return Ok(false);
        }
        let raced = sqlite::data_version(conn)? != before;

        if expired || raced {
            if raced {
                tracing::info!(
                    "a write landed while the SQLite WAL was being checkpointed; taking a \
                     fresh base snapshot rather than assuming it was replicated"
                );
            }
            self.state = None;
        } else if let Some(state) = self.state.as_mut() {
            // Cheap path: same base snapshot, next WAL index.
            state.begin_next_index();
            *index_rotated = true;
        }
        Ok(true)
    }

    /// Restore this replica into a scratch directory and integrity-check it, so
    /// "verified" means a restore actually ran (phase 3).
    ///
    /// # Errors
    ///
    /// See [`RestoreError`].
    pub fn verify(&self) -> Result<RestoreOutcome, RestoreError> {
        verify_replica(
            self.destination.as_ref(),
            &self.settings.root,
            &self.settings.database_path,
        )
    }

    /// Run a verification on its own thread.
    ///
    /// Verification restores the entire replica, which on a large database takes
    /// far longer than a tick. Running it inline would stall shipping for that
    /// whole time and blow the RPO the rest of this module exists to hold, so it
    /// gets its own thread — and a flag, so a verification slower than the
    /// verification interval cannot pile up behind itself.
    fn spawn_verification(&self) {
        if self.verifying.swap(true, Ordering::SeqCst) {
            tracing::warn!(
                "skipping a SQLite replica verification: the previous one is still running"
            );
            return;
        }
        let destination = Arc::clone(&self.destination);
        let status = Arc::clone(&self.status);
        let verifying = Arc::clone(&self.verifying);
        let root = self.settings.root.clone();
        let database_path = self.settings.database_path.clone();
        let clock = Arc::clone(&self.clock);
        let spawned = std::thread::Builder::new()
            .name("autumn-replica-verify".to_owned())
            .spawn(move || {
                // Cleared on drop, so a panic anywhere in the restore path
                // cannot leave the flag set and silently disable every future
                // verification.
                let _guard = InFlightGuard(verifying);
                let result = verify_replica(destination.as_ref(), &root, &database_path)
                    .map(|_| ())
                    .map_err(|e| e.to_string());
                if let Err(detail) = &result {
                    tracing::error!(
                        error = %detail,
                        "SQLite replica verification FAILED — the replica may not be restorable"
                    );
                }
                status.record_verification(result, clock.now());
            });
        if let Err(e) = spawned {
            tracing::warn!("could not start the SQLite replica verification thread: {e}");
            self.verifying.store(false, Ordering::SeqCst);
        }
    }

    /// Open (once) a private connection and prove the database is in WAL mode.
    fn ensure_connection(&mut self, db: &Path) -> Result<(), ReplicationError> {
        if self.connection.is_some() {
            return Ok(());
        }
        let mut conn = sqlite::open(db)?;
        let mode = journal_mode(&mut conn)?;
        if !mode.eq_ignore_ascii_case("wal") {
            // The app's pool sets WAL; do it here too so a replicator started
            // against a freshly created file does not have to wait for a write.
            conn.batch_execute("PRAGMA journal_mode = WAL;")
                .map_err(|e| {
                    ReplicationError::Sqlite(SqliteError::Query {
                        op: "journal_mode = WAL",
                        detail: e.to_string(),
                    })
                })?;
            let mode = journal_mode(&mut conn)?;
            if !mode.eq_ignore_ascii_case("wal") {
                return Err(ReplicationError::NotWalMode { mode });
            }
        }
        self.connection = Some(conn);
        Ok(())
    }

    /// Whether a fresh base snapshot is required before anything can be shipped.
    fn needs_new_generation(&self, header: Option<&WalHeader>, wal_len: u64) -> bool {
        match (&self.state, header) {
            // Nothing has been replicated yet.
            (None, _) => true,
            // The WAL restarted under a new salt: previously shipped byte
            // offsets no longer mean anything.
            (Some(state), Some(header)) => {
                state.salt.is_some_and(|salt| salt != header.salt())
                    || wal_len < state.shipped_offset
            }
            // The WAL vanished or shrank below what we had shipped — a
            // checkpoint happened (ours, on the previous tick, or someone
            // else's).
            (Some(state), None) => state.shipped_offset > 0 || wal_len < state.shipped_offset,
        }
    }

    /// Open a new generation: checkpoint, then upload a byte-faithful gzip of
    /// the database file, then its `snapshot.json` commit marker.
    ///
    /// The checkpoint comes first and is not optional. In WAL mode the main
    /// database file holds the state as of the *last* checkpoint, so snapshotting
    /// it while the `-wal` still carries committed frames would publish a base
    /// that is silently behind — and `snapshot.json` makes that generation the
    /// newest complete one, so a restore would then regress past everything the
    /// previous generation had already shipped. Checkpointing first makes the
    /// database file current by construction, and the new WAL starts empty at
    /// offset `0`, which is exactly where this generation's index `0` begins.
    fn start_generation(&mut self, db: &Path) -> Result<(), ReplicationError> {
        if let Some(conn) = self.connection.as_mut() {
            let outcome = sqlite::checkpoint_truncate(conn, db)?;
            if let CheckpointOutcome::Busy { wal_bytes } = outcome {
                return Err(ReplicationError::CheckpointBlocked { wal_bytes });
            }
        }
        // Stamped only now: the checkpoint is what fences the database file this
        // snapshot copies. A reading taken before it would claim the snapshot
        // holds a moment it does not yet cover, and a point-in-time restore
        // landing in that window would replay a transaction from *after* the
        // instant the operator asked for. Reading after the fence can only
        // over-state the snapshot's age, which costs the restore an extra
        // generation of WAL replay and never correctness.
        let now = self.clock.now();
        let created_ms = now.timestamp_millis();
        let id = segment::generation_id(created_ms, rand::random::<u64>());

        let staging = staging_path(db);
        let (sha256, uncompressed_len) = gzip_file(db, &staging)?;
        let upload = self
            .destination
            .put_file(&segment::snapshot_key(&self.settings.root, &id), &staging);
        let _ = std::fs::remove_file(&staging);
        upload?;

        let meta = SnapshotMeta {
            version: segment::LAYOUT_VERSION,
            generation: id.clone(),
            created_at: now.to_rfc3339(),
            created_ms,
            sha256,
            uncompressed_len,
        };
        let body = serde_json::to_vec(&meta).map_err(|e| ReplicationError::Io {
            op: "encode snapshot metadata",
            detail: e.to_string(),
        })?;
        // Written LAST: its presence is what makes the generation restorable.
        self.destination
            .put(&segment::snapshot_meta_key(&self.settings.root, &id), &body)?;

        tracing::info!(
            generation = %id,
            bytes = uncompressed_len,
            destination = %self.destination.describe(),
            "opened a new SQLite replication generation"
        );
        self.status.record_generation(&id, now);
        self.state = Some(GenerationState {
            id,
            index: 0,
            salt: None,
            page_size: 0,
            started_at: now,
            next_seq: 0,
            shipped_offset: 0,
            cursor: None,
        });
        Ok(())
    }

    /// Ship everything committed since the last segment. Returns
    /// `(segments, bytes)`.
    fn ship(
        &mut self,
        wal_path: &Path,
        header: &WalHeader,
        wal_len: u64,
    ) -> Result<(u64, u64), ReplicationError> {
        let root = self.settings.root.clone();
        let clock = Arc::clone(&self.clock);
        let Some(state) = self.state.as_mut() else {
            return Ok((0, 0));
        };
        if state.salt.is_none() {
            state.salt = Some(header.salt());
            state.page_size = header.page_size;
            state.cursor = Some(ScanCursor::start(header));
        }
        if state.salt != Some(header.salt()) {
            // The salt changed mid-tick; the next tick opens a new generation.
            return Ok((0, 0));
        }
        let Some(cursor) = state.cursor else {
            return Ok((0, 0));
        };
        if wal_len <= cursor.offset {
            return Ok((0, 0));
        }

        let buffer = read_range(wal_path, state.shipped_offset, wal_len)?;
        let scan_at = usize::try_from(cursor.offset.saturating_sub(state.shipped_offset))
            .unwrap_or(usize::MAX);
        let outcome = wal::scan_from(header, &cursor, buffer.get(scan_at..).unwrap_or(&[]))?;
        if outcome.last_commit_end <= cursor.offset {
            // Frames are present but no transaction has committed yet — shipping
            // them would put half a transaction offsite.
            return Ok((0, 0));
        }

        let take = usize::try_from(outcome.last_commit_end.saturating_sub(state.shipped_offset))
            .unwrap_or(usize::MAX);
        let raw = buffer.get(..take).ok_or_else(|| ReplicationError::Io {
            op: "slice the WAL range",
            detail: format!(
                "the -wal shrank while it was being read (wanted {take} bytes, have {})",
                buffer.len()
            ),
        })?;

        // Stamped only now, once the bytes this segment carries have been read
        // out of the `-wal` and sliced at their commit boundary. `wal_len` is
        // measured at the top of the tick and the read follows it, so a
        // transaction can commit into that window and land in `raw`; a timestamp
        // sampled before the read would date the segment earlier than a change
        // it actually contains, and a restore to an instant in between would
        // replay that change. Sampling after can only date the segment late,
        // which excludes it from such a restore instead.
        let now = clock.now();
        let segment_header = SegmentHeader {
            version: segment::LAYOUT_VERSION,
            index: state.index,
            seq: state.next_seq,
            start_offset: state.shipped_offset,
            end_offset: outcome.last_commit_end,
            frame_count: outcome.frames,
            commit_count: outcome.commits,
            page_size: header.page_size,
            db_size_pages: outcome.last_commit_db_pages,
            salt1: header.salt1,
            salt2: header.salt2,
            created_at: now.to_rfc3339(),
            created_ms: now.timestamp_millis(),
            sha256: segment::sha256_hex(raw),
            uncompressed_len: raw.len() as u64,
        };
        let payload = segment::encode_segment(&segment_header, raw)?;
        let key = segment::segment_key(
            &root,
            &state.id,
            state.index,
            state.next_seq,
            segment_header.created_ms,
        );
        self.destination.put(&key, &payload)?;

        let bytes = raw.len() as u64;
        state.shipped_offset = outcome.last_commit_end;
        state.cursor = Some(outcome.last_commit_cursor);
        state.next_seq = state.next_seq.saturating_add(1);
        self.status.record_segment(bytes, now);
        Ok((1, bytes))
    }

    /// Whether the current generation has outlived the snapshot interval, so the
    /// next checkpoint should start a new one with a fresh base snapshot rather
    /// than another index.
    fn generation_expired(&self, now: DateTime<Utc>) -> bool {
        self.state.as_ref().is_some_and(|state| {
            let elapsed = now.signed_duration_since(state.started_at);
            // A clock that stepped backwards leaves a negative span. Treat that
            // as "expired" rather than "forever young", so a bad clock cannot
            // pin one generation open indefinitely.
            elapsed
                .to_std()
                .map_or(true, |age| age >= self.settings.snapshot_interval)
        })
    }

    /// Drop generations that are entirely outside the retention window.
    ///
    /// The newest generation that opened at or before the cutoff is **kept**:
    /// without it the start of the window is not reconstructable. Returns how
    /// many generations were removed.
    fn prune(&self, now: DateTime<Utc>) -> Result<u64, ReplicationError> {
        let Ok(retention) = chrono::Duration::from_std(self.settings.retention) else {
            return Ok(0);
        };
        let cutoff_ms = now
            .checked_sub_signed(retention)
            .unwrap_or(now)
            .timestamp_millis();
        let keys = self
            .destination
            .list(&segment::generations_prefix(&self.settings.root))?;

        let open_generation = self.state.as_ref().map(|state| state.id.clone());
        let mut generations: BTreeMap<(i64, String), Vec<String>> = BTreeMap::new();
        let mut complete: Vec<(i64, String)> = Vec::new();
        for key in keys {
            let Some(id) = segment::generation_of_key(&self.settings.root, &key) else {
                continue;
            };
            let Some(info) = segment::parse_generation_id(&id) else {
                continue;
            };
            if key.ends_with(segment::SNAPSHOT_META_OBJECT) {
                complete.push((info.created_ms, id.clone()));
            }
            generations
                .entry((info.created_ms, id))
                .or_default()
                .push(key);
        }
        complete.sort_unstable();

        // The newest complete generation at or before the cutoff is the floor of
        // the retention window; everything strictly older than it is droppable.
        let Some(floor) = complete
            .iter()
            .rev()
            .find(|(ms, _)| *ms <= cutoff_ms)
            .cloned()
        else {
            return Ok(0);
        };
        // Keep the newest few complete generations unconditionally. A clock that
        // steps forward past the retention window would otherwise expire the
        // whole history in one sweep and leave nothing to restore from.
        let keep_newest: std::collections::BTreeSet<(i64, String)> = complete
            .iter()
            .rev()
            .take(MIN_KEPT_GENERATIONS)
            .cloned()
            .collect();

        let mut pruned: u64 = 0;
        for ((ms, id), keys) in &generations {
            if (*ms, id.clone()) >= floor
                || keep_newest.contains(&(*ms, id.clone()))
                || open_generation.as_ref() == Some(id)
            {
                continue;
            }
            // Delete the commit marker FIRST: a prune interrupted halfway then
            // leaves an *uncommitted* generation, which restore ignores, rather
            // than a complete-looking generation with no segments.
            let marker = segment::snapshot_meta_key(&self.settings.root, id);
            if keys.contains(&marker) {
                self.destination.delete(&marker)?;
            }
            for key in keys {
                if *key == marker {
                    continue;
                }
                self.destination.delete(key)?;
            }
            pruned = pruned.saturating_add(1);
            tracing::debug!(generation = %id, "pruned an expired SQLite replication generation");
        }
        if pruned > MIN_KEPT_GENERATIONS as u64 {
            tracing::warn!(
                pruned,
                "the SQLite replica retention sweep removed an unusual number of generations \
                 at once — check the host clock if this was not expected"
            );
        }
        Ok(pruned)
    }
}

/// Clears the "verification in flight" flag when dropped, panic or not.
struct InFlightGuard(Arc<AtomicBool>);

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Prove a replica restorable by restoring it into a scratch directory beside
/// `database_path` — the same filesystem, so the restore's rename is cheap — and
/// integrity-checking the result. The scratch directory is removed either way.
fn verify_replica(
    destination: &dyn ReplicaDestination,
    root: &str,
    database_path: &Path,
) -> Result<RestoreOutcome, RestoreError> {
    let parent = database_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let scratch = parent.join(format!(
        ".autumn-replica-verify-{}-{:016x}",
        std::process::id(),
        rand::random::<u64>()
    ));
    let outcome = restore::restore(destination, root, None, &scratch.join("verified.db"));
    let _ = std::fs::remove_dir_all(&scratch);
    outcome
}

/// Read `PRAGMA journal_mode`.
fn journal_mode(conn: &mut SqliteConnection) -> Result<String, ReplicationError> {
    let rows: Vec<JournalModeRow> = diesel::sql_query("PRAGMA journal_mode")
        .load(conn)
        .map_err(|e| {
            ReplicationError::Sqlite(SqliteError::Query {
                op: "journal_mode",
                detail: e.to_string(),
            })
        })?;
    Ok(rows
        .first()
        .map_or_else(|| "unknown".to_owned(), |row| row.journal_mode.clone()))
}

/// Read and validate the `-wal` header, or `None` when the WAL is too short to
/// hold one (an empty WAL, right after a checkpoint).
fn read_wal_header(wal_path: &Path, wal_len: u64) -> Result<Option<WalHeader>, ReplicationError> {
    if wal_len < wal::WAL_HEADER_SIZE as u64 {
        return Ok(None);
    }
    let mut file =
        std::fs::File::open(wal_path).map_err(ReplicationError::io("open the -wal file"))?;
    let mut buf = [0u8; wal::WAL_HEADER_SIZE];
    file.read_exact(&mut buf)
        .map_err(ReplicationError::io("read the -wal header"))?;
    match WalHeader::parse(&buf) {
        Ok(header) => Ok(Some(header)),
        // A WAL being rewritten under us is transient, not fatal: skip this tick
        // rather than failing the loop.
        Err(WalError::HeaderChecksum | WalError::TooShort { .. }) => Ok(None),
        Err(e) => Err(ReplicationError::Wal(e)),
    }
}

/// Read `[start, end)` of a file into memory.
fn read_range(path: &Path, start: u64, end: u64) -> Result<Vec<u8>, ReplicationError> {
    let len = end.saturating_sub(start);
    let mut file = std::fs::File::open(path).map_err(ReplicationError::io("open the -wal file"))?;
    file.seek(SeekFrom::Start(start))
        .map_err(ReplicationError::io("seek the -wal file"))?;
    let mut buffer = Vec::new();
    file.take(len)
        .read_to_end(&mut buffer)
        .map_err(ReplicationError::io("read the -wal file"))?;
    Ok(buffer)
}

/// The staging file a base snapshot is gzip'd into, next to the database so the
/// rename-free upload stays on the same filesystem.
fn staging_path(db: &Path) -> PathBuf {
    let mut name = db.as_os_str().to_os_string();
    name.push(format!(".autumn-replica-{}.gz", std::process::id()));
    PathBuf::from(name)
}

/// Gzip `source` into `target`, returning the **uncompressed** digest and length.
///
/// Streams in fixed-size chunks so a multi-GB database is never buffered.
fn gzip_file(source: &Path, target: &Path) -> Result<(String, u64), ReplicationError> {
    use sha2::{Digest as _, Sha256};

    let mut input =
        std::fs::File::open(source).map_err(ReplicationError::io("open the database file"))?;
    let output =
        std::fs::File::create(target).map_err(ReplicationError::io("create the snapshot"))?;
    let mut encoder = flate2::write::GzEncoder::new(output, flate2::Compression::fast());
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 256 * 1024];
    let mut total: u64 = 0;
    loop {
        let read = input
            .read(&mut buffer)
            .map_err(ReplicationError::io("read the database file"))?;
        if read == 0 {
            break;
        }
        let chunk = buffer.get(..read).unwrap_or(&[]);
        hasher.update(chunk);
        encoder
            .write_all(chunk)
            .map_err(ReplicationError::io("compress the snapshot"))?;
        total = total.saturating_add(read as u64);
    }
    let file = encoder
        .finish()
        .map_err(ReplicationError::io("finish the snapshot"))?;
    file.sync_all()
        .map_err(ReplicationError::io("flush the snapshot"))?;
    Ok((hex::encode(hasher.finalize()), total))
}

/// Sleep `total`, waking early if `shutdown` is cancelled. Returns `true` when
/// cancelled.
fn sleep_or_cancelled(shutdown: &tokio_util::sync::CancellationToken, total: Duration) -> bool {
    let start = std::time::Instant::now();
    let deadline = start.checked_add(total).unwrap_or(start);
    loop {
        if shutdown.is_cancelled() {
            return true;
        }
        let now = std::time::Instant::now();
        if now >= deadline {
            return false;
        }
        std::thread::sleep(SHUTDOWN_POLL.min(deadline.saturating_duration_since(now)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staging_path_sits_next_to_the_database() {
        let path = staging_path(Path::new("/srv/app.db"));
        assert_eq!(path.parent(), Some(Path::new("/srv")));
        assert!(
            path.to_string_lossy()
                .starts_with("/srv/app.db.autumn-replica-"),
            "{}",
            path.display()
        );
    }

    #[test]
    fn gzip_file_reports_the_uncompressed_digest_and_length() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("app.db");
        let payload = b"autumn".repeat(1024);
        std::fs::write(&source, &payload).expect("write");

        let target = dir.path().join("app.db.gz");
        let (digest, len) = gzip_file(&source, &target).expect("gzip");
        assert_eq!(len, payload.len() as u64);
        assert_eq!(digest, segment::sha256_hex(&payload));

        let mut inflated = Vec::new();
        let file = std::fs::File::open(&target).expect("open gz");
        std::io::copy(&mut flate2::read::GzDecoder::new(file), &mut inflated).expect("inflate");
        assert_eq!(inflated, payload);
    }

    #[test]
    fn read_range_returns_exactly_the_requested_window() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("wal");
        std::fs::write(&path, (0u8..=255).collect::<Vec<_>>()).expect("write");
        assert_eq!(
            read_range(&path, 10, 20).expect("read"),
            (10u8..20).collect::<Vec<_>>()
        );
        // A window past EOF returns what exists rather than failing.
        assert_eq!(read_range(&path, 250, 300).expect("read").len(), 6);
    }

    #[test]
    fn sleep_or_cancelled_returns_immediately_when_already_cancelled() {
        let token = tokio_util::sync::CancellationToken::new();
        token.cancel();
        let start = std::time::Instant::now();
        assert!(sleep_or_cancelled(&token, Duration::from_secs(30)));
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn read_wal_header_treats_a_short_wal_as_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("app.db-wal");
        std::fs::write(&path, [0u8; 8]).expect("write");
        assert!(read_wal_header(&path, 8).expect("read").is_none());
    }
}
