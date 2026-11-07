//! The replica steps of the fleet core.
//!
//! Tombstones, restores from the replica, purges and ships. They work when
//! replication is installed. Without it each step does nothing. Without the
//! `sqlite` feature, the no-op steps in `fleet.rs` take their place.

use super::sqlite::restore_in_place;
use super::{
    Arc, CancellationToken, DatabaseFleet, Duration, FleetDbKey, FleetEngine, FleetError, Path,
    ShipMode, ShipReport, SqliteEngine, check_contained,
};
use crate::db::fleet_tombstones::{ControlConnection, FleetTombstones, TombstoneError};

/// Where the restore check writes its scratch files, under the root.
const RESTORE_CHECK_DIR: &str = "restore-check";

/// A scratch file older than this is from a check that did not end.
const SCRATCH_STALE: Duration = Duration::from_hours(24);

/// How often the replica lifecycle task retries the purge of deleted
/// databases' replicas (issue #3235).
const PURGE_PASS_INTERVAL: Duration = Duration::from_secs(60);

/// Tombstone rows one purge pass reads.
const PURGE_BATCH: usize = 256;

/// The tombstone row a delete writes before it removes the files.
pub(super) struct DeleteRecord {
    tombstones: Option<Arc<FleetTombstones>>,
    /// Names this delete's pending row: a refusal drops only its own.
    owner: String,
    retention: Duration,
    name: String,
}

impl DeleteRecord {
    fn unrecorded(&self, error: &TombstoneError) -> FleetError {
        FleetError::Unavailable {
            name: self.name.clone(),
            detail: format!("could not record the replica tombstone: {error}"),
        }
    }

    /// A control connection, taken before the path lock: the lock then
    /// waits for one write only.
    pub(super) async fn checkout(&self) -> Result<Option<ControlConnection>, FleetError> {
        match &self.tombstones {
            Some(tombstones) => Ok(Some(
                tombstones
                    .checkout()
                    .await
                    .map_err(|e| self.unrecorded(&e))?,
            )),
            None => Ok(None),
        }
    }

    /// Writes the pending row. Before any file goes: a crash after the
    /// removal must not leave the replica with nothing that blocks its
    /// restore.
    pub(super) async fn record(
        &self,
        control: &mut Option<ControlConnection>,
        present: bool,
    ) -> Result<(), FleetError> {
        if let (Some(tombstones), Some(control)) = (&self.tombstones, control) {
            tombstones
                .record_on(control, &self.name, self.retention, present, &self.owner)
                .await
                .map_err(|e| self.unrecorded(&e))?;
        }
        Ok(())
    }

    /// The delete removed nothing: drop this delete's pending row.
    pub(super) async fn drop_pending(&self, key: &FleetDbKey) {
        if let Some(tombstones) = &self.tombstones
            && let Err(error) = tombstones
                .remove_pending(&self.name, Some(&self.owner))
                .await
        {
            // The retry task drops it later: the file is still there.
            tracing::warn!(database = %key, %error, "fleet delete: could not drop the pending tombstone");
        }
    }
}

impl<E: FleetEngine> DatabaseFleet<E> {
    /// Replicate every database this fleet opens from now on (ADR 0019 §5,
    /// ADR 0040 §5). The engine's [`ShipMode`] decides how:
    ///
    /// - `Continuous`: `SQLite` WAL shipping. Databases open with
    ///   `wal_autocheckpoint = 0`, and `replication` is their only
    ///   checkpointer.
    /// - `Generations`: core ships a generation of each open database every
    ///   `every`, and at close.
    /// - `Rebuild`: nothing ships. The tombstones still apply.
    ///
    /// Install before the first open, once.
    ///
    /// # Errors
    ///
    /// Gives `replication` back when a replication is already installed.
    #[allow(
        clippy::needless_pass_by_value,
        reason = "the error gives the replication back, as the earlier API did"
    )]
    pub fn install_replication(
        &self,
        replication: Arc<crate::db::fleet_replication::FleetReplication>,
    ) -> Result<(), Arc<crate::db::fleet_replication::FleetReplication>> {
        let mode = self.inner.engine.ship_mode();
        self.inner.replication.set(Arc::clone(&replication))?;
        replication.set_ship_info(crate::db::fleet_replication::ShipInfo {
            mode: mode.name(),
            every: mode.every(),
        });
        match mode {
            ShipMode::Continuous(shipper) => {
                // The trait is sealed: its one shipper is the `SQLite` WAL.
                let sqlite =
                    (self as &dyn std::any::Any).downcast_ref::<DatabaseFleet<SqliteEngine>>();
                if let Some(sqlite) = sqlite {
                    sqlite.attach_wal_shipping(Arc::clone(&replication));
                } else {
                    tracing::error!(
                        shipper = shipper.name(),
                        "fleet replication: core drives no continuous shipper for this engine; nothing ships"
                    );
                }
            }
            ShipMode::Generations { .. } => {
                // A process that died during a ship leaves its scratch.
                sweep_scratch(
                    &super::generations::ship_scratch(self.root()),
                    replication.now(),
                );
                let every = mode.every().unwrap_or(Duration::from_secs(1));
                self.add_lifecycle(Arc::new(super::generations::GenerationShipper::new(
                    self,
                    Arc::clone(&replication),
                    every,
                )));
            }
            ShipMode::Rebuild => {}
        }
        self.wire_fence_hook();
        Ok(())
    }

    /// The RPO of the replica. `None` with no replica, or in `rebuild` mode.
    pub(super) fn rpo(&self) -> Option<Duration> {
        self.replication().and_then(|replication| replication.rpo())
    }

    /// The installed replication, if any.
    #[must_use]
    pub fn replication(&self) -> Option<&Arc<crate::db::fleet_replication::FleetReplication>> {
        self.inner.replication.get()
    }

    /// The replica of `key`: the destination and its key prefix.
    pub(super) fn replica_base(&self, key: &FleetDbKey) -> Option<super::ReplicaBase> {
        self.replication().map(|replication| super::ReplicaBase {
            destination: Arc::clone(replication.destination()),
            root: replication.root_for(key),
            ownership: self.ownership().cloned(),
        })
    }

    /// Whether the replication has frames it did not ship yet.
    pub(super) fn unshipped(&self) -> bool {
        self.replication().is_some_and(|r| r.closing() > 0)
    }

    /// The tombstone row of a delete of `name`.
    pub(super) fn delete_record(&self, name: &str) -> DeleteRecord {
        let replication = self.replication();
        DeleteRecord {
            tombstones: replication.and_then(|r| r.tombstones()).cloned(),
            owner: replication
                .map(|r| format!("{:016x}", r.entropy().next_u64()))
                .unwrap_or_default(),
            retention: replication.map_or(Duration::ZERO, |r| r.retention_after_delete()),
            name: name.to_owned(),
        }
    }

    /// Whether a tombstone says `key` was deleted. An open does not restore
    /// such a database.
    pub(super) async fn deleted_in_replica(&self, key: &FleetDbKey) -> Result<bool, FleetError> {
        let Some(tombstones) = self.replication().and_then(|r| r.tombstones()) else {
            return Ok(false);
        };
        let row = tombstones
            .get(&key.name())
            .await
            .map_err(|error| FleetError::Restore {
                name: key.name(),
                detail: error.to_string(),
            })?;
        Ok(row.is_some())
    }

    /// With both ownership and replication: a replicator that sees a newer
    /// epoch drops this host's range, and the ownership loop closes it.
    pub(super) fn wire_fence_hook(&self) {
        let (Some(replication), Some(ownership)) = (self.replication(), self.ownership()) else {
            return;
        };
        let known = Arc::downgrade(ownership);
        replication.set_known_hook(Arc::new(move |key: &FleetDbKey| {
            known
                .upgrade()
                .map_or(0, |ownership| ownership.fence_floor(key.routing_slot()))
        }));
        let ownership = Arc::downgrade(ownership);
        replication.set_fence_hook(Arc::new(move |key: &FleetDbKey, newest: u64| {
            let newer = i64::try_from(newest)
                .ok()
                .and_then(|n| crate::lock::FencingToken::try_from(n).ok());
            if let (Some(ownership), Some(newer)) = (ownership.upgrade(), newer) {
                ownership.lose(key.routing_slot(), newer);
            }
        }));
    }

    /// Before an open under `epoch` (issue #3233). A host that gets a range
    /// back after another host held it still has its own, older file. The
    /// file records the epoch it was last opened under. When the replica
    /// holds a newer epoch of another owner, the file is replaced from the
    /// replica. The engine then stamps `epoch`, or refuses a newer stamp.
    pub(super) async fn check_epoch_stamp(
        &self,
        key: &FleetDbKey,
        path: &Path,
        epoch: crate::lock::FencingToken,
    ) -> Result<(), FleetError> {
        // No replica: nothing newer to compare with.
        if self.replication().is_none() {
            return Ok(());
        }
        let (fleet, key, path) = (self.clone(), key.clone(), path.to_path_buf());
        let name = key.name();
        crate::time::spawn_blocking(move || {
            fleet.check_epoch_stamp_blocking(&key, &path, epoch.get())
        })
        .await
        .map_err(|e| FleetError::Unavailable {
            name,
            detail: format!("epoch check task failed: {e}"),
        })?
    }

    fn check_epoch_stamp_blocking(
        &self,
        key: &FleetDbKey,
        path: &Path,
        epoch: u64,
    ) -> Result<(), FleetError> {
        let Some(replication) = self.replication() else {
            return Ok(());
        };
        let name = key.name();
        let newer = |found: u64| FleetError::NewerEpoch {
            name: name.clone(),
            epoch,
            found,
        };
        let mode = self.inner.engine.ship_mode();
        // A stamp the engine does not know: in `generations` mode it counts
        // as 0, so a newer epoch of another owner in the replica replaces the
        // file. In the other modes the file stays.
        let stamp = match self.inner.engine.epoch_stamp(key, path)? {
            Some(stamp) => stamp,
            None if matches!(mode, ShipMode::Generations { .. }) => 0,
            None => return Ok(()),
        };
        if stamp > epoch {
            return Err(newer(stamp));
        }
        if stamp == epoch {
            return Ok(());
        }
        let newest = replication
            .newest_epoch(key)
            .map_err(|detail| FleetError::Unavailable {
                name: name.clone(),
                detail: format!("could not read the replica's epochs: {detail}"),
            })?;
        // A newer owner shipped: this host is fenced. Do not replace the
        // file for an open that is refused.
        if let Some(found) = newest.filter(|newest| *newest > epoch) {
            return Err(newer(found));
        }
        // Newer than the file, and another owner's: an epoch belongs to one
        // owner, so the replica's generation of `epoch` is this host's own.
        if let Some(newest) = newest.filter(|newest| *newest > stamp && *newest != epoch) {
            tracing::warn!(
                database = %name,
                stamp,
                newest,
                epoch,
                "fleet database on disk is older than its replica: another host owned it \
                 meanwhile; it is restored from the replica"
            );
            for hook in self.hooks() {
                hook.release(key);
            }
            check_contained(self.root(), path)?;
            match mode {
                ShipMode::Continuous(_) => {
                    restore_in_place(self.root(), replication, key, None, path)?;
                }
                ShipMode::Generations { .. } => {
                    // Only a generation above the stamp holds newer data. The
                    // newer owner can have written its marker and no
                    // generation: then the file on disk is the newest data.
                    match self.restore_generation_in_place(
                        replication,
                        key,
                        None,
                        path,
                        Some(stamp),
                    ) {
                        Ok(false) | Err(FleetError::NotFound { .. }) => {
                            tracing::warn!(database = %name, "fleet database: the newer epoch has no generation; the file on disk stays");
                        }
                        other => {
                            other?;
                        }
                    }
                }
                // No replica holds the data.
                _ => {}
            }
        }
        Ok(())
    }

    /// Ship the open database of `key` now. See
    /// [`ship_now`](Self::ship_now).
    pub(super) async fn ship_replica(&self, key: &FleetDbKey) -> Result<ShipReport, FleetError> {
        let name = key.name();
        let replication = self
            .replication()
            .cloned()
            .ok_or_else(|| FleetError::NotReplicated { name: name.clone() })?;
        let not_open = || FleetError::Unavailable {
            name: name.clone(),
            detail: "the database is not open in this process, or another process ships it"
                .to_owned(),
        };
        match self.inner.engine.ship_mode() {
            ShipMode::Continuous(_) => {
                let (worker, task_key) = (Arc::clone(&replication), key.clone());
                let ticked = crate::time::spawn_blocking(move || worker.ship_one(&task_key))
                    .await
                    .map_err(|error| FleetError::Unavailable {
                        name: name.clone(),
                        detail: error.to_string(),
                    })?;
                match ticked {
                    Ok(Some(report)) => Ok(ShipReport {
                        generation: None,
                        shipped: usize::try_from(report.segments)
                            .unwrap_or(usize::MAX)
                            .saturating_add(usize::from(report.snapshot_taken)),
                        reused: 0,
                        bytes: report.bytes,
                    }),
                    Ok(None) => Err(not_open()),
                    Err(crate::replication::ReplicationError::Fenced { epoch, newest }) => {
                        Err(FleetError::Fenced {
                            name,
                            epoch,
                            newest,
                        })
                    }
                    Err(error) => Err(FleetError::Unavailable {
                        name,
                        detail: error.to_string(),
                    }),
                }
            }
            ShipMode::Generations { .. } => {
                let db = self.get_open(key).ok_or_else(not_open)?;
                self.ship_generation(&replication, db).await
            }
            _ => Err(FleetError::NotReplicated { name }),
        }
    }

    /// Replace the closed database of `key` from its replica, or rebuild it.
    /// The caller holds the drain of `key`. See
    /// [`restore_latest`](Self::restore_latest).
    pub(super) async fn replace_from_replica(
        &self,
        key: &FleetDbKey,
        target: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<Option<crate::replication::RestoreOutcome>, FleetError> {
        let mode = self.inner.engine.ship_mode();
        let replication = self.replication().cloned();
        if !matches!(mode, ShipMode::Rebuild) && replication.is_none() {
            return Err(FleetError::NotReplicated { name: key.name() });
        }
        if let Some(replication) = &replication {
            // A ship still in flight writes nothing more: wait for it.
            let (worker, task_key) = (Arc::clone(replication), key.clone());
            let _ = crate::time::spawn_blocking(move || worker.quiesce_released(&task_key)).await;
        }
        // Take the tombstone first, under the purge turn: the retry task
        // then cannot purge the replica this restore reads.
        let _turn = match &replication {
            Some(replication) => Some(replication.purge_turn().lock().await),
            None => None,
        };
        let claimed = match &replication {
            Some(replication) => self.claim_tombstone(replication, key).await?,
            None => None,
        };
        let path = self.path_of(key);
        let restored = match (&mode, &replication) {
            (ShipMode::Rebuild, _) => self.rebuild_in_place(key, &path).await,
            (_, Some(replication)) => {
                let (fleet, replication, task_key, task_path) = (
                    self.clone(),
                    Arc::clone(replication),
                    key.clone(),
                    path.clone(),
                );
                let generations = matches!(mode, ShipMode::Generations { .. });
                crate::time::spawn_blocking(move || {
                    check_contained(fleet.root(), &task_path)?;
                    if generations {
                        fleet
                            .restore_generation_in_place(
                                &replication,
                                &task_key,
                                target,
                                &task_path,
                                None,
                            )
                            .map(|_| None)
                    } else {
                        restore_in_place(fleet.root(), &replication, &task_key, target, &task_path)
                            .map(Some)
                    }
                })
                .await
                .map_err(|error| FleetError::Restore {
                    name: key.name(),
                    detail: error.to_string(),
                })
                .and_then(|r| r)
            }
            (_, None) => Err(FleetError::NotReplicated { name: key.name() }),
        };
        if let Some(replication) = &replication {
            match (&restored, claimed) {
                (Ok(_), _) => self.forget_marker(replication, key).await,
                // Nothing came back: the tombstone stays.
                (Err(_), Some(row)) => {
                    if let Some(tombstones) = replication.tombstones()
                        && let Err(error) = tombstones.put(&row).await
                    {
                        tracing::error!(database = %key, %error, "fleet restore failed and its tombstone could not be put back");
                    }
                }
                (Err(_), None) => {}
            }
        }
        restored
    }

    /// Rebuild the database at `path` from its source. A rebuild replaces a
    /// database: it never brings back a deleted one.
    ///
    /// # Errors
    ///
    /// [`FleetError::NotFound`] when no database is at `path`, or the error
    /// of [`FleetEngine::rebuild`].
    pub(super) async fn rebuild_in_place(
        &self,
        key: &FleetDbKey,
        path: &Path,
    ) -> Result<Option<crate::replication::RestoreOutcome>, FleetError> {
        check_contained(self.root(), path)?;
        if !self.inner.engine.exists(path) {
            return Err(FleetError::NotFound { name: key.name() });
        }
        self.inner.engine.rebuild(key, path).await.map(|()| None)
    }

    /// An explicit restore of `key` starts: take its tombstone row, so the
    /// retry task does not purge the replica meanwhile. The caller puts it
    /// back if the restore fails.
    ///
    /// # Errors
    ///
    /// [`FleetError::Restore`] when the row cannot be read or dropped. The
    /// restore does not start: a row left next to the restored file would
    /// block its later restore.
    async fn claim_tombstone(
        &self,
        replication: &crate::db::fleet_replication::FleetReplication,
        key: &FleetDbKey,
    ) -> Result<Option<crate::db::fleet_tombstones::Tombstone>, FleetError> {
        let Some(tombstones) = replication.tombstones() else {
            return Ok(None);
        };
        let name = key.name();
        let failed = |error: crate::db::fleet_tombstones::TombstoneError| FleetError::Restore {
            name: name.clone(),
            detail: format!("could not take the tombstone: {error}"),
        };
        let row = tombstones.get(&name).await.map_err(failed)?;
        if row.is_some() {
            tombstones.remove(&name).await.map_err(failed)?;
        }
        Ok(row)
    }

    /// An explicit restore brought `key` back: remove its marker, so other
    /// hosts restore it again.
    async fn forget_marker(
        &self,
        replication: &Arc<crate::db::fleet_replication::FleetReplication>,
        key: &FleetDbKey,
    ) {
        let (worker, marker_key) = (Arc::clone(replication), key.clone());
        let removed = crate::time::spawn_blocking(move || worker.delete_marker(&marker_key)).await;
        if let Ok(Err(error)) = removed {
            tracing::warn!(database = %key, %error, "fleet restore: could not remove the tombstone marker");
        }
    }

    /// Before the shutdown closes the databases: bound the final ship of
    /// each one. Keeps a cutoff the caller set before.
    pub(super) fn begin_replica_shutdown(&self, began: std::time::Instant) {
        if let Some(replication) = self.replication()
            && replication.cutoff_left().is_none()
        {
            replication.set_shutdown_cutoff(began.checked_add(replication.final_flush()));
        }
    }

    /// Every database is closed. Keep shipping parked replicators until they
    /// catch up or the flush deadline passes, then stop the loop thread so it
    /// does not outlive the app (an embedded or restarted server). Closing
    /// the databases used part of the deadline.
    pub(super) async fn finish_replica_shutdown(&self, began: std::time::Instant) {
        if let Some(replication) = self.replication().cloned() {
            let _ = crate::time::spawn_blocking(move || {
                let left = replication
                    .cutoff_left()
                    .unwrap_or_else(|| replication.final_flush().saturating_sub(began.elapsed()));
                replication.flush_closing(left);
                replication.stop();
            })
            .await;
        }
    }

    /// Whether the fleet replicates (for health).
    pub(super) fn replicates(&self) -> bool {
        self.replication().is_some()
    }

    /// After a delete removed the files: confirm the tombstone, write the
    /// marker, and purge the replica unless the retention keeps it. Never
    /// fails the delete.
    pub(super) async fn purge_after_delete(&self, key: &FleetDbKey) {
        let Some(replication) = self.replication().cloned() else {
            return;
        };
        let name = key.name();
        // A tick still in flight on a released replica could ship after the
        // cutoff is taken.
        {
            let (replication, key) = (Arc::clone(&replication), key.clone());
            let _ = crate::time::spawn_blocking(move || replication.quiesce_released(&key)).await;
        }
        let retention = replication.retention_after_delete();
        let tombstones = replication.tombstones().cloned();
        let floor = match (newest_generation(&replication, key).await, &tombstones) {
            (Ok(floor), _) => floor,
            (Err(error), Some(_)) => {
                // The retry task confirms the pending row once the lookup works.
                tracing::warn!(database = %key, %error, "fleet delete: could not read the replica's generations; the tombstone stays pending and the retry task purges the replica");
                return;
            }
            (Err(_), None) => None,
        };
        let confirmed = match &tombstones {
            Some(tombstones) => match tombstones.confirm(&name, retention, floor).await {
                Ok(Some(row)) => Some(row),
                Ok(None) => {
                    // Another process re-created or restored the key after
                    // the removal. Its new database is not this purge's.
                    tracing::info!(database = %key, "fleet delete: the key was made again; nothing to purge");
                    return;
                }
                Err(error) => {
                    tracing::warn!(database = %key, %error, "fleet delete: could not confirm the replica tombstone");
                    None
                }
            },
            None => None,
        };
        let row = confirmed.clone().unwrap_or_else(|| {
            crate::db::fleet_tombstones::Tombstone::new(&name, replication.now(), retention, true)
        });
        let _turn = replication.purge_turn().lock().await;
        // The pending row stays when the confirm failed: the retry task
        // confirms it later, and its cutoff cannot cover a re-create.
        let purge_now = retention.is_zero() && (confirmed.is_some() || tombstones.is_none());
        let (marked, result) = {
            let (replication, key, row) = (Arc::clone(&replication), key.clone(), row.clone());
            crate::time::spawn_blocking(move || {
                // Before the purge: a purge that stops part way leaves the
                // marker, so another host does not restore the rest.
                let marked = replication
                    .write_marker(&key, &row)
                    .inspect_err(|error| {
                        tracing::warn!(database = %key, %error, "fleet delete: could not write the tombstone marker; the retry task writes it");
                    })
                    .is_ok();
                let purged = if purge_now {
                    replication.purge_replica(&key, row.deleted_at).map(Some)
                } else {
                    Ok(None)
                };
                (marked, purged)
            })
            .await
            .unwrap_or_else(|error| (false, Err(error.to_string())))
        };
        if marked
            && confirmed.is_some()
            && let Some(tombstones) = &tombstones
            && let Err(error) = tombstones.set_marked(&name).await
        {
            tracing::warn!(database = %key, %error, "fleet delete: could not record the marker");
        }
        match (result, &tombstones) {
            (Ok(Some(objects)), _) => {
                tracing::info!(database = %key, objects, "fleet delete: replica purged");
                if let Some(tombstones) = &tombstones
                    && let Err(error) = tombstones.remove(&name).await
                {
                    tracing::warn!(database = %key, %error, "fleet delete: could not drop the tombstone; the retry task drops it");
                }
            }
            (Ok(None), _) if confirmed.is_some() => {
                tracing::info!(database = %key, purge_after = %row.purge_after, "fleet delete: replica kept until its retention ends");
            }
            (Ok(None), _) => {
                tracing::warn!(database = %key, "fleet delete: the pending tombstone stays; the retry task purges the replica");
            }
            (Err(error), Some(tombstones)) if confirmed.is_some() => {
                tracing::warn!(database = %key, %error, "fleet delete: could not purge the replica; the retry task tries again");
                if let Err(error) = tombstones.record_failure(row, &error).await {
                    tracing::warn!(database = %key, %error, "fleet delete: could not record the failed purge");
                }
            }
            (Err(error), Some(_)) => {
                tracing::warn!(database = %key, %error, "fleet delete: could not purge the replica; the pending tombstone stays and the retry task purges it");
            }
            (Err(error), None) => {
                tracing::error!(database = %key, %error, "fleet delete: could not purge the replica, and no tombstone records it; the replica stays");
            }
        }
    }

    /// Before a new database is made for `key`: purge the replica of a
    /// deleted database with the same key, and drop its tombstone. The new
    /// database must not share a prefix with old data that a later purge
    /// deletes. An `explicit` create (`provision`) ends a retention early.
    /// A create on demand does not: it gets `410` while the row is pending
    /// or in its retention.
    ///
    /// # Errors
    ///
    /// [`FleetError::Deleting`] for a create on demand while a row is
    /// pending or in its retention.
    /// [`FleetError::Unavailable`] when the tombstone cannot be read or the
    /// old replica cannot be purged. Retry later.
    pub(super) async fn purge_before_create(
        &self,
        key: &FleetDbKey,
        explicit: bool,
    ) -> Result<(), FleetError> {
        let Some(replication) = self.replication().cloned() else {
            return Ok(());
        };
        let Some(tombstones) = replication.tombstones().cloned() else {
            return Ok(());
        };
        let name = key.name();
        let unavailable = |detail: String| FleetError::Unavailable {
            name: name.clone(),
            detail,
        };
        let read = |row: Result<_, crate::db::fleet_tombstones::TombstoneError>| {
            row.map_err(|error| unavailable(error.to_string()))
        };
        if read(tombstones.get(&name).await)?.is_none() {
            return self
                .check_remote_retention(&replication, key, explicit)
                .await;
        }
        let _turn = replication.purge_turn().lock().await;
        let Some(row) = read(tombstones.get(&name).await)? else {
            return Ok(());
        };
        if !explicit
            && !crate::db::fleet_tombstones::create_on_demand_allowed(Some(&row), tombstones.now())
        {
            // A stale client must not end the undo window. A pending row has
            // no cutoff yet (issue #3516).
            return Err(FleetError::Deleting { name });
        }
        let cutoff = if row.confirmed {
            row.deleted_at
        } else {
            replication.now()
        };
        let (purger, purge_key) = (Arc::clone(&replication), key.clone());
        crate::time::spawn_blocking(move || purger.purge_replica(&purge_key, cutoff))
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result)
            .map_err(|error| {
                unavailable(format!(
                    "the replica of the deleted database is not purged yet ({error}); retry"
                ))
            })?;
        tombstones
            .remove(&name)
            .await
            .map_err(|error| unavailable(error.to_string()))?;
        tracing::info!(database = %key, "fleet database made again: the old replica is purged");
        Ok(())
    }

    /// A create on demand with no local tombstone: another host may have
    /// deleted the key and keep its replica for undo. Its marker says until
    /// when. Only with `restore_missing`, which reads the replicas already.
    ///
    /// # Errors
    ///
    /// [`FleetError::Deleting`] during that retention.
    /// [`FleetError::Unavailable`] when the marker cannot be read.
    async fn check_remote_retention(
        &self,
        replication: &Arc<crate::db::fleet_replication::FleetReplication>,
        key: &FleetDbKey,
        explicit: bool,
    ) -> Result<(), FleetError> {
        if explicit || !replication.restores_missing() {
            return Ok(());
        }
        let (reader, marker_key) = (Arc::clone(replication), key.clone());
        let marker = crate::time::spawn_blocking(move || reader.marker(&marker_key))
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result)
            .map_err(|detail| FleetError::Unavailable {
                name: key.name(),
                detail: format!("could not read the tombstone marker: {detail}"),
            })?;
        if marker.is_some_and(|marker| replication.now() < marker.purge_after()) {
            return Err(FleetError::Deleting { name: key.name() });
        }
        Ok(())
    }

    /// Purge the replica of each deleted database that is due (issue
    /// #3235): its retention ended, and its backoff after a failed purge
    /// too. The replica lifecycle task calls this every minute. A key whose
    /// file is on disk, or whose slot this host does not own, waits.
    pub async fn purge_deleted_replicas(&self) -> crate::db::fleet_tombstones::PurgeReport {
        let mut report = crate::db::fleet_tombstones::PurgeReport::default();
        let Some(replication) = self.replication().cloned() else {
            return report;
        };
        let Some(tombstones) = replication.tombstones().cloned() else {
            return report;
        };
        // Every row, page by page: rows that wait must not hide later ones.
        let mut after: Option<String> = None;
        loop {
            let page = match tombstones.list_after(after.as_deref(), PURGE_BATCH).await {
                Ok(page) => page,
                Err(error) => {
                    report.last_error = Some(error.to_string());
                    break;
                }
            };
            let Some(last) = page.last().map(|row| row.key.clone()) else {
                break;
            };
            for listed in page {
                self.purge_one(&replication, &tombstones, &listed.key, &mut report)
                    .await;
            }
            after = Some(last);
        }
        replication.record_purge_report(report.clone());
        report
    }

    /// One row of [`purge_deleted_replicas`](Self::purge_deleted_replicas).
    async fn purge_one(
        &self,
        replication: &Arc<crate::db::fleet_replication::FleetReplication>,
        tombstones: &crate::db::fleet_tombstones::FleetTombstones,
        name: &str,
        report: &mut crate::db::fleet_tombstones::PurgeReport,
    ) {
        let fail = |report: &mut crate::db::fleet_tombstones::PurgeReport, error: String| {
            report.failed += 1;
            report.last_error = Some(error);
        };
        let Ok(key) = self.key_for_name(name) else {
            // Another mode's name: not this fleet's to purge.
            report.waiting += 1;
            return;
        };
        let _turn = replication.purge_turn().lock().await;
        // Again under the turn: a re-create may have handled it.
        let row = match tombstones.get(name).await {
            Ok(Some(row)) => row,
            Ok(None) => return,
            Err(error) => return fail(report, error.to_string()),
        };
        let now = tombstones.now();
        if row.attempts > 0 {
            report.failing += 1;
            if report.last_error.is_none() {
                report.last_error.clone_from(&row.last_error);
            }
        }
        let stale = chrono::Duration::from_std(crate::db::fleet_tombstones::STALE_PENDING)
            .is_ok_and(|stale| now.signed_duration_since(row.deleted_at) > stale);
        if self.exists(&key) {
            // A confirmed row next to a live file is stale: a restore or a
            // re-create drops the row before it publishes. A pending row is
            // a delete that runs now, or one that stopped long ago.
            if row.confirmed {
                let _ = tombstones.remove(name).await;
            } else if stale {
                let _ = tombstones.remove_pending(name, None).await;
            } else {
                report.waiting += 1;
            }
            return;
        }
        // A young pending row: its delete may still find no file and drop it.
        if !self.owns(&key) || (!row.confirmed && !stale) {
            report.waiting += 1;
            return;
        }
        let row = if row.confirmed {
            row
        } else {
            // The delete stopped after the removal.
            let floor = match newest_generation(replication, &key).await {
                Ok(floor) => floor,
                Err(error) => return fail(report, error),
            };
            match tombstones
                .confirm(name, replication.retention_after_delete(), floor)
                .await
            {
                Ok(Some(row)) => row,
                Ok(None) => return,
                Err(error) => return fail(report, error.to_string()),
            }
        };
        if !row.marked {
            // Other hosts read the marker during the retention too.
            let (writer, marker_key, marker_row) =
                (Arc::clone(replication), key.clone(), row.clone());
            let written =
                crate::time::spawn_blocking(move || writer.write_marker(&marker_key, &marker_row))
                    .await
                    .map_err(|error| error.to_string())
                    .and_then(|result| result);
            match written {
                Ok(()) => {
                    if let Err(error) = tombstones.set_marked(name).await {
                        fail(report, error.to_string());
                    }
                }
                Err(error) => fail(report, error),
            }
        }
        if !row.is_due(now) {
            report.waiting += 1;
            return;
        }
        let (purger, purge_key, cutoff) = (Arc::clone(replication), key.clone(), row.deleted_at);
        let outcome = crate::time::spawn_blocking(move || purger.purge_replica(&purge_key, cutoff))
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result);
        match outcome {
            Ok(objects) => match tombstones.remove(name).await {
                Ok(()) => {
                    tracing::info!(database = %key, objects, "fleet: replica of a deleted database purged");
                    report.purged += 1;
                }
                Err(error) => fail(report, error.to_string()),
            },
            Err(error) => {
                tracing::warn!(database = %key, %error, attempts = row.attempts + 1, "fleet: could not purge the replica of a deleted database");
                if let Err(store) = tombstones.record_failure(row, &error).await {
                    fail(report, store.to_string());
                } else {
                    fail(report, error);
                }
            }
        }
    }

    /// Restore `sample` random databases of this fleet into scratch files,
    /// and check each one (issue #3235): `PRAGMA integrity_check` and the
    /// identity row. The result goes to the `replication:fleet` health
    /// indicator, which turns `Down` when a check failed. Blocking work runs
    /// on the blocking pool.
    ///
    /// # Errors
    ///
    /// [`FleetError::NotReplicated`] without replication, or a
    /// [`list`](Self::list) error.
    pub async fn check_replica_sample(
        &self,
        sample: usize,
    ) -> Result<crate::db::fleet_replication::RestoreCheckReport, FleetError> {
        let replication = self
            .replication()
            .cloned()
            .ok_or_else(|| FleetError::NotReplicated {
                name: self.root().display().to_string(),
            })?;
        let mut keys = self.list().await?;
        // A partial Fisher-Yates shuffle: the first `take` keys are the sample.
        let take = sample.min(keys.len());
        for i in 0..take {
            let span = u64::try_from(keys.len() - i).unwrap_or(u64::MAX);
            let pick = usize::try_from(replication.entropy().next_u64() % span).unwrap_or(0);
            keys.swap(i, i + pick);
        }
        keys.truncate(take);
        let scratch = self
            .root()
            .join(crate::fleet_layout::RESERVED_DIR)
            .join(RESTORE_CHECK_DIR);
        let checker = Arc::clone(&replication);
        let generations = matches!(self.inner.engine.ship_mode(), ShipMode::Generations { .. });
        let now = replication.now();
        let mut report = crate::time::spawn_blocking(move || {
            // A process that died during a check leaves its files.
            sweep_scratch(&scratch, now);
            let mut report = crate::db::fleet_replication::RestoreCheckReport::default();
            for key in keys {
                let outcome = if generations {
                    super::generations::check_restore(&checker, &key, &scratch)
                } else {
                    checker.check_restore(&key, &scratch)
                };
                match outcome {
                    crate::db::fleet_replication::RestoreCheck::Passed => {
                        report.passed.push(key.name());
                    }
                    crate::db::fleet_replication::RestoreCheck::NoReplica => {
                        report.unreplicated.push(key.name());
                    }
                    crate::db::fleet_replication::RestoreCheck::Failed(error) => {
                        tracing::error!(database = %key, %error, "fleet restore check failed");
                        report.failed.push((key.name(), error));
                    }
                }
            }
            report
        })
        .await
        .map_err(|error| FleetError::Io {
            op: "restore check",
            detail: error.to_string(),
        })?;
        report.checked_at = Some(replication.now());
        replication.record_restore_check(report.clone());
        Ok(report)
    }

    /// Start the replica lifecycle task (issue #3235): a purge pass every
    /// minute, and the restore check at its interval. `None` without
    /// replication. It stops when `shutdown` is cancelled.
    pub(super) fn spawn_replica_lifecycle(
        &self,
        shutdown: CancellationToken,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let replication = self.replication().cloned()?;
        let fleet = self.clone();
        Some(tokio::spawn(async move {
            let mut purge = tokio::time::interval(PURGE_PASS_INTERVAL);
            purge.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let (every, sample) = replication.restore_check().unwrap_or((Duration::MAX, 0));
            let mut check =
                tokio::time::interval(every.min(Duration::from_secs(u64::from(u32::MAX))));
            check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // The first tick is at once. Boot is busy: the first check waits
            // one interval.
            check.tick().await;
            loop {
                tokio::select! {
                    () = shutdown.cancelled() => break,
                    _ = purge.tick() => {
                        let _ = fleet.purge_deleted_replicas().await;
                    }
                    _ = check.tick(), if sample > 0 => {
                        let _ = fleet.check_replica_sample(sample).await;
                    }
                }
            }
        }))
    }
}

/// Remove the scratch files and staging directories under `dir` that were
/// last changed more than [`SCRATCH_STALE`] before `now`. Blocking.
fn sweep_scratch(dir: &Path, now: chrono::DateTime<chrono::Utc>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let changed = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .map(chrono::DateTime::<chrono::Utc>::from);
        let old = changed.is_ok_and(|changed| {
            now.signed_duration_since(changed)
                .to_std()
                .is_ok_and(|age| age > SCRATCH_STALE)
        });
        if !old {
            continue;
        }
        let path = entry.path();
        let _ = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
    }
}

/// The newest generation in the replica of `key`, read on the blocking
/// pool. `Ok(None)` when there is none.
///
/// # Errors
///
/// The destination failed. Then no cutoff can be set: a generation of a
/// host clock that is ahead could be newer than the app clock.
async fn newest_generation(
    replication: &Arc<crate::db::fleet_replication::FleetReplication>,
    key: &FleetDbKey,
) -> Result<Option<chrono::DateTime<chrono::Utc>>, String> {
    let (reader, key) = (Arc::clone(replication), key.clone());
    crate::time::spawn_blocking(move || reader.newest_generation(&key))
        .await
        .map_err(|error| error.to_string())
        .and_then(|result| result)
}
