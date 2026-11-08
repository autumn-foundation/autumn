//! Restore one fleet database from a local backup file.

use super::sqlite::{swap_in_staged, task_failed};
use super::{
    Arc, DatabaseFleet, FleetDbKey, FleetError, Path, ShipReport, check_contained, discard_staging,
};

/// The options of [`DatabaseFleet::restore_from_file`]. Each one makes the
/// restore accept more. All are off by default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LocalRestore {
    allow_rekey: bool,
    clear_tombstone: bool,
    apply_migrations: bool,
}

impl LocalRestore {
    /// The default options: the safe rules.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            allow_rekey: false,
            clear_tombstone: false,
            apply_migrations: false,
        }
    }

    /// Accept a backup of another key, and write the target key into the
    /// identity row of the copy. Use it to copy one tenant to a new key.
    /// Without it, such a backup is refused. Default: `false`.
    #[must_use]
    pub const fn allow_rekey(mut self, allow: bool) -> Self {
        self.allow_rekey = allow;
        self
    }

    /// Restore a deleted key that has a tombstone. The restore then purges
    /// the replica of the deleted database and drops the tombstone, as
    /// `provision` does. This ends the undo window of the delete. Without
    /// it, such a key is refused. Default: `false`.
    #[must_use]
    pub const fn clear_tombstone(mut self, clear: bool) -> Self {
        self.clear_tombstone = clear;
        self
    }

    /// Apply the pending migrations of the backup even when opens do not
    /// apply them (`auto_migrate` off). Without it, and with opens that do
    /// not apply them, pending migrations refuse the restore. Default:
    /// `false`.
    #[must_use]
    pub const fn apply_migrations(mut self, apply: bool) -> Self {
        self.apply_migrations = apply;
        self
    }
}

/// What [`DatabaseFleet::restore_from_file`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct LocalRestoreOutcome {
    /// A database file was there before.
    pub replaced: bool,
    /// The key the backup recorded, when the restore rewrote it.
    pub rekeyed_from: Option<String>,
    /// Migrations applied to the backup.
    pub migrations_applied: usize,
    /// The restore removed the tombstone of a deleted key.
    pub tombstone_cleared: bool,
    /// The ship of the new base snapshot.
    pub shipped: Option<ShipReport>,
}

impl DatabaseFleet {
    /// Replace (or create) the database of `key` with the backup at `src`,
    /// for example a file that [`backup`](Self::backup) wrote. `src` does not
    /// change. Replication is not needed.
    ///
    /// 1. The key goes out of service: openers wait, and the open database
    ///    closes.
    /// 2. The fleet copies `src` into a staging file with `VACUUM INTO`. The
    ///    copy must pass `PRAGMA integrity_check`, and its identity row must
    ///    record `key` (see [`LocalRestore::allow_rekey`]).
    /// 3. The fleet applies pending migrations when opens apply them, or
    ///    when [`LocalRestore::apply_migrations`] is set. Otherwise pending
    ///    migrations refuse the restore.
    /// 4. The copy gets the current epoch with ownership, and no epoch
    ///    without it.
    /// 5. A key with a tombstone (a delete) is refused, unless
    ///    [`LocalRestore::clear_tombstone`] is set. Then the old replica and
    ///    the tombstone go, as in [`provision`](Self::provision).
    /// 6. The copy goes in place of the old file in one step, under the path
    ///    lock. The old `-wal` and `-shm` go.
    /// 7. Openers continue, and see the restored file.
    /// 8. With replication, the fleet opens the database and ships a new
    ///    base snapshot. So a later replica restore does not roll it back.
    ///
    /// A failure before step 6 changes nothing. Cancellation-safe: the work
    /// runs in an owned task.
    ///
    /// # Errors
    ///
    /// [`FleetError::Restore`] when `src` is refused (missing, not a
    /// database, corrupt, another key), when the key has a tombstone, or
    /// when the base snapshot does not ship (the file is in place then).
    /// [`FleetError::PendingMigrations`] when migrations are pending and
    /// not applied. [`FleetError::InUseElsewhere`] when another process has
    /// the file open. [`FleetError::NotOwner`] when this host does not own
    /// the slot. [`FleetError::Deleting`] during a delete.
    /// [`FleetError::Unavailable`] once a shutdown has begun.
    pub async fn restore_from_file(
        &self,
        key: &FleetDbKey,
        src: &Path,
        opts: LocalRestore,
    ) -> Result<LocalRestoreOutcome, FleetError> {
        self.check_key(key)?;
        let epoch = self.owned_epoch(key)?;
        let counted = self.admit_mutation(key)?;
        let (fleet, task_key, src) = (self.clone(), key.clone(), src.to_path_buf());
        tokio::spawn(async move {
            // Held to the end, so `close_all` also sees the open of the ship.
            let _counted = counted;
            let swap = fleet.swap_in_backup(&task_key, &src, opts, epoch);
            let mut outcome = fleet.with_exclusive(&task_key, swap).await?;
            outcome.shipped = fleet.ship_restored(&task_key).await?;
            tracing::info!(database = %task_key, "fleet database restored from a local file");
            Ok(outcome)
        })
        .await
        .map_err(|e| FleetError::Restore {
            name: key.name(),
            detail: format!("restore task failed: {e}"),
        })?
    }

    /// Steps 2 to 6 of [`restore_from_file`](Self::restore_from_file). The
    /// caller holds the key out of service.
    async fn swap_in_backup(
        &self,
        key: &FleetDbKey,
        src: &Path,
        opts: LocalRestore,
        epoch: Option<crate::lock::FencingToken>,
    ) -> Result<LocalRestoreOutcome, FleetError> {
        self.with_provision_turn(key, async {
            let tombstoned = self.deleted_in_replica(key).await?;
            if tombstoned && !opts.clear_tombstone {
                return Err(FleetError::Restore {
                    name: key.name(),
                    detail: "the key was deleted and its tombstone stands; set \
                             LocalRestore::clear_tombstone to restore it"
                        .to_owned(),
                });
            }
            if let Some(replication) = self.replication() {
                // A ship still in flight on the old file writes nothing more.
                let (worker, task_key) = (Arc::clone(replication), key.clone());
                let _ =
                    crate::time::spawn_blocking(move || worker.quiesce_released(&task_key)).await;
            }
            let (fleet, task_key, src) = (self.clone(), key.clone(), src.to_path_buf());
            let staged = crate::time::spawn_blocking(move || {
                fleet.stage_restore(
                    &task_key,
                    &src,
                    opts.allow_rekey,
                    opts.apply_migrations,
                    epoch.map(crate::lock::FencingToken::get),
                )
            })
            .await
            .map_err(task_failed(&key.name(), "restore"))??;
            let swapped = self
                .publish_backup(key, &staged.file, epoch, tombstoned)
                .await;
            discard_staging(&staged.file);
            let replaced = swapped?;
            self.count_migrations(staged.applied);
            Ok(LocalRestoreOutcome {
                replaced,
                rekeyed_from: staged.rekeyed_from,
                migrations_applied: staged.applied,
                tombstone_cleared: tombstoned,
                shipped: None,
            })
        })
        .await
    }

    /// Clear the tombstone if asked, then put the checked staging file in
    /// place. Returns whether a database was there.
    async fn publish_backup(
        &self,
        key: &FleetDbKey,
        staging: &Path,
        epoch: Option<crate::lock::FencingToken>,
        tombstoned: bool,
    ) -> Result<bool, FleetError> {
        // The copy carries the epoch read at the start. A slot lost since
        // then must not get a file stamped for it.
        if epoch.is_some() && self.owned_epoch(key)? != epoch {
            return Err(FleetError::NotOwner {
                name: key.name(),
                slot: key.routing_slot(),
            });
        }
        if tombstoned {
            self.purge_before_create(key, true).await?;
        }
        let (root, path, staging, name) = (
            self.root().to_path_buf(),
            self.path_of(key),
            staging.to_path_buf(),
            key.name(),
        );
        crate::time::spawn_blocking(move || {
            check_contained(&root, &path)?;
            swap_in_staged(&root, &path, &name, &staging)
        })
        .await
        .map_err(task_failed(&key.name(), "restore"))?
    }

    /// Step 8: open the restored database and ship a new base snapshot.
    /// `None` without replication.
    async fn ship_restored(&self, key: &FleetDbKey) -> Result<Option<ShipReport>, FleetError> {
        if self.replication().is_none() {
            return Ok(None);
        }
        let not_shipped = |error: FleetError| FleetError::Restore {
            name: key.name(),
            detail: format!(
                "the backup is in place, but its new base snapshot did not ship ({error}); \
                 call ship_now before any replica restore"
            ),
        };
        let db = self.open_with(key, false).await.map_err(not_shipped)?;
        let shipped = self.ship_replica(key).await.map_err(not_shipped);
        drop(db);
        shipped.map(Some)
    }
}
