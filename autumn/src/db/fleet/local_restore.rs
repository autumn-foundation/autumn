//! Restore one fleet database from a local backup file.

use super::sqlite::{swap_in_staged, task_failed};
use super::{
    Arc, DatabaseFleet, FleetDbKey, FleetError, Path, ShipReport, check_contained, discard_staging,
};

/// The options of [`DatabaseFleet::restore_from_file`]. Each option makes
/// the restore accept more. All options are off by default.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalRestore {
    allow_rekey: bool,
    clear_tombstone: bool,
    apply_migrations: bool,
}

impl LocalRestore {
    /// All options off. The restore accepts only a backup of the same key.
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
    /// If this is off, the restore refuses such a backup. Default: `false`.
    #[must_use]
    pub const fn allow_rekey(mut self, allow: bool) -> Self {
        self.allow_rekey = allow;
        self
    }

    /// Restore a deleted key that has a tombstone. After the swap, the
    /// restore purges the replica of the deleted database and drops the
    /// tombstone, as `provision` does. This ends the undo window of the
    /// delete. If this is off, the restore refuses such a key. Default:
    /// `false`.
    #[must_use]
    pub const fn clear_tombstone(mut self, clear: bool) -> Self {
        self.clear_tombstone = clear;
        self
    }

    /// Apply the pending migrations of the backup. If this is off and opens
    /// do not apply migrations, the restore refuses a backup that has
    /// pending migrations. Default: `false`.
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
    /// A database file was there before the restore.
    pub replaced: bool,
    /// The key the backup recorded, when the restore wrote a new key.
    pub rekeyed_from: Option<String>,
    /// Migrations applied to the copy.
    pub migrations_applied: usize,
    /// The restore dropped the tombstone and purged the old replica. `false`
    /// when the purge failed: the old objects stay, older than the new base.
    pub tombstone_cleared: bool,
    /// The ship of the new base snapshot. `None` without replication, or
    /// when the ship failed.
    pub shipped: Option<ShipReport>,
    /// Why the new base snapshot did not ship. The restored file is in
    /// place. Ship it before any replica restore: an open of the key in a
    /// process that replicates it ships a new base.
    pub ship_error: Option<String>,
}

impl DatabaseFleet {
    /// Replace (or create) the database of `key` with the backup at
    /// `source`, for example a file that [`backup`](Self::backup) wrote.
    /// `source` does not change. Replication is not needed.
    ///
    /// 1. A key with a tombstone is refused, unless
    ///    [`LocalRestore::clear_tombstone`] is set.
    /// 2. The fleet copies `source` into a staging file with `VACUUM INTO`.
    ///    The copy must pass `PRAGMA integrity_check`. Its identity row must
    ///    record `key` (see [`LocalRestore::allow_rekey`]). A trigger on the
    ///    identity or epoch table is refused.
    /// 3. The fleet applies pending migrations to the copy when opens apply
    ///    them (`apply_migrations_on_open`, set by `database.auto_migrate`)
    ///    or when [`LocalRestore::apply_migrations`] is set. Otherwise
    ///    pending migrations refuse the restore.
    /// 4. With ownership, the copy gets the epoch of this host. Without
    ///    ownership, the copy has no epoch.
    /// 5. The key goes out of service. Openers wait. The open database
    ///    closes.
    /// 6. The copy goes in place of the old file in one step, under the path
    ///    lock. The old `-wal` and `-shm` go.
    /// 7. With [`LocalRestore::clear_tombstone`], the old replica and the
    ///    tombstone go.
    /// 8. Openers continue. They see the restored file.
    /// 9. With replication, the fleet opens the database and ships a new
    ///    base snapshot. A later replica restore then finds the restored
    ///    data. A failed ship does not fail the call: see
    ///    [`LocalRestoreOutcome::ship_error`].
    ///
    /// An error before step 6 changes nothing. Cancellation-safe: the work
    /// runs in an owned task.
    ///
    /// # Errors
    ///
    /// - [`FleetError::Restore`] when `source` is refused (missing, not a
    ///   database, corrupt, another key, a trigger on the identity or epoch
    ///   table), or when the key has a tombstone.
    /// - [`FleetError::PendingMigrations`] when migrations are pending and
    ///   are not applied. [`FleetError::Migration`] when a migration fails.
    /// - [`FleetError::InUseElsewhere`] when another process has the file
    ///   open.
    /// - [`FleetError::NotOwner`] when this host does not own the slot.
    /// - [`FleetError::Deleting`] during a delete.
    /// - [`FleetError::InvalidKey`] for a key of another mode.
    /// - [`FleetError::Unavailable`] once a shutdown has begun, or when a
    ///   task or a file write fails.
    pub async fn restore_from_file(
        &self,
        key: &FleetDbKey,
        source: &Path,
        opts: LocalRestore,
    ) -> Result<LocalRestoreOutcome, FleetError> {
        self.check_key(key)?;
        let epoch = self.owned_epoch(key)?;
        let counted = self.admit_mutation(key)?;
        let (fleet, task_key, source) = (self.clone(), key.clone(), source.to_path_buf());
        tokio::spawn(async move {
            // Held to the end, so `close_all` also sees the open of the ship.
            let _counted = counted;
            fleet
                .restore_file_owned(&task_key, &source, &opts, epoch)
                .await
        })
        .await
        .map_err(task_failed(&key.name(), "restore"))?
    }

    async fn restore_file_owned(
        &self,
        key: &FleetDbKey,
        source: &Path,
        opts: &LocalRestore,
        epoch: Option<crate::lock::FencingToken>,
    ) -> Result<LocalRestoreOutcome, FleetError> {
        // A cheap refusal before the copy.
        self.check_tombstone(key, opts).await?;
        // The copy does not read the live file, so openers keep working
        // while it runs.
        let (fleet, task_key, task_source) = (self.clone(), key.clone(), source.to_path_buf());
        let (rekey, migrate) = (opts.allow_rekey, opts.apply_migrations);
        let staged = crate::time::spawn_blocking(move || {
            fleet.stage_restore(
                &task_key,
                &task_source,
                rekey,
                migrate,
                epoch.map(crate::lock::FencingToken::get),
            )
        })
        .await
        .map_err(task_failed(&key.name(), "restore"))??;
        let swap = self.swap_in_backup(key, &staged.file, opts, epoch);
        let swapped = self.with_exclusive(key, swap).await;
        discard_staging(&staged.file);
        let (replaced, tombstone_cleared) = swapped?;
        self.count_migrations(staged.applied);
        let mut outcome = LocalRestoreOutcome {
            replaced,
            rekeyed_from: staged.rekeyed_from,
            migrations_applied: staged.applied,
            tombstone_cleared,
            shipped: None,
            ship_error: None,
        };
        match self.ship_restored(key).await {
            Ok(shipped) => outcome.shipped = shipped,
            Err(error) => {
                tracing::warn!(database = %key, %error, "fleet restore: the file is in place, but its new base snapshot did not ship");
                outcome.ship_error = Some(error.to_string());
            }
        }
        tracing::info!(database = %key, "fleet database restored from a local file");
        Ok(outcome)
    }

    /// Refuse a key with a tombstone, unless the caller clears it. Returns
    /// whether the key has a tombstone.
    async fn check_tombstone(
        &self,
        key: &FleetDbKey,
        opts: &LocalRestore,
    ) -> Result<bool, FleetError> {
        let tombstoned = self.deleted_in_replica(key).await?;
        if tombstoned && !opts.clear_tombstone {
            return Err(FleetError::Restore {
                name: key.name(),
                detail: "the key was deleted and its tombstone stands; set \
                         LocalRestore::clear_tombstone to restore it"
                    .to_owned(),
            });
        }
        Ok(tombstoned)
    }

    /// Steps 6 and 7 of [`restore_from_file`](Self::restore_from_file). The
    /// caller holds the key out of service. Returns whether a database was
    /// there, and whether the tombstone went.
    async fn swap_in_backup(
        &self,
        key: &FleetDbKey,
        staging: &Path,
        opts: &LocalRestore,
        epoch: Option<crate::lock::FencingToken>,
    ) -> Result<(bool, bool), FleetError> {
        self.with_provision_turn(key, async {
            // Again under the drain: a delete can have run since the copy.
            let tombstoned = self.check_tombstone(key, opts).await?;
            // The copy carries the epoch read at the start. A slot lost
            // since then must not get a file stamped for it.
            if epoch.is_some() && self.owned_epoch(key)? != epoch {
                return Err(FleetError::NotOwner {
                    name: key.name(),
                    slot: key.routing_slot(),
                });
            }
            if let Some(replication) = self.replication() {
                // A ship still in flight on the old file writes nothing more.
                let (worker, task_key) = (Arc::clone(replication), key.clone());
                let _ = crate::time::spawn_blocking(move || worker.quiesce_released(&task_key))
                    .await;
            }
            // Take the row before the swap: the replica task drops a row
            // next to a live file without the purge.
            let taken = if tombstoned {
                self.take_tombstone(key).await?
            } else {
                None
            };
            let (root, path, task_staging, name) = (
                self.root().to_path_buf(),
                self.path_of(key),
                staging.to_path_buf(),
                key.name(),
            );
            let swapped = crate::time::spawn_blocking(move || {
                check_contained(&root, &path)?;
                swap_in_staged(&root, &path, &name, &task_staging)
            })
            .await
            .map_err(task_failed(&key.name(), "restore"))
            .and_then(|r| r);
            let replaced = match swapped {
                Ok(replaced) => replaced,
                Err(error) => {
                    // The undo replica stays, so the row goes back.
                    if let Some(row) = &taken {
                        self.put_back_tombstone(key, row).await;
                    }
                    return Err(error);
                }
            };
            // A provision that has not reported its creation did not make
            // this file.
            self.lock_state().unacknowledged.remove(key);
            // The new file has not shipped yet, so the purge cannot reach it.
            let cleared = match &taken {
                Some(row) => match self.purge_taken(key, row).await {
                    Ok(()) => true,
                    Err(error) => {
                        tracing::warn!(database = %key, %error, "fleet restore: the old replica is not purged; its objects are older than the new base");
                        false
                    }
                },
                None => false,
            };
            Ok((replaced, cleared))
        })
        .await
    }

    /// Step 9: open the restored database and ship a new base snapshot.
    /// `None` without replication.
    async fn ship_restored(&self, key: &FleetDbKey) -> Result<Option<ShipReport>, FleetError> {
        if self.replication().is_none() {
            return Ok(None);
        }
        let db = self.open_with(key, false).await?;
        let shipped = self.ship_replica(key).await;
        drop(db);
        shipped.map(Some)
    }
}
