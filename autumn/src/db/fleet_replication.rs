//! Continuous replication of every open fleet database (ADR 0019 §5).
//!
//! A cloud host attaches one volume, so the volume cannot be how a fleet
//! database survives the host or moves to another one. Object storage is the
//! medium between volumes: each fleet database ships its WAL there under
//! `<root>/fleet/tenant/<id>` or `<root>/fleet/slot/<nnnnn>`, through the same
//! engine ([`Replicator`]) and destination as the control database.
//!
//! ```text
//! open    ─► register a Replicator (its own connection, its own status)
//! loop    ─► every sync_interval, tick each registered Replicator
//! close   ─► pool drained ─► final tick (ships the last frames) ─► drop
//!                             └► only now may SQLite's last-connection
//!                                checkpoint run, with nothing unshipped
//! missing ─► restore_missing = true: rebuild the file from its replica
//!            before the open, so a fresh volume serves the database
//! delete  ─► purge the replica, or keep it for the retention; a tombstone
//!            blocks restore_missing until then (see `fleet_tombstones`)
//! check   ─► every verify interval: restore a random sample into scratch
//!            files, check integrity and the identity row
//! ```
//!
//! The databases run with `wal_autocheckpoint = 0`; the replicator is the only
//! checkpointer, and it checkpoints only what is already offsite. That is the
//! control database's invariant, applied per database.
//!
//! Processes can share a fleet root. Only one of them replicates a database:
//! the one that holds the OS lock `<db>.replicator.lock`. The others open the
//! database, never checkpoint it, and try the lock again on each tick. The OS
//! frees the lock when its holder exits or dies. The new holder starts a new
//! generation, so a restore never misses a commit.

// autumn-determinism-gate: production code in this module must read time and
// mint identifiers through the framework's injected seams (ClockSource /
// Entropy), never `Instant::now()` / `Utc::now()` / `SystemTime::now()` /
// `Uuid::new_v4()` directly. See CONTRIBUTING.md "Determinism seam gate"
// (issue #1797). Justify exceptions with
// #[allow(clippy::disallowed_methods, reason = "…")] at the narrowest scope.
#![cfg_attr(not(test), deny(clippy::disallowed_methods))]

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::actuator::{HealthCheckOutput, HealthIndicator};
use crate::entropy::Entropy;
use crate::fleet_layout::FleetDbKey;
use crate::replication::destination::ReplicaDestination;
use crate::replication::engine::{ReplicationSettings, Replicator};
use crate::replication::restore::{RestoreError, RestoreOutcome};
use crate::replication::segment;
use crate::replication::status::ReplicationStatus;
use crate::time::ClockSource;

use super::fleet::{FleetDatabase, FleetLifecycle};
use super::fleet_tombstones::{
    FleetTombstones, MARKER_OBJECT, Marker, PurgeReport, Tombstone, purge_deletes, restore_allowed,
};

/// Health indicator name of the fleet's replication.
pub const FLEET_REPLICATION_INDICATOR: &str = "replication:fleet";

/// How often the loop checks for shutdown while it sleeps.
const STOP_POLL: Duration = Duration::from_millis(100);

/// Most ticks the close-time ship takes before it gives up and parks the
/// replicator. One tick ships at most one segment, so a backlog can take
/// several.
const FINAL_SHIP_MAX_TICKS: usize = 64;

/// Longest a closing non-owner waits for the owner to hand over the lock.
const CLAIM_WAIT: Duration = Duration::from_secs(2);

/// Longest a reopen under a new epoch waits for a tick to free the parked
/// replicator. After it, the open is refused and the caller retries.
const REUSE_WAIT: Duration = Duration::from_secs(2);

/// Pause between claim attempts at close.
const CLAIM_POLL: Duration = Duration::from_millis(25);

/// Pause between flush passes at shutdown.
const FLUSH_RETRY: Duration = Duration::from_millis(100);

/// The default number of threads that ship the databases of one pass.
pub const DEFAULT_SHIP_WORKERS: usize = 4;

/// The pause after a pass that took `took`, so that a pass starts once per
/// `interval`. Zero when the pass took longer than `interval`.
const fn pause_after(interval: Duration, took: Duration) -> Duration {
    interval.saturating_sub(took)
}

/// The keys, the oldest time first. A pass ships in this order, so the
/// database with the most lag ships first.
fn oldest_first(mut entries: Vec<(FleetDbKey, DateTime<Utc>)>) -> Vec<FleetDbKey> {
    entries.sort_by_key(|entry| entry.1);
    entries.into_iter().map(|(key, _)| key).collect()
}

/// Run `work` once for each key, on at most `workers` threads. The calling
/// thread is one of them. Blocking: it returns when all keys are done.
fn for_each_on(workers: usize, keys: &[FleetDbKey], work: impl Fn(&FleetDbKey) + Sync) {
    let next = std::sync::atomic::AtomicUsize::new(0);
    let drain = || {
        while let Some(key) = keys.get(next.fetch_add(1, Ordering::Relaxed)) {
            work(key);
        }
    };
    let helpers = workers.min(keys.len()).saturating_sub(1);
    std::thread::scope(|scope| {
        for _ in 0..helpers {
            // If a thread does not start, the other threads do its keys.
            let _ = std::thread::Builder::new()
                .name("autumn-fleet-ship".to_owned())
                .spawn_scoped(scope, drain);
        }
        drain();
    });
}

/// Real time since `started`, in milliseconds.
fn millis_since(started: std::time::Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// No pass has run yet.
const NO_PASS: u64 = u64::MAX;

/// The destination key prefix of one fleet database, under the control
/// database's replication root.
#[must_use]
pub fn replica_root(base: &str, key: &FleetDbKey) -> String {
    let base = base.trim_end_matches('/');
    match key {
        FleetDbKey::Tenant { id, .. } => format!("{base}/fleet/tenant/{id}"),
        FleetDbKey::Slot(slot) => format!("{base}/fleet/slot/{slot:05}"),
    }
}

struct ActiveReplica {
    /// Never ticked while the lock is not held, so it opens no connection.
    replicator: Mutex<Replicator>,
    /// Declared after `replicator`: the replicator's connection closes before
    /// the lock is freed.
    lock: Mutex<Option<ReplicaLock>>,
    path: std::path::PathBuf,
    /// Why the last claim failed, when it was not simply held by another
    /// process. Such a database is never replicated.
    lock_error: Mutex<Option<String>>,
    /// Set once the database closes. A tick pass that still holds this
    /// replica must not claim the lock for it.
    retired: AtomicBool,
    /// Set when a newer ownership epoch writes to the replica (issue #3233).
    /// Such a replica ships nothing more, not even at close.
    fenced: AtomicBool,
    /// Set when a delete or a restore lets the parked replica go. A tick
    /// that still holds it ships nothing more.
    released: AtomicBool,
    /// The replicator's epoch, 0 for none. Written under the replicator
    /// lock; read without it, so a reopen under the same epoch does not wait
    /// for a tick in destination I/O.
    epoch: AtomicU64,
    status: Arc<ReplicationStatus>,
    registered_at: DateTime<Utc>,
}

impl ActiveReplica {
    fn owned(&self) -> bool {
        self.lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
    }

    /// Try to become the replicator of this database. `true` when this
    /// process owns it.
    fn claim(&self, key: &FleetDbKey) -> bool {
        if self.owned() {
            return true;
        }
        if self.retired.load(Ordering::Acquire) {
            return false;
        }
        match ReplicaLock::try_acquire(&self.path) {
            Ok(Some(held)) if self.retired.load(Ordering::Acquire) => {
                drop(held);
                false
            }
            Ok(Some(held)) => {
                *self
                    .lock_error
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = None;
                tracing::info!(database = %key, "fleet replication: this process now replicates the database");
                *self.lock.lock().unwrap_or_else(PoisonError::into_inner) = Some(held);
                true
            }
            Ok(None) => {
                *self
                    .lock_error
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = None;
                false
            }
            Err(error) => {
                tracing::warn!(database = %key, error = %error, "fleet replication: cannot take the replicator lock");
                *self
                    .lock_error
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = Some(error.to_string());
                false
            }
        }
    }
}

/// The OS lock that makes one process the replicator of a database. The OS
/// frees it when the file closes or the process ends.
struct ReplicaLock {
    _file: std::fs::File,
}

impl ReplicaLock {
    fn try_acquire(db: &Path) -> std::io::Result<Option<Self>> {
        let name = crate::fleet_layout::replicator_lock_path(db);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(name)?;
        let mut lock = fd_lock::RwLock::new(file);
        match lock.try_write() {
            // Keep the lock held: closing the file frees it, so the guard
            // must not unlock early.
            Ok(guard) => std::mem::forget(guard),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
            Err(e) => return Err(e),
        }
        Ok(Some(Self {
            _file: lock.into_inner(),
        }))
    }
}

/// Replicates the open databases of one fleet. Installed by
/// [`DatabaseFleet::install_replication`](super::fleet::DatabaseFleet::install_replication).
pub struct FleetReplication {
    destination: Arc<dyn ReplicaDestination>,
    /// The control database's settings: `root` is the base every fleet
    /// database's prefix hangs off; `database_path` is replaced per database.
    settings: ReplicationSettings,
    restore_missing: bool,
    lag_alert_after: Duration,
    clock: Arc<dyn ClockSource>,
    active: Mutex<HashMap<FleetDbKey, Arc<ActiveReplica>>>,
    /// Replicators of closed databases whose close-time ship did not catch
    /// up (a destination outage). Each keeps its own connection open, so
    /// `SQLite` cannot checkpoint the unshipped frames away; the loop keeps
    /// ticking them and drops each once it is caught up. A reopen takes its
    /// replicator back instead of starting a second one.
    parked: Mutex<HashMap<FleetDbKey, Arc<ActiveReplica>>>,
    stopped: AtomicBool,
    /// Longest the shutdown flush keeps shipping parked replicators.
    final_flush_ms: AtomicU64,
    /// Names still unshipped when the last shutdown flush ended.
    unshipped: Mutex<Vec<String>>,
    /// When shutdown must be done. Close-time shipping stops at it.
    shutdown_cutoff: Mutex<Option<std::time::Instant>>,
    /// Told when a replicator sees a newer epoch (issue #3233).
    fence_hook: Mutex<Option<FenceHook>>,
    /// The tombstone rows of deleted databases (issue #3235). `None`: a
    /// failed purge is not retried.
    tombstones: Option<Arc<FleetTombstones>>,
    /// How long a deleted database's replica stays before its purge.
    retention_after_delete: Duration,
    /// How often the restore check runs, and how many databases it takes.
    restore_check: Option<(Duration, usize)>,
    /// Picks the restore-check sample and names its scratch files.
    entropy: Arc<dyn Entropy>,
    /// One purge at a time in this process: a re-create waits for the retry
    /// task, and the other way round.
    purge_turn: tokio::sync::Mutex<()>,
    /// Parked replicas a delete or a restore let go. A purge waits for a
    /// tick that still holds one.
    released: Mutex<HashMap<FleetDbKey, std::sync::Weak<ActiveReplica>>>,
    /// The last purge pass, for health.
    last_purge: Mutex<Option<PurgeReport>>,
    /// The last restore check, for health.
    last_restore_check: Mutex<Option<RestoreCheckReport>>,
    /// The threads that ship the databases of one pass.
    ship_workers: usize,
    /// The time of the last loop pass in milliseconds, [`NO_PASS`] before
    /// it. A shutdown flush does not change it.
    last_pass_ms: AtomicU64,
}

/// The result of one restore check (issue #3235). See
/// [`DatabaseFleet::check_replica_sample`](super::fleet::DatabaseFleet::check_replica_sample).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestoreCheckReport {
    /// When the check ran (app clock).
    pub checked_at: Option<DateTime<Utc>>,
    /// Databases whose replica restored, passed `PRAGMA integrity_check`
    /// and carries the right identity row.
    pub passed: Vec<String>,
    /// Databases with no replica yet (never opened while replicated).
    pub unreplicated: Vec<String>,
    /// Databases whose restore failed, with the reason.
    pub failed: Vec<(String, String)>,
}

/// The result of one restore check of one database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RestoreCheck {
    Passed,
    NoReplica,
    Failed(String),
}

/// Called with the database and the newer epoch a replicator saw.
pub(crate) type FenceHook = Arc<dyn Fn(&FleetDbKey, u64) + Send + Sync>;

impl FleetReplication {
    /// Replicate fleet databases next to the control database.
    ///
    /// `settings` are the control database's resolved replication settings;
    /// `lag_alert_after` is the lag at which the health indicator turns
    /// `Down`; `restore_missing` rebuilds a missing database from its replica
    /// before opening it.
    #[must_use]
    pub fn new(
        destination: Arc<dyn ReplicaDestination>,
        settings: ReplicationSettings,
        lag_alert_after: Duration,
        restore_missing: bool,
        clock: Arc<dyn ClockSource>,
    ) -> Self {
        Self {
            destination,
            settings,
            restore_missing,
            lag_alert_after,
            clock,
            active: Mutex::new(HashMap::new()),
            parked: Mutex::new(HashMap::new()),
            stopped: AtomicBool::new(false),
            final_flush_ms: AtomicU64::new(5_000),
            unshipped: Mutex::new(Vec::new()),
            shutdown_cutoff: Mutex::new(None),
            fence_hook: Mutex::new(None),
            tombstones: None,
            retention_after_delete: Duration::ZERO,
            restore_check: None,
            entropy: Arc::new(crate::entropy::OsEntropy),
            purge_turn: tokio::sync::Mutex::new(()),
            released: Mutex::new(HashMap::new()),
            last_purge: Mutex::new(None),
            last_restore_check: Mutex::new(None),
            ship_workers: DEFAULT_SHIP_WORKERS,
            last_pass_ms: AtomicU64::new(NO_PASS),
        }
    }

    /// Ship the databases of a pass on `workers` threads. The default is
    /// [`DEFAULT_SHIP_WORKERS`]. `0` means `1`.
    #[must_use]
    pub const fn with_ship_workers(mut self, workers: usize) -> Self {
        self.ship_workers = if workers == 0 { 1 } else { workers };
        self
    }

    /// The threads that ship the databases of a pass.
    #[must_use]
    pub const fn ship_workers(&self) -> usize {
        self.ship_workers
    }

    /// Keep a tombstone row on the control database for each deleted
    /// database until its replica is purged (issue #3235). Without it, a
    /// failed purge is logged and not retried.
    #[must_use]
    pub fn with_tombstones(mut self, tombstones: Arc<FleetTombstones>) -> Self {
        self.tombstones = Some(tombstones);
        self
    }

    /// Keep a deleted database's replica for `retention`, then purge it.
    /// The default, zero, purges at once.
    #[must_use]
    pub const fn with_retention_after_delete(mut self, retention: Duration) -> Self {
        self.retention_after_delete = retention;
        self
    }

    /// Restore `sample` random databases every `every` (issue #3235).
    /// `None` or a sample of 0 turns the check off. The default is off.
    #[must_use]
    pub fn with_restore_check(mut self, every: Option<Duration>, sample: usize) -> Self {
        self.restore_check = every.filter(|_| sample > 0).map(|every| (every, sample));
        self
    }

    /// The entropy that picks the restore-check sample. Default: the OS.
    #[must_use]
    pub fn with_entropy(mut self, entropy: Arc<dyn Entropy>) -> Self {
        self.entropy = entropy;
        self
    }

    /// The tombstone store, if any.
    #[must_use]
    pub const fn tombstones(&self) -> Option<&Arc<FleetTombstones>> {
        self.tombstones.as_ref()
    }

    /// How long a deleted database's replica stays.
    #[must_use]
    pub const fn retention_after_delete(&self) -> Duration {
        self.retention_after_delete
    }

    /// How often the restore check runs, and its sample size.
    #[must_use]
    pub const fn restore_check(&self) -> Option<(Duration, usize)> {
        self.restore_check
    }

    /// Whether a missing database is restored from its replica.
    #[must_use]
    pub const fn restores_missing(&self) -> bool {
        self.restore_missing
    }

    pub(crate) fn entropy(&self) -> &dyn Entropy {
        self.entropy.as_ref()
    }

    pub(crate) const fn purge_turn(&self) -> &tokio::sync::Mutex<()> {
        &self.purge_turn
    }

    /// The app clock.
    pub(crate) fn now(&self) -> DateTime<Utc> {
        self.clock.now()
    }

    /// Tell `hook` when a replicator sees a newer epoch. The fleet installs
    /// it with its ownership.
    pub(crate) fn set_fence_hook(&self, hook: FenceHook) {
        *self
            .fence_hook
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(hook);
    }

    /// Stop `replica` for good: a newer epoch owns its database.
    fn fence(&self, key: &FleetDbKey, replica: &ActiveReplica, newest: u64) {
        if replica.fenced.swap(true, Ordering::AcqRel) {
            return;
        }
        tracing::warn!(database = %key, newest, "fleet replication: a newer epoch owns this database; this host stops shipping it");
        let hook = self
            .fence_hook
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(hook) = hook {
            hook(key, newest);
        }
    }

    /// Set the longest the shutdown flush keeps shipping. The default is 5
    /// seconds.
    #[must_use]
    pub fn with_final_flush(self, deadline: Duration) -> Self {
        self.set_final_flush(deadline);
        self
    }

    /// Change the longest the shutdown flush keeps shipping. Call it before
    /// shutdown starts, with the time the shutdown still has left.
    pub fn set_final_flush(&self, deadline: Duration) {
        let millis = u64::try_from(deadline.as_millis()).unwrap_or(u64::MAX);
        self.final_flush_ms.store(millis, Ordering::Release);
    }

    /// Set the time by which shutdown must be done. A database that closes
    /// after it parks its replicator and ships nothing more at close.
    pub fn set_shutdown_cutoff(&self, cutoff: Option<std::time::Instant>) {
        *self
            .shutdown_cutoff
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = cutoff;
    }

    /// Set the shutdown cutoff to `left` from now. The caller does this
    /// before it wakes the maintenance task, so the task's start-up delay is
    /// charged to the shared budget.
    pub fn set_shutdown_deadline_in(&self, left: Duration) {
        // Real time on purpose: the cutoff bounds blocking I/O.
        #[allow(
            clippy::disallowed_methods,
            reason = "real time: the shutdown bound covers blocking I/O"
        )]
        let now = std::time::Instant::now();
        self.set_shutdown_cutoff(now.checked_add(left));
    }

    /// Time until the shutdown cutoff. `None` when none is set.
    pub(crate) fn cutoff_left(&self) -> Option<Duration> {
        #[allow(
            clippy::disallowed_methods,
            reason = "real time: the shutdown bound covers blocking I/O"
        )]
        let now = std::time::Instant::now();
        self.shutdown_cutoff()
            .map(|cutoff| cutoff.saturating_duration_since(now))
    }

    pub(crate) fn shutdown_cutoff(&self) -> Option<std::time::Instant> {
        *self
            .shutdown_cutoff
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The longest the shutdown flush keeps shipping.
    #[must_use]
    pub fn final_flush(&self) -> Duration {
        Duration::from_millis(self.final_flush_ms.load(Ordering::Acquire))
    }

    /// Tick until no replicator is parked, or until `deadline` passes.
    /// This call blocks. It returns the names still unshipped. Each one is
    /// logged with `error!`. The call replaces the `unshipped` list in the
    /// health details.
    ///
    /// The deadline uses real elapsed time, ticks included. A pinned or
    /// virtual clock cannot make the loop run forever. No tick starts after
    /// the deadline, and a replica that another thread holds is skipped. A
    /// tick that is already running cannot be stopped, so the call can
    /// overrun by one tick.
    pub fn flush_closing(&self, deadline: Duration) -> Vec<String> {
        // Real time on purpose: the wait is blocking I/O, not simulated work.
        #[allow(
            clippy::disallowed_methods,
            reason = "real time: the shutdown bound covers blocking I/O"
        )]
        let started = std::time::Instant::now();
        let cutoff = started.checked_add(deadline);
        loop {
            if expired(cutoff) {
                break;
            }
            self.tick_pass(&|| cutoff);
            if self.closing() == 0 {
                break;
            }
            std::thread::sleep(FLUSH_RETRY.min(deadline.saturating_sub(started.elapsed())));
        }
        let mut unshipped: Vec<String> = self.parked().keys().map(FleetDbKey::name).collect();
        unshipped.sort();
        for name in &unshipped {
            tracing::error!(
                database = %name,
                "fleet shutdown: committed WAL is not shipped and stays on this volume"
            );
        }
        self.unshipped
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone_from(&unshipped);
        unshipped
    }

    fn root_for(&self, key: &FleetDbKey) -> String {
        replica_root(&self.settings.root, key)
    }

    /// The newest ownership epoch any host recorded at the replica of `key`
    /// (issue #3233). `None` when no generation recorded one. Blocking: one
    /// list at the destination.
    pub(crate) fn newest_epoch(&self, key: &FleetDbKey) -> Result<Option<u64>, String> {
        let root = self.root_for(key);
        Ok(self
            .destination
            .list(&crate::replication::segment::epochs_prefix(&root))
            .map_err(|error| error.to_string())?
            .iter()
            .filter_map(|marker| crate::replication::segment::parse_epoch_marker_key(&root, marker))
            .max())
    }

    fn marker_key(&self, key: &FleetDbKey) -> String {
        format!("{}/{MARKER_OBJECT}", self.root_for(key))
    }

    /// Write the tombstone marker of `key`. Hosts that do not share the
    /// control database read it. Blocking.
    pub(crate) fn write_marker(
        &self,
        key: &FleetDbKey,
        tombstone: &Tombstone,
    ) -> Result<(), String> {
        let body = serde_json::to_vec(&Marker::of(tombstone)).map_err(|e| e.to_string())?;
        self.destination
            .put(&self.marker_key(key), &body)
            .map_err(|e| e.to_string())
    }

    /// The tombstone marker of `key`, if any. Blocking.
    ///
    /// # Errors
    ///
    /// The destination failed, or the marker cannot be read. An unreadable
    /// marker refuses the restore: it is not ignored.
    pub fn marker(&self, key: &FleetDbKey) -> Result<Option<Marker>, String> {
        match self.destination.get(&self.marker_key(key)) {
            Ok(body) => serde_json::from_slice(&body)
                .map(Some)
                .map_err(|e| format!("unreadable tombstone marker: {e}")),
            Err(error) if error.is_not_found() => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }

    /// When the newest generation of the replica of `key` was created.
    /// `None` when there is none. Blocking: one list at the destination.
    ///
    /// # Errors
    ///
    /// The destination failed.
    pub(crate) fn newest_generation(
        &self,
        key: &FleetDbKey,
    ) -> Result<Option<DateTime<Utc>>, String> {
        let root = self.root_for(key);
        Ok(self
            .destination
            .list(&segment::generations_prefix(&root))
            .map_err(|e| e.to_string())?
            .iter()
            .filter_map(|object| segment::generation_of_key(&root, object))
            .filter_map(|generation| segment::parse_generation_id(&generation))
            .filter_map(|info| DateTime::from_timestamp_millis(info.created_ms))
            .max())
    }

    /// Remove the tombstone marker of `key`. Blocking.
    pub(crate) fn delete_marker(&self, key: &FleetDbKey) -> Result<(), String> {
        self.destination
            .delete(&self.marker_key(key))
            .map_err(|e| e.to_string())
    }

    /// Wait until no tick holds a replica that a delete or a restore of
    /// `key` let go. Blocking.
    pub(crate) fn quiesce_released(&self, key: &FleetDbKey) {
        let released = self
            .released
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(key);
        if let Some(replica) = released.and_then(|weak| weak.upgrade()) {
            drop(lock_replicator(&replica, &|| None));
        }
    }

    /// Delete the replica of `key` up to `cutoff` (issue #3235). Blocking.
    ///
    /// Deletes each object of a generation created at or before `cutoff`.
    /// When no newer generation exists, it also deletes the rest (epoch
    /// markers, the tombstone marker last). A newer generation is a new
    /// database with the same key: it keeps its objects. Returns the number
    /// of objects deleted.
    ///
    /// # Errors
    ///
    /// The destination failed. Some objects can be gone: call again.
    pub fn purge_replica(&self, key: &FleetDbKey, cutoff: DateTime<Utc>) -> Result<usize, String> {
        self.quiesce_released(key);
        let root = self.root_for(key);
        // The slash: `tenant/acme` is not a prefix of `tenant/acme2`.
        let objects = self
            .destination
            .list(&format!("{root}/"))
            .map_err(|e| e.to_string())?;
        let created = |object: &str| {
            segment::generation_of_key(&root, object)
                .and_then(|generation| segment::parse_generation_id(&generation))
                .and_then(|info| DateTime::from_timestamp_millis(info.created_ms))
        };
        let newer = objects
            .iter()
            .filter_map(|object| created(object))
            .any(|at| at > cutoff);
        let marker = self.marker_key(key);
        let mut deleted = 0;
        let mut marker_goes = false;
        for object in &objects {
            if !purge_deletes(created(object), cutoff, newer) {
                continue;
            }
            if *object == marker {
                marker_goes = true;
                continue;
            }
            self.destination
                .delete(object)
                .map_err(|e| format!("could not delete {object}: {e}"))?;
            deleted += 1;
        }
        // Last: a purge that stops part way stays tombstoned.
        if marker_goes {
            self.delete_marker(key)?;
            deleted += 1;
        }
        Ok(deleted)
    }

    /// Restore the replica of `key` into a scratch file under `scratch`,
    /// check its identity row, and remove the file. The restore itself runs
    /// `PRAGMA integrity_check`. A failure is tried once more: a live
    /// replicator can ship between the list and the head read. Blocking.
    pub(crate) fn check_restore(&self, key: &FleetDbKey, scratch: &Path) -> RestoreCheck {
        match self.check_restore_once(key, scratch) {
            RestoreCheck::Failed(_) => self.check_restore_once(key, scratch),
            outcome => outcome,
        }
    }

    fn check_restore_once(&self, key: &FleetDbKey, scratch: &Path) -> RestoreCheck {
        if let Err(error) = std::fs::create_dir_all(scratch) {
            return RestoreCheck::Failed(format!(
                "could not create the scratch directory: {error}"
            ));
        }
        let output = scratch.join(format!("{:016x}.db", self.entropy.next_u64()));
        let name = key.name();
        let result = match self.restore_to(key, None, &output) {
            Ok(_) => match super::fleet::read_identity(&output, &name) {
                Ok(Some(found)) if found == name => RestoreCheck::Passed,
                Ok(Some(found)) => {
                    RestoreCheck::Failed(format!("the identity row names {found}, not {name}"))
                }
                Ok(None) => RestoreCheck::Failed("the identity row is missing".to_owned()),
                Err(error) => RestoreCheck::Failed(error.to_string()),
            },
            Err(RestoreError::NoReplica { .. }) => RestoreCheck::NoReplica,
            Err(error) => RestoreCheck::Failed(error.to_string()),
        };
        let _ = std::fs::remove_file(&output);
        for sidecar in crate::fleet_layout::sidecar_paths(&output) {
            let _ = std::fs::remove_file(sidecar);
        }
        result
    }

    pub(crate) fn record_purge_report(&self, report: PurgeReport) {
        *self
            .last_purge
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(report);
    }

    pub(crate) fn record_restore_check(&self, report: RestoreCheckReport) {
        *self
            .last_restore_check
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(report);
    }

    fn active(&self) -> std::sync::MutexGuard<'_, HashMap<FleetDbKey, Arc<ActiveReplica>>> {
        self.active.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn parked(&self) -> std::sync::MutexGuard<'_, HashMap<FleetDbKey, Arc<ActiveReplica>>> {
        self.parked.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Drop a parked replicator that is still the one parked for `key`.
    fn retire_parked(&self, key: &FleetDbKey, replica: &Arc<ActiveReplica>) -> bool {
        let mut parked = self.parked();
        let current = parked
            .get(key)
            .is_some_and(|current| Arc::ptr_eq(current, replica));
        if current {
            parked.remove(key);
        }
        current
    }

    /// Closed databases still shipping their backlog.
    #[must_use]
    pub fn closing(&self) -> usize {
        self.parked().len()
    }

    fn all_replicas(&self) -> Vec<(FleetDbKey, Arc<ActiveReplica>)> {
        let mut replicas: Vec<(FleetDbKey, Arc<ActiveReplica>)> = self
            .active()
            .iter()
            .map(|(key, replica)| (key.clone(), Arc::clone(replica)))
            .collect();
        replicas.extend(
            self.parked()
                .iter()
                .map(|(key, replica)| (key.clone(), Arc::clone(replica))),
        );
        replicas
    }

    /// Open databases whose replicator lock this process does not hold.
    /// Usually another process on the same root holds it. A pass claims
    /// the lock again when that process lets it go.
    #[must_use]
    pub fn unowned(&self) -> usize {
        self.active().values().filter(|r| !r.owned()).count()
    }

    /// Databases this process replicates now.
    #[must_use]
    pub fn replicating(&self) -> usize {
        self.active().values().filter(|r| r.owned()).count()
    }

    /// Tick every registered replicator once, and retire each parked one
    /// that has caught up. Blocking. The loop calls this; tests call it to
    /// step replication without sleeping.
    ///
    /// After the shutdown cutoff (see
    /// [`set_shutdown_cutoff`](Self::set_shutdown_cutoff)), it starts no work.
    pub fn tick_all(&self) {
        #[allow(
            clippy::disallowed_methods,
            reason = "real time: the pass time measures blocking I/O"
        )]
        let started = std::time::Instant::now();
        self.tick_pass(&|| self.shutdown_cutoff());
        self.last_pass_ms
            .store(millis_since(started), Ordering::Release);
    }

    /// One tick pass. `cutoff` is read again for each replica, each lock
    /// attempt and each ship step, so a cutoff installed mid-pass takes
    /// effect. After it, no replica starts, and a replica that another thread
    /// holds is given up on.
    ///
    /// [`ship_workers`](Self::ship_workers) threads tick the replicas, the
    /// one that shipped longest ago first. A pass looks up each replica when
    /// it gets to it. It does not keep a closed database's replica, so that
    /// replica's lock is free when the database opens again.
    fn tick_pass(&self, cutoff: &(dyn Fn() -> Option<std::time::Instant> + Sync)) {
        let active = self.ship_order();
        for_each_on(self.ship_workers, &active, |key| {
            self.tick_active(key, cutoff);
        });
        let parked: Vec<FleetDbKey> = self.parked().keys().cloned().collect();
        for_each_on(self.ship_workers, &parked, |key| {
            self.tick_parked(key, cutoff);
        });
    }

    /// The open databases, the oldest last ship first. A database that never
    /// shipped counts from its registration, as its lag does.
    fn ship_order(&self) -> Vec<FleetDbKey> {
        let entries = self
            .active()
            .iter()
            .map(|(key, replica)| {
                let shipped = replica.status.snapshot().last_success_at;
                (key.clone(), shipped.unwrap_or(replica.registered_at))
            })
            .collect();
        oldest_first(entries)
    }

    /// Tick the open database `key` once.
    fn tick_active(&self, key: &FleetDbKey, cutoff: &dyn Fn() -> Option<std::time::Instant>) {
        if expired(cutoff()) {
            return;
        }
        let Some(replica) = self.active().get(key).cloned() else {
            return;
        };
        // A database that another process replicates is retried each tick.
        if replica.fenced.load(Ordering::Acquire) || !replica.claim(key) {
            return;
        }
        let Some(mut replicator) = lock_replicator(&replica, cutoff) else {
            return;
        };
        // Closed since the lookup: its close shipped the last frames, or it
        // is parked. A reopen puts a replica back in `active`.
        let current = self
            .active()
            .get(key)
            .is_some_and(|current| Arc::ptr_eq(current, &replica));
        if !current {
            return;
        }
        if let Err(error) = replicator.tick() {
            let failures = replica.status.snapshot().consecutive_failures;
            if crate::replication::engine::tick_failure_level(&error, failures)
                == tracing::Level::WARN
            {
                tracing::warn!(database = %key, error = %error, "fleet replication tick failed");
            } else {
                tracing::debug!(database = %key, error = %error, "fleet replication tick failed");
            }
        }
        // Under the replicator lock: an `on_open` that reuses this replica
        // then sees the flag.
        if let Some(newest) = replicator.fenced_by() {
            self.fence(key, &replica, newest);
        }
    }

    /// Ship the backlog of the closed database `key`, and retire its
    /// replicator when it caught up.
    fn tick_parked(&self, key: &FleetDbKey, cutoff: &dyn Fn() -> Option<std::time::Instant>) {
        if expired(cutoff()) {
            return;
        }
        let Some(replica) = self.parked().get(key).cloned() else {
            return;
        };
        let Some(mut replicator) = lock_replicator(&replica, cutoff) else {
            return;
        };
        // A delete or a restore let it go: its files are being removed. Or
        // another pass retired it, or a reopen took it back.
        let parked = self
            .parked()
            .get(key)
            .is_some_and(|current| Arc::ptr_eq(current, &replica));
        if replica.released.load(Ordering::Acquire) || !parked {
            return;
        }
        let caught_up = ship_until_caught_up(&mut replicator, cutoff).is_ok();
        let fenced_by = replicator.fenced_by();
        if let Some(newest) = fenced_by {
            self.fence(key, &replica, newest);
        }
        drop(replicator);
        if fenced_by.is_some() {
            // Nothing more ships: let the file go.
            self.retire_parked(key, &replica);
            return;
        }
        if caught_up && self.retire_parked(key, &replica) {
            tracing::info!(database = %key, "fleet replication: closed database caught up");
        }
    }

    /// Run the loop on a dedicated thread until [`stop`](Self::stop). A pass
    /// starts every `sync_interval`, or at once when the last pass took
    /// longer. The final ship of each database happens when it closes, not
    /// here.
    ///
    /// # Errors
    ///
    /// The OS refused to start the thread.
    pub fn spawn_loop(self: &Arc<Self>) -> std::io::Result<std::thread::JoinHandle<()>> {
        let this = Arc::clone(self);
        std::thread::Builder::new()
            .name("autumn-fleet-replication".to_owned())
            .spawn(move || {
                while !this.stopped.load(Ordering::Acquire) {
                    #[allow(
                        clippy::disallowed_methods,
                        reason = "real time: the loop paces blocking I/O"
                    )]
                    let started = std::time::Instant::now();
                    this.tick_all();
                    // A pass starts once per interval. A long pass does not
                    // add a full interval to the lag.
                    let pause = pause_after(this.settings.sync_interval, started.elapsed());
                    let mut slept = Duration::ZERO;
                    while slept < pause && !this.stopped.load(Ordering::Acquire) {
                        let step = STOP_POLL.min(pause.saturating_sub(slept));
                        std::thread::sleep(step);
                        slept += step;
                    }
                }
            })
    }

    /// Stop the loop after its current tick.
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
    }

    /// Rebuild the replica of `key`, as of `target` (latest when `None`),
    /// into `output`. Blocking. `output` should be a fresh staging path: the
    /// caller publishes it (see `DatabaseFleet::restore`).
    ///
    /// # Errors
    ///
    /// [`RestoreError::NoReplica`] when nothing was ever shipped for `key`, or
    /// any other restore failure.
    pub fn restore_to(
        &self,
        key: &FleetDbKey,
        target: Option<DateTime<Utc>>,
        output: &Path,
    ) -> Result<RestoreOutcome, RestoreError> {
        crate::replication::restore::restore(
            self.destination.as_ref(),
            &self.root_for(key),
            target,
            output,
        )
    }

    /// The health indicator for every replicating database.
    #[must_use]
    pub fn indicator(self: &Arc<Self>) -> Arc<dyn HealthIndicator> {
        Arc::new(FleetReplicationIndicator {
            replication: Arc::clone(self),
        })
    }

    fn health(&self) -> HealthCheckOutput {
        let now = self.clock.now();
        let closing = self.closing();
        let (replicas, unowned): (Vec<_>, Vec<_>) = self
            .all_replicas()
            .into_iter()
            .partition(|(_, replica)| replica.owned());
        let mut worst: Option<(Duration, String)> = None;
        let mut failing = 0_u64;
        let mut pending_bytes = 0_u64;
        for (key, replica) in &replicas {
            let snapshot = replica.status.snapshot();
            pending_bytes = pending_bytes.saturating_add(snapshot.pending_bytes);
            if snapshot.consecutive_failures > 0 {
                failing += 1;
            }
            // Before the first successful tick, lag runs from registration, so
            // a database that never ships still trips the threshold.
            let lag = snapshot.lag(now).unwrap_or_else(|| {
                now.signed_duration_since(replica.registered_at)
                    .to_std()
                    .unwrap_or(Duration::ZERO)
            });
            if worst.as_ref().is_none_or(|(w, _)| lag > *w) {
                worst = Some((lag, key.name()));
            }
        }
        let mut details: HashMap<String, serde_json::Value> = HashMap::new();
        details.insert("databases".to_owned(), replicas.len().into());
        details.insert("closing".to_owned(), closing.into());
        details.insert("unowned".to_owned(), unowned.len().into());
        let lock_failures: Vec<String> = unowned
            .iter()
            .filter(|(_, replica)| {
                replica
                    .lock_error
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .is_some()
            })
            .map(|(key, _)| key.name())
            .collect();
        details.insert("lock_failures".to_owned(), lock_failures.len().into());
        details.insert("failing".to_owned(), failing.into());
        details.insert(
            "unshipped".to_owned(),
            self.unshipped
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
                .into(),
        );
        details.insert("pending_bytes".to_owned(), pending_bytes.into());
        details.insert(
            "lag_alert_after_seconds".to_owned(),
            self.lag_alert_after.as_secs().into(),
        );
        details.insert("destination".to_owned(), self.destination.describe().into());
        details.insert("ship_workers".to_owned(), self.ship_workers.into());
        let last_pass_ms = self.last_pass_ms.load(Ordering::Acquire);
        if last_pass_ms != NO_PASS {
            details.insert("last_pass_ms".to_owned(), last_pass_ms.into());
        }
        let lag_ok = if let Some((lag, name)) = worst {
            details.insert("worst_lag_seconds".to_owned(), lag.as_secs().into());
            details.insert("worst_database".to_owned(), name.into());
            lag <= self.lag_alert_after
        } else {
            true
        };
        let restore_ok = self.lifecycle_details(&mut details);
        // A database whose lock cannot be taken is never replicated. A
        // replica that does not restore is not a backup.
        let healthy = lag_ok && lock_failures.is_empty() && restore_ok;
        if healthy {
            HealthCheckOutput::up().with_details(details)
        } else {
            HealthCheckOutput::down().with_details(details)
        }
    }

    /// The tombstone and restore-check details (issue #3235). `false` when
    /// the last restore check failed.
    fn lifecycle_details(&self, details: &mut HashMap<String, serde_json::Value>) -> bool {
        if let Some(purge) = self
            .last_purge
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            details.insert(
                "tombstones".to_owned(),
                serde_json::json!({
                    "purged": purge.purged,
                    "failed": purge.failed,
                    "waiting": purge.waiting,
                    "failing": purge.failing,
                    "last_error": purge.last_error,
                }),
            );
        }
        let mut restore_ok = true;
        if let Some(check) = self
            .last_restore_check
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            restore_ok = check.failed.is_empty();
            let failed: Vec<serde_json::Value> = check
                .failed
                .iter()
                .map(|(database, error)| serde_json::json!({"database": database, "error": error}))
                .collect();
            details.insert(
                "restore_check".to_owned(),
                serde_json::json!({
                    "checked_at": check.checked_at.map(|at| at.to_rfc3339()),
                    "passed": check.passed.len(),
                    "unreplicated": check.unreplicated.len(),
                    "failed": failed,
                }),
            );
        }
        restore_ok
    }
}

impl FleetReplication {
    /// Claim at close. An owner that closes at the same time still holds the
    /// lock while it ships, so wait for it, within `CLAIM_WAIT` and the
    /// shutdown cutoff.
    fn claim_for_close(&self, replica: &ActiveReplica, key: &FleetDbKey) -> bool {
        #[allow(
            clippy::disallowed_methods,
            reason = "real time: the wait bounds blocking I/O"
        )]
        let limit = std::time::Instant::now() + CLAIM_WAIT;
        loop {
            if replica.claim(key) {
                return true;
            }
            #[allow(
                clippy::disallowed_methods,
                reason = "real time: the wait bounds blocking I/O"
            )]
            let now = std::time::Instant::now();
            if now >= limit || expired(self.shutdown_cutoff()) {
                return false;
            }
            std::thread::sleep(CLAIM_POLL);
        }
    }
}

impl FleetLifecycle for FleetReplication {
    fn restore_missing(&self, key: &FleetDbKey, path: &Path) -> Result<bool, String> {
        if !self.restore_missing {
            return Ok(false);
        }
        let root = self.root_for(key);
        let plan = match crate::replication::restore::plan(self.destination.as_ref(), &root, None) {
            Ok(plan) => plan,
            Err(RestoreError::NoReplica { .. }) => return Ok(false),
            Err(error) => return Err(format!("restore from replica failed: {error}")),
        };
        // A delete on another host leaves a marker. A generation that is
        // not newer than it is the deleted database (issue #3235).
        let marker = self.marker(key)?;
        if !restore_allowed(
            false,
            marker.map(|marker| marker.deleted_at()),
            plan.generation_started_at,
        ) {
            tracing::info!(database = %key, "fleet database was deleted: its replica is not restored");
            return Ok(false);
        }
        // Restore privately, then publish with a link that refuses an
        // existing target: when two processes restore the same database at
        // once, the first to publish wins and the other opens its file,
        // rather than replacing it under a process already serving it.
        let staging = super::fleet::staging_path(path, "restoring").map_err(|e| e.to_string())?;
        let result =
            match crate::replication::restore::apply(self.destination.as_ref(), &plan, &staging) {
                Ok(outcome) => match std::fs::hard_link(&staging, path) {
                    Ok(()) => {
                        tracing::info!(
                            database = %key,
                            bytes = outcome.bytes,
                            "fleet database restored from its replica"
                        );
                        Ok(true)
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(true),
                    Err(e) => Err(format!("could not publish the restored database: {e}")),
                },
                Err(RestoreError::NoReplica { .. }) => Ok(false),
                Err(error) => Err(format!("restore from replica failed: {error}")),
            };
        super::fleet::discard_staging(&staging);
        result
    }

    fn on_open(&self, db: &FleetDatabase) -> Result<(), String> {
        // A closed database still shipping its backlog keeps its replicator:
        // two replicators on one file would both checkpoint it.
        let epoch = db.epoch().map(crate::lock::FencingToken::get);
        let parked = self.parked().remove(db.key());
        // A fenced replica cannot be reused.
        if let Some(parked) = parked
            && !parked.fenced.load(Ordering::Acquire)
        {
            if parked.epoch.load(Ordering::Acquire) == epoch.unwrap_or(0) {
                // Nothing to change, so no wait for a tick in destination
                // I/O. A fence that lands now names a newer epoch, and that
                // also drops this host's range.
                self.active().insert(db.key().clone(), parked);
                return Ok(());
            }
            // A new epoch starts a new generation. The flag is read under
            // the replicator lock, which a tick holds while it sets it. The
            // tick may be in destination I/O: wait only so long.
            #[allow(
                clippy::disallowed_methods,
                reason = "real time: the wait bounds blocking I/O"
            )]
            let limit = std::time::Instant::now() + REUSE_WAIT;
            let Some(mut replicator) = lock_replicator(&parked, &|| Some(limit)) else {
                self.parked().insert(db.key().clone(), parked);
                return Err(
                    "the replicator of this database is busy shipping its backlog; retry"
                        .to_owned(),
                );
            };
            if !parked.fenced.load(Ordering::Acquire) {
                replicator.set_epoch(epoch);
                parked.epoch.store(epoch.unwrap_or(0), Ordering::Release);
                drop(replicator);
                self.active().insert(db.key().clone(), Arc::clone(&parked));
                return Ok(());
            }
        }
        let settings = ReplicationSettings {
            database_path: db.path().to_path_buf(),
            root: self.root_for(db.key()),
            // A verification restores the whole database; across a fleet that
            // is the operator's call (`restore`), not a timer's.
            verify_interval: None,
            ..self.settings.clone()
        };
        let status = Arc::new(ReplicationStatus::new(self.destination.describe()));
        let replicator =
            Replicator::new(settings, Arc::clone(&self.destination), Arc::clone(&status))
                .with_clock(Arc::clone(&self.clock))
                .with_epoch(epoch);
        let replica = Arc::new(ActiveReplica {
            replicator: Mutex::new(replicator),
            lock: Mutex::new(None),
            lock_error: Mutex::new(None),
            retired: AtomicBool::new(false),
            fenced: AtomicBool::new(false),
            released: AtomicBool::new(false),
            epoch: AtomicU64::new(epoch.unwrap_or(0)),
            path: db.path().to_path_buf(),
            status,
            registered_at: self.clock.now(),
        });
        // Another process may own it. The loop claims it when that one lets go.
        replica.claim(db.key());
        self.active().insert(db.key().clone(), replica);
        Ok(())
    }

    fn on_close(&self, db: &FleetDatabase) {
        let Some(replica) = self.active().remove(db.key()) else {
            return;
        };
        // The owner may have left since the last tick. Try once, so commits
        // made meanwhile ship. Then no later tick may claim for this replica.
        // A fenced replica ships nothing: a newer epoch owns the replica.
        if replica.fenced.load(Ordering::Acquire) {
            replica.retired.store(true, Ordering::Release);
            return;
        }
        let owner = self.claim_for_close(&replica, db.key());
        replica.retired.store(true, Ordering::Release);
        if !owner {
            return;
        }
        // Every pooled connection is gone. Ship everything they committed
        // before the replicator's own connection — the last one — closes and
        // lets SQLite checkpoint.
        // The cutoff-aware lock: another thread may hold the replica, and
        // shutdown must not wait for it past the cutoff.
        let shipped = lock_replicator(&replica, &|| self.shutdown_cutoff()).map_or_else(
            || {
                Err((
                    "the shutdown cutoff passed before the close could ship".to_owned(),
                    None,
                ))
            },
            |mut replicator| {
                ship_until_caught_up(&mut replicator, &|| self.shutdown_cutoff())
                    .map_err(|error| (error, replicator.fenced_by()))
            },
        );
        let shipped = match shipped {
            Err((_, Some(newest))) => {
                self.fence(db.key(), &replica, newest);
                return;
            }
            other => other.map_err(|(error, _)| error),
        };
        if let Err(error) = shipped {
            tracing::warn!(
                database = %db.key(),
                error = %error,
                "fleet replication: the close-time ship did not catch up; the replicator \
                 stays open and keeps shipping until it does"
            );
            self.parked().insert(db.key().clone(), replica);
        }
    }

    fn release(&self, key: &FleetDbKey) {
        // The files are about to be removed or replaced: a parked replicator
        // must not hold them open, and must ship nothing more.
        let Some(replica) = self.parked().remove(key) else {
            return;
        };
        replica.released.store(true, Ordering::Release);
        self.released
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key.clone(), Arc::downgrade(&replica));
    }
}

/// Tick until nothing committed is left unshipped.
///
/// # Errors
///
/// A tick failed, made no progress, or the backlog outlasted
/// [`FINAL_SHIP_MAX_TICKS`].
fn ship_until_caught_up(
    replicator: &mut Replicator,
    cutoff: &dyn Fn() -> Option<std::time::Instant>,
) -> Result<(), String> {
    for _ in 0..FINAL_SHIP_MAX_TICKS {
        // Read it each step: shutdown can install it while a close ships.
        if expired(cutoff()) {
            return Err("the shutdown flush ran out of time".to_owned());
        }
        let report = replicator.tick().map_err(|e| e.to_string())?;
        if report.pending_bytes == 0 {
            return Ok(());
        }
        let progressed = report.segments > 0 || report.snapshot_taken || report.checkpointed;
        if !progressed {
            return Err(format!(
                "{} WAL bytes are pending and the last tick shipped nothing",
                report.pending_bytes
            ));
        }
    }
    Err(format!("still behind after {FINAL_SHIP_MAX_TICKS} ticks"))
}

/// Whether `cutoff` has passed. Real time on purpose: the flush bounds
/// blocking I/O, not simulated work.
fn expired(cutoff: Option<std::time::Instant>) -> bool {
    #[allow(
        clippy::disallowed_methods,
        reason = "real time: the shutdown bound covers blocking I/O"
    )]
    cutoff.is_some_and(|cutoff| std::time::Instant::now() >= cutoff)
}

/// Pause between lock attempts while another thread holds a replicator.
const LOCK_POLL: Duration = Duration::from_millis(5);

/// Lock a replicator. Never block without a way out: poll, and read the
/// cutoff again on every attempt and after the lock is won. `None` once the
/// cutoff has passed, so shutdown work cannot start late behind another
/// thread's tick.
fn lock_replicator<'a>(
    replica: &'a ActiveReplica,
    cutoff: &dyn Fn() -> Option<std::time::Instant>,
) -> Option<std::sync::MutexGuard<'a, Replicator>> {
    loop {
        if expired(cutoff()) {
            return None;
        }
        match replica.replicator.try_lock() {
            Ok(guard) => return (!expired(cutoff())).then_some(guard),
            Err(std::sync::TryLockError::Poisoned(poisoned)) => {
                return (!expired(cutoff())).then(|| poisoned.into_inner());
            }
            Err(std::sync::TryLockError::WouldBlock) => std::thread::sleep(LOCK_POLL),
        }
    }
}

struct FleetReplicationIndicator {
    replication: Arc<FleetReplication>,
}

impl HealthIndicator for FleetReplicationIndicator {
    fn check(&self) -> futures::future::BoxFuture<'_, HealthCheckOutput> {
        Box::pin(std::future::ready(self.replication.health()))
    }

    fn group(&self) -> crate::actuator::IndicatorGroup {
        crate::actuator::IndicatorGroup::HealthOnly
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DatabaseFleetConfig;
    use crate::db::fleet::{DatabaseFleet, FleetError};
    use crate::fleet_layout::FleetMode;
    use crate::replication::FileDestination;
    use tokio_util::sync::CancellationToken;

    /// Bound for "is not stuck" checks. A real hang still trips it. A slow
    /// Windows runner does not.
    const NOT_STUCK: Duration = Duration::from_secs(30);

    fn settings() -> ReplicationSettings {
        ReplicationSettings {
            database_path: std::path::PathBuf::new(),
            root: "app/test".to_owned(),
            sync_interval: Duration::from_secs(1),
            snapshot_interval: Duration::from_secs(3600),
            max_wal_bytes: 4 * 1024 * 1024,
            retention: Duration::from_secs(24 * 3600),
            verify_interval: None,
        }
    }

    fn replicated_fleet(
        root: &Path,
        replicas: &Path,
        restore_missing: bool,
    ) -> (DatabaseFleet, Arc<FleetReplication>) {
        let fleet = DatabaseFleet::builder(DatabaseFleetConfig {
            mode: FleetMode::Tenant,
            root: root.display().to_string(),
            path: None,
            max_open: 8,
            pool_size: 2,
            create_on_demand: Some(true),
            idle_close_secs: 0,
            restore_missing,
            ownership: None,
            replica_retention_after_delete: std::time::Duration::ZERO,
            restore_check_sample: 0,
            replication_workers: 4,
            min_residency: std::time::Duration::from_secs(2),
        })
        .build()
        .unwrap();
        let destination: Arc<dyn ReplicaDestination> =
            Arc::new(FileDestination::new(replicas).unwrap());
        let replication = Arc::new(FleetReplication::new(
            destination,
            settings(),
            Duration::from_secs(30),
            restore_missing,
            Arc::new(crate::time::SystemClock),
        ));
        assert!(fleet.install_replication(Arc::clone(&replication)).is_ok());
        assert!(fleet.install_replication(Arc::clone(&replication)).is_err());
        (fleet, replication)
    }

    async fn exec(db: &FleetDatabase, sql: &str) {
        use diesel_async::RunQueryDsl as _;
        let mut conn = db.pool().get().await.unwrap();
        diesel::sql_query(sql).execute(&mut *conn).await.unwrap();
    }

    async fn count(db: &FleetDatabase) -> i64 {
        use diesel_async::RunQueryDsl as _;
        #[derive(diesel::QueryableByName)]
        struct Count {
            #[diesel(sql_type = diesel::sql_types::BigInt)]
            n: i64,
        }
        let mut conn = db.pool().get().await.unwrap();
        diesel::sql_query("SELECT COUNT(*) AS n FROM notes")
            .get_result::<Count>(&mut *conn)
            .await
            .unwrap()
            .n
    }

    async fn wal_autocheckpoint(db: &FleetDatabase) -> i64 {
        use diesel_async::RunQueryDsl as _;
        #[derive(diesel::QueryableByName)]
        struct Value {
            #[diesel(sql_type = diesel::sql_types::BigInt)]
            wal_autocheckpoint: i64,
        }
        let mut conn = db.pool().get().await.unwrap();
        diesel::sql_query("PRAGMA wal_autocheckpoint")
            .get_result::<Value>(&mut *conn)
            .await
            .unwrap()
            .wal_autocheckpoint
    }

    #[test]
    fn replica_roots_hang_off_the_control_root() {
        assert_eq!(
            replica_root("app/prod/", &FleetDbKey::Slot(42)),
            "app/prod/fleet/slot/00042"
        );
        let key = FleetDbKey::Tenant {
            id: crate::fleet_layout::TenantDbId::parse("acme").unwrap(),
            slot: 1,
        };
        assert_eq!(replica_root("app/prod", &key), "app/prod/fleet/tenant/acme");
        for root in [
            replica_root("app/prod", &key),
            replica_root("app/prod", &FleetDbKey::Slot(0)),
        ] {
            crate::replication::destination::validate_key(&root).unwrap();
        }
    }

    #[tokio::test]
    async fn a_closed_database_ships_its_last_frames_and_restores() {
        let tmp = tempfile::tempdir().unwrap();
        let replicas = tmp.path().join("replicas");
        let (fleet, replication) = replicated_fleet(&tmp.path().join("a"), &replicas, false);
        let key = fleet.key_for("acme").unwrap();
        let db = fleet.open(&key).await.unwrap();
        assert_eq!(
            wal_autocheckpoint(&db).await,
            0,
            "the replicator checkpoints"
        );
        assert_eq!(replication.replicating(), 1);
        exec(&db, "CREATE TABLE notes (body TEXT NOT NULL)").await;
        exec(&db, "INSERT INTO notes VALUES ('shipped by a tick')").await;
        crate::time::spawn_blocking({
            let replication = Arc::clone(&replication);
            move || replication.tick_all()
        })
        .await
        .unwrap();
        // Written after the last tick: only the close-time ship carries it.
        exec(&db, "INSERT INTO notes VALUES ('shipped on close')").await;
        drop(db);
        fleet.close(&key).await;
        assert_eq!(replication.replicating(), 0);

        // Lose the volume.
        let path = fleet.path_of(&key);
        std::fs::remove_file(&path).unwrap();
        for sidecar in crate::fleet_layout::sidecar_paths(&path) {
            let _ = std::fs::remove_file(sidecar);
        }
        let outcome = fleet.restore(&key, None).await.unwrap();
        assert!(outcome.bytes > 0);
        let restored = fleet.open(&key).await.unwrap();
        assert_eq!(count(&restored).await, 2);
    }

    #[tokio::test]
    async fn a_fresh_volume_serves_a_database_from_its_replica() {
        let tmp = tempfile::tempdir().unwrap();
        let replicas = tmp.path().join("replicas");
        let (old_host, _) = replicated_fleet(&tmp.path().join("old"), &replicas, false);
        let key = old_host.key_for("acme").unwrap();
        let db = old_host.open(&key).await.unwrap();
        exec(&db, "CREATE TABLE notes (body TEXT NOT NULL)").await;
        exec(&db, "INSERT INTO notes VALUES ('moved with the slot')").await;
        drop(db);
        old_host.close_all().await;

        // A second host with an empty volume and the same object storage.
        let (new_host, replication) = replicated_fleet(&tmp.path().join("new"), &replicas, true);
        assert!(!new_host.exists(&key));
        let db = new_host.open(&key).await.unwrap();
        assert_eq!(count(&db).await, 1);
        assert_eq!(new_host.stats().created_total, 0, "restored, not created");
        assert_eq!(replication.replicating(), 1, "and replicating again");

        // A key that never shipped is simply created.
        new_host.open_for("globex").await.unwrap();
        assert_eq!(new_host.stats().created_total, 1);
    }

    #[tokio::test]
    async fn restore_without_replication_or_replica_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let plain = DatabaseFleet::builder(DatabaseFleetConfig {
            mode: FleetMode::Slot,
            root: tmp.path().join("plain").display().to_string(),
            path: None,
            max_open: 8,
            pool_size: 2,
            create_on_demand: None,
            idle_close_secs: 0,
            restore_missing: false,
            ownership: None,
            replica_retention_after_delete: std::time::Duration::ZERO,
            restore_check_sample: 0,
            replication_workers: 4,
            min_residency: std::time::Duration::from_secs(2),
        })
        .build()
        .unwrap();
        let key = FleetDbKey::Slot(1);
        assert!(matches!(
            plain.restore(&key, None).await,
            Err(FleetError::NotReplicated { .. })
        ));
        let db = plain.open(&key).await.unwrap();
        assert_eq!(
            wal_autocheckpoint(&db).await,
            1000,
            "a fleet nobody replicates keeps SQLite's own checkpointing"
        );

        let (fleet, _) =
            replicated_fleet(&tmp.path().join("r"), &tmp.path().join("replicas"), false);
        let never_shipped = fleet.key_for("nobody").unwrap();
        assert!(matches!(
            fleet.restore(&never_shipped, None).await,
            Err(FleetError::NotFound { .. })
        ));
    }

    #[tokio::test]
    async fn the_indicator_reports_every_replicating_database() {
        let tmp = tempfile::tempdir().unwrap();
        let (fleet, replication) =
            replicated_fleet(&tmp.path().join("a"), &tmp.path().join("replicas"), false);
        let output = replication.indicator().check().await;
        assert_eq!(output.status, crate::actuator::HealthStatus::Up);
        assert_eq!(output.details["databases"], 0);

        fleet.open_for("acme").await.unwrap();
        crate::time::spawn_blocking({
            let replication = Arc::clone(&replication);
            move || replication.tick_all()
        })
        .await
        .unwrap();
        let output = replication.indicator().check().await;
        assert_eq!(
            output.status,
            crate::actuator::HealthStatus::Up,
            "{:?}",
            output.details
        );
        assert_eq!(output.details["databases"], 1);
        assert_eq!(output.details["failing"], 0);
        assert_eq!(output.details["worst_database"], "tenant:acme");
        fleet.close_all().await;
    }

    async fn tick(replication: &Arc<FleetReplication>) {
        crate::time::spawn_blocking({
            let replication = Arc::clone(replication);
            move || replication.tick_all()
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn two_fleets_on_one_root_elect_one_replicator() {
        let tmp = tempfile::tempdir().unwrap();
        let (root, replicas) = (tmp.path().join("shared"), tmp.path().join("replicas"));
        let (a, ra) = replicated_fleet(&root, &replicas, false);
        let (b, rb) = replicated_fleet(&root, &replicas, false);
        let key = a.key_for("acme").unwrap();
        let _db_a = a.open(&key).await.unwrap();
        let _db_b = b.open(&key).await.unwrap();
        assert_eq!(ra.replicating() + rb.replicating(), 1);
        assert_eq!(ra.unowned() + rb.unowned(), 1);
        let loser = if ra.unowned() == 1 { &rb } else { &ra };
        assert_eq!(loser.replicating(), 1, "the other process owns it");
    }

    #[tokio::test]
    async fn the_other_process_takes_over_and_a_restore_has_every_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let (root, replicas) = (tmp.path().join("shared"), tmp.path().join("replicas"));
        let (a, ra) = replicated_fleet(&root, &replicas, false);
        let (b, rb) = replicated_fleet(&root, &replicas, false);
        let key = a.key_for("acme").unwrap();
        let db_a = a.open(&key).await.unwrap();
        let db_b = b.open(&key).await.unwrap();
        assert_eq!(
            (ra.replicating(), rb.replicating()),
            (1, 0),
            "A opened first"
        );

        exec(&db_a, "CREATE TABLE notes (body TEXT NOT NULL)").await;
        exec(&db_a, "INSERT INTO notes VALUES ('from a')").await;
        tick(&ra).await;
        tick(&rb).await;
        assert_eq!(rb.replicating(), 0, "B waits while A lives");

        // A closes: it ships the last frames and lets go.
        drop(db_a);
        a.close(&key).await;
        tick(&rb).await;
        assert_eq!((rb.replicating(), rb.unowned()), (1, 0), "B took over");
        exec(&db_b, "INSERT INTO notes VALUES ('from b')").await;
        drop(db_b);
        b.close(&key).await;

        let path = b.path_of(&key);
        std::fs::remove_file(&path).unwrap();
        for sidecar in crate::fleet_layout::sidecar_paths(&path) {
            let _ = std::fs::remove_file(sidecar);
        }
        b.restore(&key, None).await.unwrap();
        assert_eq!(count(&b.open(&key).await.unwrap()).await, 2);
    }

    #[tokio::test]
    async fn health_counts_databases_this_process_does_not_own() {
        let tmp = tempfile::tempdir().unwrap();
        let (root, replicas) = (tmp.path().join("shared"), tmp.path().join("replicas"));
        let (a, _ra) = replicated_fleet(&root, &replicas, false);
        let (b, rb) = replicated_fleet(&root, &replicas, false);
        let key = a.key_for("acme").unwrap();
        let _db_a = a.open(&key).await.unwrap();
        let _db_b = b.open(&key).await.unwrap();
        let output = rb.indicator().check().await;
        assert_eq!(output.status, crate::actuator::HealthStatus::Up);
        assert_eq!(output.details["unowned"], 1);
        assert_eq!(output.details["databases"], 0);
    }

    #[tokio::test]
    async fn a_dropped_owner_frees_the_database_for_the_other_process() {
        let tmp = tempfile::tempdir().unwrap();
        let (root, replicas) = (tmp.path().join("shared"), tmp.path().join("replicas"));
        let (a, ra) = replicated_fleet(&root, &replicas, false);
        let (b, rb) = replicated_fleet(&root, &replicas, false);
        let key = a.key_for("acme").unwrap();
        let db_a = a.open(&key).await.unwrap();
        let _db_b = b.open(&key).await.unwrap();
        exec(&db_a, "CREATE TABLE notes (body TEXT NOT NULL)").await;
        exec(&db_a, "INSERT INTO notes VALUES ('from a')").await;
        tick(&ra).await;
        // The owner ends without a close.
        ra.active().clear();
        tick(&rb).await;
        assert_eq!(rb.replicating(), 1, "B took over after A ended");
    }

    #[tokio::test]
    async fn closing_a_database_another_process_owns_keeps_the_owner() {
        let tmp = tempfile::tempdir().unwrap();
        let (root, replicas) = (tmp.path().join("shared"), tmp.path().join("replicas"));
        let (a, ra) = replicated_fleet(&root, &replicas, false);
        let (b, rb) = replicated_fleet(&root, &replicas, false);
        let key = a.key_for("acme").unwrap();
        let _db_a = a.open(&key).await.unwrap();
        let db_b = b.open(&key).await.unwrap();
        drop(db_b);
        b.close(&key).await;
        assert_eq!((rb.replicating(), rb.unowned(), rb.closing()), (0, 0, 0));
        assert_eq!(ra.replicating(), 1);
    }

    #[tokio::test]
    async fn an_owner_can_reopen_after_a_close() {
        let tmp = tempfile::tempdir().unwrap();
        let (fleet, replication) =
            replicated_fleet(&tmp.path().join("a"), &tmp.path().join("replicas"), false);
        let key = fleet.key_for("acme").unwrap();
        drop(fleet.open(&key).await.unwrap());
        fleet.close(&key).await;
        fleet.open(&key).await.unwrap();
        assert_eq!((replication.replicating(), replication.unowned()), (1, 0));
    }

    #[tokio::test]
    async fn a_lock_that_cannot_be_taken_turns_health_down() {
        let tmp = tempfile::tempdir().unwrap();
        let (fleet, replication) =
            replicated_fleet(&tmp.path().join("a"), &tmp.path().join("replicas"), false);
        let key = fleet.key_for("acme").unwrap();
        drop(fleet.open(&key).await.unwrap());
        fleet.close(&key).await;
        // A directory in the lock path makes the next claim fail.
        let lock = crate::fleet_layout::replicator_lock_path(&fleet.path_of(&key));
        std::fs::remove_file(&lock).unwrap();
        std::fs::create_dir(&lock).unwrap();
        fleet.open(&key).await.unwrap();
        assert_eq!(replication.unowned(), 1);
        let output = replication.indicator().check().await;
        assert_eq!(output.status, crate::actuator::HealthStatus::Down);
        assert_eq!(output.details["lock_failures"], 1);
    }

    #[tokio::test]
    async fn delete_removes_the_lock_file_and_the_bucket() {
        let tmp = tempfile::tempdir().unwrap();
        let (fleet, _) =
            replicated_fleet(&tmp.path().join("a"), &tmp.path().join("replicas"), false);
        let key = fleet.key_for("acme").unwrap();
        let path = fleet.path_of(&key);
        let bucket = path.parent().unwrap().to_path_buf();
        drop(fleet.open(&key).await.unwrap());
        let lock = crate::fleet_layout::replicator_lock_path(&path);
        assert!(lock.exists());
        assert!(fleet.delete(&key).await.unwrap());
        assert!(!lock.exists());
        assert!(!bucket.exists());
    }

    #[tokio::test]
    async fn a_non_owner_closing_with_the_owner_waits_for_the_handover() {
        let tmp = tempfile::tempdir().unwrap();
        let (root, replicas) = (tmp.path().join("shared"), tmp.path().join("replicas"));
        let (a, _ra) = replicated_fleet(&root, &replicas, false);
        let (b, rb) = replicated_fleet(&root, &replicas, false);
        let key = a.key_for("acme").unwrap();
        let db_a = a.open(&key).await.unwrap();
        let db_b = b.open(&key).await.unwrap();
        exec(&db_a, "CREATE TABLE notes (body TEXT NOT NULL)").await;
        exec(&db_b, "INSERT INTO notes VALUES ('from b')").await;
        drop(db_b);
        // A lets go shortly after B starts to close.
        drop(db_a);
        let closer = tokio::spawn({
            let (a, key) = (a.clone(), key.clone());
            async move {
                tokio::time::sleep(Duration::from_millis(200)).await;
                a.close(&key).await;
            }
        });
        b.close(&key).await;
        closer.await.unwrap();
        assert_eq!(rb.closing(), 0);

        let path = b.path_of(&key);
        std::fs::remove_file(&path).unwrap();
        for sidecar in crate::fleet_layout::sidecar_paths(&path) {
            let _ = std::fs::remove_file(sidecar);
        }
        b.restore(&key, None).await.unwrap();
        assert_eq!(count(&b.open(&key).await.unwrap()).await, 1);
    }

    /// A destination that can be switched off, for an outage mid-close.
    struct Flaky {
        inner: FileDestination,
        down: AtomicBool,
    }

    impl Flaky {
        fn check(&self) -> Result<(), crate::replication::DestinationError> {
            if self.down.load(Ordering::SeqCst) {
                Err(crate::replication::DestinationError::Io {
                    op: "put",
                    detail: "destination is down".to_owned(),
                })
            } else {
                Ok(())
            }
        }
    }

    impl ReplicaDestination for Flaky {
        fn describe(&self) -> String {
            "flaky".to_owned()
        }
        fn put(&self, key: &str, body: &[u8]) -> Result<(), crate::replication::DestinationError> {
            self.check()?;
            self.inner.put(key, body)
        }
        fn put_file(
            &self,
            key: &str,
            path: &Path,
        ) -> Result<(), crate::replication::DestinationError> {
            self.check()?;
            self.inner.put_file(key, path)
        }
        fn get(&self, key: &str) -> Result<Vec<u8>, crate::replication::DestinationError> {
            self.inner.get(key)
        }
        fn get_to_file(
            &self,
            key: &str,
            path: &Path,
        ) -> Result<(), crate::replication::DestinationError> {
            self.inner.get_to_file(key, path)
        }
        fn list(&self, prefix: &str) -> Result<Vec<String>, crate::replication::DestinationError> {
            self.inner.list(prefix)
        }
        fn delete(&self, key: &str) -> Result<(), crate::replication::DestinationError> {
            self.inner.delete(key)
        }
    }

    #[tokio::test]
    async fn a_close_during_an_outage_keeps_shipping_until_caught_up() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = DatabaseFleet::builder(DatabaseFleetConfig {
            mode: FleetMode::Tenant,
            root: tmp.path().join("a").display().to_string(),
            path: None,
            max_open: 8,
            pool_size: 2,
            create_on_demand: Some(true),
            idle_close_secs: 0,
            restore_missing: false,
            ownership: None,
            replica_retention_after_delete: std::time::Duration::ZERO,
            restore_check_sample: 0,
            replication_workers: 4,
            min_residency: std::time::Duration::from_secs(2),
        })
        .build()
        .unwrap();
        let flaky = Arc::new(Flaky {
            inner: FileDestination::new(tmp.path().join("replicas")).unwrap(),
            down: AtomicBool::new(true),
        });
        let replication = Arc::new(FleetReplication::new(
            Arc::clone(&flaky) as Arc<dyn ReplicaDestination>,
            settings(),
            Duration::from_secs(30),
            false,
            Arc::new(crate::time::SystemClock),
        ));
        assert!(fleet.install_replication(Arc::clone(&replication)).is_ok());
        let key = fleet.key_for("acme").unwrap();
        let db = fleet.open(&key).await.unwrap();
        exec(&db, "CREATE TABLE notes (body TEXT NOT NULL)").await;
        exec(
            &db,
            "INSERT INTO notes VALUES ('written during the outage')",
        )
        .await;
        drop(db);
        fleet.close(&key).await;
        assert_eq!(replication.replicating(), 0);
        assert_eq!(replication.closing(), 1, "parked, not dropped");
        let health = replication.indicator().check().await;
        assert_eq!(health.details["closing"], 1);

        // Reopening takes the parked replicator back: one per file.
        let db = fleet.open(&key).await.unwrap();
        assert_eq!((replication.replicating(), replication.closing()), (1, 0));
        drop(db);
        fleet.close(&key).await;
        assert_eq!(replication.closing(), 1);

        // The destination recovers; the loop ships the backlog and retires it.
        flaky.down.store(false, Ordering::SeqCst);
        crate::time::spawn_blocking({
            let replication = Arc::clone(&replication);
            move || replication.tick_all()
        })
        .await
        .unwrap();
        assert_eq!(replication.closing(), 0);

        // What was written during the outage is in the replica.
        let path = fleet.path_of(&key);
        std::fs::remove_file(&path).unwrap();
        fleet.restore(&key, None).await.unwrap();
        assert_eq!(count(&fleet.open(&key).await.unwrap()).await, 1);
    }

    #[tokio::test]
    async fn a_failed_restore_releases_the_database() {
        let tmp = tempfile::tempdir().unwrap();
        let replicas = tmp.path().join("replicas");
        let (fleet, _) = replicated_fleet(&tmp.path().join("a"), &replicas, false);
        let key = fleet.key_for("acme").unwrap();
        // A file where the bucket directory belongs: creating it fails.
        std::fs::write(
            fleet.root().join(format!("{:03}", key.bucket())),
            b"not a directory",
        )
        .unwrap();
        let err = fleet.restore(&key, None).await.unwrap_err();
        assert!(matches!(err, FleetError::Io { .. }), "{err}");
        // The key is not stranded on a drain that never finishes.
        let reopened = tokio::time::timeout(NOT_STUCK, fleet.open(&key)).await;
        assert!(
            reopened.is_ok(),
            "open must not hang after a failed restore"
        );
    }

    /// Counts the `release` calls a restore makes once its drain is over.
    #[derive(Default)]
    struct Releases(std::sync::atomic::AtomicUsize);

    impl super::FleetLifecycle for Releases {
        fn release(&self, _key: &FleetDbKey) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn a_cancelled_restore_still_releases_the_database() {
        let tmp = tempfile::tempdir().unwrap();
        let replicas = tmp.path().join("replicas");
        let (fleet, replication) = replicated_fleet(&tmp.path().join("a"), &replicas, false);
        let releases = Arc::new(Releases::default());
        fleet.add_lifecycle(releases.clone());
        let key = fleet.key_for("acme").unwrap();
        let db = fleet.open(&key).await.unwrap();
        exec(&db, "CREATE TABLE notes (body TEXT NOT NULL)").await;
        exec(&db, "INSERT INTO notes VALUES ('shipped')").await;
        crate::time::spawn_blocking(move || replication.tick_all())
            .await
            .unwrap();
        // A held connection keeps the restore waiting in its drain.
        let held = db.pool().get().await.unwrap();
        let cancelled = tokio::time::timeout(Duration::from_millis(60), fleet.restore(&key, None))
            .await
            .is_err();
        assert!(cancelled, "the restore was still draining");
        drop(held);
        drop(db);
        for _ in 0..500 {
            if fleet.stats().draining == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(fleet.stats().draining, 0, "the restore ran to the end");
        assert_eq!(
            releases.0.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the restore went on past its drain"
        );
        let reopened = tokio::time::timeout(NOT_STUCK, fleet.open(&key))
            .await
            .expect("open must not hang after a cancelled restore")
            .unwrap();
        assert_eq!(count(&reopened).await, 1);
        drop(reopened);
        tokio::time::timeout(NOT_STUCK, fleet.close_all())
            .await
            .expect("close_all returns");
    }

    #[tokio::test]
    async fn a_concurrent_restore_keeps_the_winners_file() {
        let tmp = tempfile::tempdir().unwrap();
        let replicas = tmp.path().join("replicas");
        let (old_host, _) = replicated_fleet(&tmp.path().join("old"), &replicas, false);
        let key = old_host.key_for("acme").unwrap();
        let db = old_host.open(&key).await.unwrap();
        exec(&db, "CREATE TABLE notes (body TEXT NOT NULL)").await;
        drop(db);
        old_host.close_all().await;

        let (new_host, replication) = replicated_fleet(&tmp.path().join("new"), &replicas, true);
        let path = new_host.path_of(&key);
        // Another process published first.
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"the winner's file").unwrap();
        let restored = crate::time::spawn_blocking({
            let (key, path) = (key.clone(), path.clone());
            move || replication.restore_missing(&key, &path)
        })
        .await
        .unwrap();
        assert_eq!(restored, Ok(true));
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"the winner's file",
            "not replaced"
        );
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(leftovers.len(), 1, "no staging left: {leftovers:?}");
    }

    #[tokio::test]
    async fn restore_refuses_a_database_another_process_has_open() {
        use diesel::connection::SimpleConnection as _;
        let tmp = tempfile::tempdir().unwrap();
        let (fleet, _) =
            replicated_fleet(&tmp.path().join("a"), &tmp.path().join("replicas"), false);
        let key = fleet.key_for("acme").unwrap();
        let db = fleet.open(&key).await.unwrap();
        exec(&db, "CREATE TABLE notes (body TEXT NOT NULL)").await;
        let path = db.path().to_path_buf();
        drop(db);
        fleet.close(&key).await;
        let mut other = crate::db::establish_sqlite_migration_connection(&format!(
            "sqlite://{}",
            path.display()
        ))
        .unwrap();
        other.batch_execute("SELECT 1 FROM notes").unwrap();
        let err = fleet.restore(&key, None).await.unwrap_err();
        assert!(matches!(err, FleetError::InUseElsewhere { .. }), "{err}");
        drop(other);
        fleet.restore(&key, None).await.unwrap();
        assert_eq!(count(&fleet.open(&key).await.unwrap()).await, 0);
    }

    #[tokio::test]
    async fn shutting_the_fleet_down_stops_the_replication_thread() {
        let tmp = tempfile::tempdir().unwrap();
        let (fleet, replication) =
            replicated_fleet(&tmp.path().join("a"), &tmp.path().join("replicas"), false);
        fleet.open_for("acme").await.unwrap();
        let thread = replication.spawn_loop().unwrap();
        let shutdown = CancellationToken::new();
        let maintenance = fleet.spawn_maintenance(shutdown.clone());
        shutdown.cancel();
        maintenance.await.unwrap();
        assert_eq!(fleet.stats().open, 0);
        // The loop sees the stop flag within one poll.
        let joined = crate::time::spawn_blocking(move || thread.join())
            .await
            .unwrap();
        assert!(joined.is_ok());
    }

    #[tokio::test]
    async fn the_loop_ticks_until_stopped() {
        let tmp = tempfile::tempdir().unwrap();
        let (fleet, replication) =
            replicated_fleet(&tmp.path().join("a"), &tmp.path().join("replicas"), false);
        fleet.open_for("acme").await.unwrap();
        let handle = replication.spawn_loop().unwrap();
        for _ in 0..100 {
            let output = replication.indicator().check().await;
            if output.details.contains_key("worst_lag_seconds")
                && output.details["failing"] == 0
                && tmp
                    .path()
                    .join("replicas/app/test/fleet/tenant/acme")
                    .exists()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            tmp.path()
                .join("replicas/app/test/fleet/tenant/acme")
                .exists()
        );
        replication.stop();
        handle.join().unwrap();
        fleet.close_all().await;
    }

    /// A fleet whose destination is down, with a short final flush.
    fn outage_fleet(
        tmp: &Path,
        final_flush: Duration,
    ) -> (DatabaseFleet, Arc<FleetReplication>, Arc<Flaky>) {
        let fleet = DatabaseFleet::builder(DatabaseFleetConfig {
            mode: FleetMode::Tenant,
            root: tmp.join("a").display().to_string(),
            path: None,
            max_open: 8,
            pool_size: 2,
            create_on_demand: Some(true),
            idle_close_secs: 0,
            restore_missing: false,
            ownership: None,
            replica_retention_after_delete: std::time::Duration::ZERO,
            restore_check_sample: 0,
            replication_workers: 4,
            min_residency: std::time::Duration::from_secs(2),
        })
        .build()
        .unwrap();
        let flaky = Arc::new(Flaky {
            inner: FileDestination::new(tmp.join("replicas")).unwrap(),
            down: AtomicBool::new(true),
        });
        let replication = Arc::new(
            FleetReplication::new(
                Arc::clone(&flaky) as Arc<dyn ReplicaDestination>,
                settings(),
                Duration::from_secs(30),
                false,
                Arc::new(crate::time::SystemClock),
            )
            .with_final_flush(final_flush),
        );
        assert!(fleet.install_replication(Arc::clone(&replication)).is_ok());
        (fleet, replication, flaky)
    }

    /// Write a row, close the database and leave its replicator parked.
    async fn park_one(fleet: &DatabaseFleet) -> FleetDbKey {
        let key = fleet.key_for("acme").unwrap();
        let db = fleet.open(&key).await.unwrap();
        exec(&db, "CREATE TABLE notes (body TEXT NOT NULL)").await;
        exec(&db, "INSERT INTO notes VALUES ('unshipped')").await;
        drop(db);
        fleet.close(&key).await;
        key
    }

    /// Hold the replicator lock of `replica` on another thread, as a tick
    /// in destination I/O does, until the returned sender drops.
    fn hold_replicator(replica: Arc<ActiveReplica>) -> std::sync::mpsc::Sender<()> {
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            let _guard = replica.replicator.lock().unwrap();
            held_tx.send(()).unwrap();
            let _ = release_rx.recv();
        });
        held_rx.recv().unwrap();
        release_tx
    }

    #[tokio::test]
    async fn a_reopen_under_the_same_epoch_does_not_wait_for_a_busy_replicator() {
        let tmp = tempfile::tempdir().unwrap();
        let (fleet, replication, _flaky) = outage_fleet(tmp.path(), Duration::from_secs(5));
        let key = park_one(&fleet).await;
        let replica = replication.parked().get(&key).cloned().unwrap();
        let release = hold_replicator(Arc::clone(&replica));
        let opened = tokio::time::timeout(Duration::from_secs(10), fleet.open(&key)).await;
        drop(release);
        assert!(opened.expect("the open does not wait for the tick").is_ok());
        let active = replication.active().get(&key).cloned().unwrap();
        assert!(
            Arc::ptr_eq(&active, &replica),
            "the parked replicator is reused"
        );
    }

    #[tokio::test]
    async fn a_reopen_under_a_new_epoch_waits_for_a_busy_replicator_only_so_long() {
        let tmp = tempfile::tempdir().unwrap();
        let (fleet, replication, _flaky) = outage_fleet(tmp.path(), Duration::from_secs(5));
        let key = park_one(&fleet).await;
        let replica = replication.parked().get(&key).cloned().unwrap();
        // The database closed under epoch 7; it reopens with no epoch.
        replica.epoch.store(7, Ordering::Release);
        let release = hold_replicator(Arc::clone(&replica));
        let opened = tokio::time::timeout(Duration::from_secs(10), fleet.open(&key)).await;
        drop(release);
        assert!(
            matches!(opened, Ok(Err(FleetError::Unavailable { .. }))),
            "the open is refused, so the caller retries"
        );
        assert!(
            replication.parked().contains_key(&key),
            "the replicator stays parked and keeps shipping"
        );
        // Free again: the open takes the parked replicator.
        fleet.open(&key).await.unwrap();
        let active = replication.active().get(&key).cloned().unwrap();
        assert!(Arc::ptr_eq(&active, &replica));
        assert_eq!(replica.epoch.load(Ordering::Acquire), 0);
    }

    fn recover_after(flaky: &Arc<Flaky>, delay: Duration) {
        let flaky = Arc::clone(flaky);
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            flaky.down.store(false, Ordering::SeqCst);
        });
    }

    #[tokio::test]
    async fn a_reopened_parked_replicator_keeps_shipping() {
        let tmp = tempfile::tempdir().unwrap();
        let (fleet, replication, flaky) = outage_fleet(tmp.path(), Duration::from_secs(5));
        let key = park_one(&fleet).await;
        assert_eq!(replication.closing(), 1);
        // A request reopens it: the parked replicator goes back to work.
        let db = fleet.open(&key).await.unwrap();
        assert_eq!(replication.closing(), 0);
        flaky.down.store(false, Ordering::SeqCst);
        tick(&replication).await;
        let health = replication.health();
        assert_eq!(health.details["failing"], 0, "{health:?}");
        assert_eq!(health.details["pending_bytes"], 0, "{health:?}");
        drop(db);
        fleet.close_all().await;
    }

    #[tokio::test]
    async fn the_final_flush_ships_once_the_destination_recovers() {
        let tmp = tempfile::tempdir().unwrap();
        let (fleet, replication, flaky) = outage_fleet(tmp.path(), Duration::from_secs(5));
        park_one(&fleet).await;
        assert_eq!(replication.closing(), 1);
        recover_after(&flaky, Duration::from_millis(200));
        let left = crate::time::spawn_blocking({
            let replication = Arc::clone(&replication);
            move || replication.flush_closing(Duration::from_secs(5))
        })
        .await
        .unwrap();
        assert!(left.is_empty(), "{left:?}");
        assert_eq!(replication.closing(), 0);
    }

    #[tokio::test]
    async fn the_final_flush_names_what_it_could_not_ship() {
        let tmp = tempfile::tempdir().unwrap();
        let (fleet, replication, _flaky) = outage_fleet(tmp.path(), Duration::from_secs(5));
        let key = park_one(&fleet).await;
        let left = crate::time::spawn_blocking({
            let replication = Arc::clone(&replication);
            move || replication.flush_closing(Duration::from_millis(300))
        })
        .await
        .unwrap();
        assert_eq!(left, vec![key.name()]);
        assert_eq!(replication.closing(), 1);
        let health = replication.indicator().check().await;
        assert_eq!(health.details["unshipped"], serde_json::json!([key.name()]));
    }

    #[tokio::test]
    async fn shutdown_keeps_shipping_until_the_destination_recovers() {
        let tmp = tempfile::tempdir().unwrap();
        let (fleet, replication, flaky) = outage_fleet(tmp.path(), Duration::from_secs(5));
        let shutdown = CancellationToken::new();
        let maintenance = fleet.spawn_maintenance(shutdown.clone());
        let key = fleet.key_for("acme").unwrap();
        let db = fleet.open(&key).await.unwrap();
        exec(&db, "CREATE TABLE notes (body TEXT NOT NULL)").await;
        exec(&db, "INSERT INTO notes VALUES ('written before shutdown')").await;
        drop(db);
        recover_after(&flaky, Duration::from_millis(300));
        shutdown.cancel();
        maintenance.await.unwrap();
        assert_eq!(replication.closing(), 0, "everything shipped");
    }

    #[tokio::test]
    async fn shutdown_reports_a_destination_that_stays_down() {
        let tmp = tempfile::tempdir().unwrap();
        let (fleet, replication, _flaky) = outage_fleet(tmp.path(), Duration::from_secs(60));
        // The shutdown shortens the deadline to what it has left.
        replication.set_final_flush(Duration::from_millis(300));
        let shutdown = CancellationToken::new();
        let maintenance = fleet.spawn_maintenance(shutdown.clone());
        let key = fleet.key_for("acme").unwrap();
        let db = fleet.open(&key).await.unwrap();
        exec(&db, "CREATE TABLE notes (body TEXT NOT NULL)").await;
        drop(db);
        shutdown.cancel();
        maintenance.await.unwrap();
        assert_eq!(replication.closing(), 1);
        let health = replication.indicator().check().await;
        assert_eq!(health.details["unshipped"], serde_json::json!([key.name()]));
    }

    #[tokio::test]
    async fn an_expired_flush_starts_no_tick() {
        let tmp = tempfile::tempdir().unwrap();
        let (fleet, replication, flaky) = outage_fleet(tmp.path(), Duration::from_secs(5));
        let key = park_one(&fleet).await;
        flaky.down.store(false, Ordering::SeqCst);
        let left = crate::time::spawn_blocking({
            let replication = Arc::clone(&replication);
            move || replication.flush_closing(Duration::ZERO)
        })
        .await
        .unwrap();
        assert_eq!(left, vec![key.name()], "no time, so no tick");
        assert_eq!(replication.closing(), 1);
    }

    #[tokio::test]
    async fn a_pass_past_its_cutoff_ships_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let (fleet, replication, flaky) = outage_fleet(tmp.path(), Duration::from_secs(5));
        park_one(&fleet).await;
        flaky.down.store(false, Ordering::SeqCst);
        crate::time::spawn_blocking({
            let replication = Arc::clone(&replication);
            // The cutoff is already behind us when the pass checks it.
            move || replication.tick_pass(&|| Some(std::time::Instant::now()))
        })
        .await
        .unwrap();
        assert_eq!(replication.closing(), 1, "the pass stopped at the cutoff");
        crate::time::spawn_blocking({
            let replication = Arc::clone(&replication);
            move || replication.tick_pass(&|| None)
        })
        .await
        .unwrap();
        assert_eq!(replication.closing(), 0, "an open pass ships");
    }

    #[tokio::test]
    async fn a_close_after_the_shutdown_cutoff_parks_without_shipping() {
        let tmp = tempfile::tempdir().unwrap();
        let (fleet, replication, flaky) = outage_fleet(tmp.path(), Duration::from_secs(5));
        flaky.down.store(false, Ordering::SeqCst);
        let key = fleet.key_for("acme").unwrap();
        let db = fleet.open(&key).await.unwrap();
        exec(&db, "CREATE TABLE notes (body TEXT NOT NULL)").await;
        drop(db);
        // The destination is up, but the shutdown time is already gone.
        replication.set_shutdown_cutoff(Some(std::time::Instant::now()));
        fleet.close(&key).await;
        assert_eq!(replication.closing(), 1, "parked, not shipped");
    }

    #[tokio::test]
    async fn the_background_loop_stops_ticking_at_the_shutdown_cutoff() {
        let tmp = tempfile::tempdir().unwrap();
        let (fleet, replication, flaky) = outage_fleet(tmp.path(), Duration::from_secs(5));
        park_one(&fleet).await;
        flaky.down.store(false, Ordering::SeqCst);
        replication.set_shutdown_cutoff(Some(std::time::Instant::now()));
        crate::time::spawn_blocking({
            let replication = Arc::clone(&replication);
            move || replication.tick_all()
        })
        .await
        .unwrap();
        assert_eq!(replication.closing(), 1, "the loop ticks nothing after it");
        replication.set_shutdown_cutoff(None);
        crate::time::spawn_blocking({
            let replication = Arc::clone(&replication);
            move || replication.tick_all()
        })
        .await
        .unwrap();
        assert_eq!(replication.closing(), 0);
    }

    #[tokio::test]
    async fn maintenance_keeps_a_cutoff_the_caller_set_before_it_woke() {
        let tmp = tempfile::tempdir().unwrap();
        // The final flush alone would allow a minute.
        let (fleet, replication, _flaky) = outage_fleet(tmp.path(), Duration::from_secs(60));
        let shutdown = CancellationToken::new();
        let maintenance = fleet.spawn_maintenance(shutdown.clone());
        let key = fleet.key_for("acme").unwrap();
        let db = fleet.open(&key).await.unwrap();
        exec(&db, "CREATE TABLE notes (body TEXT NOT NULL)").await;
        drop(db);
        // The caller's deadline has already passed when maintenance wakes.
        replication.set_shutdown_cutoff(Some(std::time::Instant::now()));
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(10), maintenance)
            .await
            .expect("the caller's cutoff bounds the flush")
            .unwrap();
        assert_eq!(replication.closing(), 1);
    }

    #[tokio::test]
    async fn the_ship_loop_reads_the_cutoff_on_every_step() {
        let tmp = tempfile::tempdir().unwrap();
        let (fleet, replication, flaky) = outage_fleet(tmp.path(), Duration::from_secs(5));
        let key = park_one(&fleet).await;
        flaky.down.store(false, Ordering::SeqCst);
        let reads = std::sync::atomic::AtomicUsize::new(0);
        let shipped = {
            let replica = replication.parked().get(&key).cloned().unwrap();
            let mut replicator = replica.replicator.lock().unwrap();
            // No cutoff at the first read, a passed one after it.
            ship_until_caught_up(&mut replicator, &|| {
                if reads.fetch_add(1, Ordering::SeqCst) == 0 {
                    None
                } else {
                    Some(std::time::Instant::now())
                }
            })
        };
        assert!(reads.load(Ordering::SeqCst) >= 1);
        // The destination is up: it either caught up on the first step, or
        // it stopped at the cutoff it read on the next one.
        if let Err(error) = shipped {
            assert!(error.contains("ran out of time"), "{error}");
        }
    }

    #[tokio::test]
    async fn a_running_pass_reads_a_cutoff_installed_after_it_began() {
        let tmp = tempfile::tempdir().unwrap();
        let (fleet, replication, flaky) = outage_fleet(tmp.path(), Duration::from_secs(5));
        park_one(&fleet).await;
        flaky.down.store(false, Ordering::SeqCst);
        let reads = std::sync::atomic::AtomicUsize::new(0);
        crate::time::spawn_blocking({
            let replication = Arc::clone(&replication);
            move || {
                // No cutoff when the pass starts; a passed one right after.
                replication.tick_pass(&|| {
                    if reads.fetch_add(1, Ordering::SeqCst) == 0 {
                        None
                    } else {
                        Some(std::time::Instant::now())
                    }
                });
            }
        })
        .await
        .unwrap();
        // The pass read the cutoff again before it shipped the parked replica.
        assert_eq!(replication.closing(), 1);
    }

    #[tokio::test]
    async fn a_pass_waiting_on_a_replica_lock_gives_up_at_a_new_cutoff() {
        let tmp = tempfile::tempdir().unwrap();
        let (fleet, replication, flaky) = outage_fleet(tmp.path(), Duration::from_secs(5));
        let key = park_one(&fleet).await;
        flaky.down.store(false, Ordering::SeqCst);
        // Another thread holds the replica for three seconds.
        let replica = replication.parked().get(&key).cloned().unwrap();
        let (held_tx, held) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _guard = replica.replicator.lock().unwrap();
            held_tx.send(()).unwrap();
            std::thread::sleep(Duration::from_secs(3));
        });
        held.recv().unwrap();
        let reads = std::sync::atomic::AtomicUsize::new(0);
        let began = std::time::Instant::now();
        crate::time::spawn_blocking({
            let replication = Arc::clone(&replication);
            move || {
                // No cutoff at first, a passed one from the second read on.
                replication.tick_pass(&|| {
                    if reads.fetch_add(1, Ordering::SeqCst) == 0 {
                        None
                    } else {
                        Some(std::time::Instant::now())
                    }
                });
            }
        })
        .await
        .unwrap();
        assert!(
            began.elapsed() < Duration::from_secs(2),
            "the pass must not wait out the lock: {:?}",
            began.elapsed()
        );
        holder.join().unwrap();
        assert_eq!(replication.closing(), 1, "nothing shipped");
    }

    #[tokio::test]
    async fn a_close_waiting_on_a_replica_lock_gives_up_at_the_cutoff() {
        let tmp = tempfile::tempdir().unwrap();
        let (fleet, replication, flaky) = outage_fleet(tmp.path(), Duration::from_secs(5));
        flaky.down.store(false, Ordering::SeqCst);
        let key = fleet.key_for("acme").unwrap();
        let db = fleet.open(&key).await.unwrap();
        exec(&db, "CREATE TABLE notes (body TEXT NOT NULL)").await;
        drop(db);
        // The loop thread holds the replica for three seconds.
        let replica = replication.active().get(&key).cloned().unwrap();
        let (held_tx, held) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _guard = replica.replicator.lock().unwrap();
            held_tx.send(()).unwrap();
            std::thread::sleep(Duration::from_secs(3));
        });
        held.recv().unwrap();
        replication.set_shutdown_cutoff(Some(std::time::Instant::now()));
        let began = std::time::Instant::now();
        fleet.close(&key).await;
        assert!(
            began.elapsed() < Duration::from_secs(2),
            "the close must not wait out the lock: {:?}",
            began.elapsed()
        );
        holder.join().unwrap();
        assert_eq!(replication.closing(), 1, "parked, not shipped");
    }

    // --- Many busy databases per process (Murmuration FINDINGS 1 and 6) ---

    /// A destination that counts the writes that run at once. Each write
    /// takes `delay`. With `meet`, the first write waits (up to
    /// [`NOT_STUCK`]) until a second write starts, so two workers always
    /// overlap.
    struct Slow {
        inner: FileDestination,
        delay: Duration,
        meet: bool,
        in_flight: std::sync::atomic::AtomicUsize,
        most: std::sync::atomic::AtomicUsize,
    }

    impl Slow {
        fn new(dir: &Path, delay: Duration, meet: bool) -> Self {
            Self {
                inner: FileDestination::new(dir).unwrap(),
                delay,
                meet,
                in_flight: std::sync::atomic::AtomicUsize::new(0),
                most: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        fn write<T>(&self, op: impl FnOnce() -> T) -> T {
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.most.fetch_max(now, Ordering::SeqCst);
            let began = std::time::Instant::now();
            while self.meet && self.most.load(Ordering::SeqCst) < 2 && began.elapsed() < NOT_STUCK {
                std::thread::sleep(Duration::from_millis(1));
            }
            std::thread::sleep(self.delay);
            let out = op();
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            out
        }
    }

    impl ReplicaDestination for Slow {
        fn describe(&self) -> String {
            "slow".to_owned()
        }
        fn put(&self, key: &str, body: &[u8]) -> Result<(), crate::replication::DestinationError> {
            self.write(|| self.inner.put(key, body))
        }
        fn put_file(
            &self,
            key: &str,
            path: &Path,
        ) -> Result<(), crate::replication::DestinationError> {
            self.write(|| self.inner.put_file(key, path))
        }
        fn get(&self, key: &str) -> Result<Vec<u8>, crate::replication::DestinationError> {
            self.inner.get(key)
        }
        fn get_to_file(
            &self,
            key: &str,
            path: &Path,
        ) -> Result<(), crate::replication::DestinationError> {
            self.inner.get_to_file(key, path)
        }
        fn list(&self, prefix: &str) -> Result<Vec<String>, crate::replication::DestinationError> {
            self.inner.list(prefix)
        }
        fn delete(&self, key: &str) -> Result<(), crate::replication::DestinationError> {
            self.inner.delete(key)
        }
    }

    /// A destination that stops the first write after `arm` until the
    /// returned sender drops. It sends the key of that write.
    struct Gate {
        inner: FileDestination,
        armed: AtomicBool,
        reached: Mutex<Option<std::sync::mpsc::Sender<String>>>,
        release: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    }

    impl Gate {
        fn new(dir: &Path) -> Self {
            Self {
                inner: FileDestination::new(dir).unwrap(),
                armed: AtomicBool::new(false),
                reached: Mutex::new(None),
                release: Mutex::new(None),
            }
        }

        fn arm(
            &self,
        ) -> (
            std::sync::mpsc::Receiver<String>,
            std::sync::mpsc::Sender<()>,
        ) {
            let (reached_tx, reached_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            *self.reached.lock().unwrap() = Some(reached_tx);
            *self.release.lock().unwrap() = Some(release_rx);
            self.armed.store(true, Ordering::SeqCst);
            (reached_rx, release_tx)
        }

        fn stop(&self, key: &str) {
            if !self.armed.swap(false, Ordering::SeqCst) {
                return;
            }
            let reached = self.reached.lock().unwrap().take();
            if let Some(reached) = reached {
                let _ = reached.send(key.to_owned());
            }
            let release = self.release.lock().unwrap().take();
            if let Some(release) = release {
                let _ = release.recv();
            }
        }
    }

    impl ReplicaDestination for Gate {
        fn describe(&self) -> String {
            "gate".to_owned()
        }
        fn put(&self, key: &str, body: &[u8]) -> Result<(), crate::replication::DestinationError> {
            self.stop(key);
            self.inner.put(key, body)
        }
        fn put_file(
            &self,
            key: &str,
            path: &Path,
        ) -> Result<(), crate::replication::DestinationError> {
            self.stop(key);
            self.inner.put_file(key, path)
        }
        fn get(&self, key: &str) -> Result<Vec<u8>, crate::replication::DestinationError> {
            self.inner.get(key)
        }
        fn get_to_file(
            &self,
            key: &str,
            path: &Path,
        ) -> Result<(), crate::replication::DestinationError> {
            self.inner.get_to_file(key, path)
        }
        fn list(&self, prefix: &str) -> Result<Vec<String>, crate::replication::DestinationError> {
            self.inner.list(prefix)
        }
        fn delete(&self, key: &str) -> Result<(), crate::replication::DestinationError> {
            self.inner.delete(key)
        }
    }

    /// A fleet that ships to `destination` with `workers` ship workers.
    fn fleet_with(
        tmp: &Path,
        destination: Arc<dyn ReplicaDestination>,
        workers: usize,
    ) -> (DatabaseFleet, Arc<FleetReplication>) {
        let fleet = DatabaseFleet::builder(DatabaseFleetConfig {
            mode: FleetMode::Tenant,
            root: tmp.join("a").display().to_string(),
            path: None,
            max_open: 16,
            pool_size: 1,
            create_on_demand: Some(true),
            idle_close_secs: 0,
            restore_missing: false,
            ownership: None,
            replica_retention_after_delete: std::time::Duration::ZERO,
            restore_check_sample: 0,
            replication_workers: 4,
            min_residency: std::time::Duration::from_secs(2),
        })
        .build()
        .unwrap();
        let replication = Arc::new(
            FleetReplication::new(
                destination,
                settings(),
                Duration::from_secs(30),
                false,
                Arc::new(crate::time::SystemClock),
            )
            .with_ship_workers(workers),
        );
        assert!(fleet.install_replication(Arc::clone(&replication)).is_ok());
        (fleet, replication)
    }

    const TENANTS: [&str; 8] = ["t0", "t1", "t2", "t3", "t4", "t5", "t6", "t7"];

    /// Open each tenant and run `sql` on it.
    async fn write_each(fleet: &DatabaseFleet, tenants: &[&str], sql: &str) {
        for tenant in tenants {
            let db = fleet.open_for(tenant).await.unwrap();
            exec(&db, sql).await;
        }
    }

    #[test]
    fn a_fleet_ships_with_a_small_pool_by_default() {
        let tmp = tempfile::tempdir().unwrap();
        let replication = FleetReplication::new(
            Arc::new(FileDestination::new(tmp.path()).unwrap()),
            settings(),
            Duration::from_secs(30),
            false,
            Arc::new(crate::time::SystemClock),
        );
        assert_eq!(replication.ship_workers(), DEFAULT_SHIP_WORKERS);
        assert_eq!(replication.with_ship_workers(0).ship_workers(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_pass_ships_databases_in_parallel() {
        for (workers, parallel) in [(1, false), (4, true)] {
            let tmp = tempfile::tempdir().unwrap();
            let slow = Arc::new(Slow::new(
                &tmp.path().join("replicas"),
                Duration::from_millis(5),
                parallel,
            ));
            let (fleet, replication) = fleet_with(
                tmp.path(),
                Arc::clone(&slow) as Arc<dyn ReplicaDestination>,
                workers,
            );
            write_each(&fleet, &TENANTS, "CREATE TABLE notes (body TEXT NOT NULL)").await;
            tick(&replication).await;
            let most = slow.most.load(Ordering::SeqCst);
            if parallel {
                assert!(most >= 2, "{workers} workers shipped one at a time");
            } else {
                assert_eq!(most, 1, "one worker ships one database at a time");
            }
            fleet.close_all().await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_database_reopened_during_a_pass_is_replicated_at_once() {
        let tmp = tempfile::tempdir().unwrap();
        let gate = Arc::new(Gate::new(&tmp.path().join("replicas")));
        let (fleet, replication) = fleet_with(
            tmp.path(),
            Arc::clone(&gate) as Arc<dyn ReplicaDestination>,
            1,
        );
        let tenants = &TENANTS[..3];
        write_each(&fleet, tenants, "CREATE TABLE notes (body TEXT NOT NULL)").await;
        tick(&replication).await;
        write_each(&fleet, tenants, "INSERT INTO notes VALUES ('next')").await;

        // The pass stops in the first database it ships. The others wait.
        let (reached, release) = gate.arm();
        let pass = crate::time::spawn_blocking({
            let replication = Arc::clone(&replication);
            move || replication.tick_all()
        });
        let stopped = crate::time::spawn_blocking(move || reached.recv_timeout(NOT_STUCK))
            .await
            .unwrap()
            .expect("the pass ships");
        let waiting = tenants
            .iter()
            .find(|tenant| !stopped.contains(&format!("/tenant/{tenant}/")))
            .unwrap();

        // Close and reopen a database that the pass did not reach yet.
        let key = fleet.key_for(waiting).unwrap();
        fleet.close(&key).await;
        let _db = fleet.open(&key).await.unwrap();
        assert_eq!(
            (replication.unowned(), replication.replicating()),
            (0, 3),
            "no other process has this volume: this process owns every database"
        );

        drop(release);
        pass.await.unwrap();
        fleet.close_all().await;
    }

    #[tokio::test]
    async fn health_reports_the_pass_time_and_the_ship_workers() {
        let tmp = tempfile::tempdir().unwrap();
        let (fleet, replication) =
            replicated_fleet(&tmp.path().join("a"), &tmp.path().join("replicas"), false);
        fleet.open_for("acme").await.unwrap();
        tick(&replication).await;
        let output = replication.indicator().check().await;
        assert_eq!(output.details["ship_workers"], DEFAULT_SHIP_WORKERS);
        assert!(
            output.details["last_pass_ms"].is_u64(),
            "{:?}",
            output.details
        );
        fleet.close_all().await;
    }

    #[test]
    fn the_loop_starts_a_pass_once_per_interval() {
        let interval = Duration::from_secs(1);
        assert_eq!(pause_after(interval, Duration::ZERO), interval);
        assert_eq!(
            pause_after(interval, Duration::from_millis(300)),
            Duration::from_millis(700)
        );
        // A pass that overran starts the next one at once.
        assert_eq!(
            pause_after(interval, Duration::from_secs(3)),
            Duration::ZERO
        );
    }

    #[test]
    fn a_pass_takes_the_most_lagging_database_first() {
        let at = |secs| DateTime::from_timestamp(secs, 0).unwrap();
        let order = oldest_first(vec![
            (FleetDbKey::Slot(1), at(30)),
            (FleetDbKey::Slot(2), at(10)),
            (FleetDbKey::Slot(3), at(20)),
        ]);
        assert_eq!(
            order,
            vec![
                FleetDbKey::Slot(2),
                FleetDbKey::Slot(3),
                FleetDbKey::Slot(1)
            ]
        );
    }

    proptest::proptest! {
        #[test]
        fn a_pass_order_is_every_database_once_oldest_first(
            times in proptest::collection::vec(0_i64..1_000, 0..64)
        ) {
            let at = |secs| DateTime::from_timestamp(secs, 0).unwrap();
            let entries: Vec<(FleetDbKey, DateTime<Utc>)> = times
                .iter()
                .enumerate()
                .map(|(slot, secs)| (FleetDbKey::Slot(u16::try_from(slot).unwrap()), at(*secs)))
                .collect();
            let order = oldest_first(entries.clone());
            let mut sorted = order.clone();
            sorted.sort();
            let mut keys: Vec<FleetDbKey> = entries.iter().map(|(key, _)| key.clone()).collect();
            keys.sort();
            proptest::prop_assert_eq!(sorted, keys, "every key once");
            let when: HashMap<FleetDbKey, DateTime<Utc>> = entries.into_iter().collect();
            for pair in order.windows(2) {
                proptest::prop_assert!(when[&pair[0]] <= when[&pair[1]]);
            }
        }

        #[test]
        fn every_worker_count_visits_each_key_once(
            keys in 0_usize..40,
            workers in 1_usize..9,
        ) {
            let keys: Vec<FleetDbKey> = (0..keys)
                .map(|slot| FleetDbKey::Slot(u16::try_from(slot).unwrap()))
                .collect();
            let seen = Mutex::new(Vec::new());
            for_each_on(workers, &keys, |key| seen.lock().unwrap().push(key.clone()));
            let mut seen = seen.into_inner().unwrap();
            seen.sort();
            let mut expected = keys;
            expected.sort();
            proptest::prop_assert_eq!(seen, expected);
        }
    }
}
