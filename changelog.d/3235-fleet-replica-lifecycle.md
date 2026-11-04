### Added

- **db:** with `[replication]` on, a SQLite fleet delete also purges the
  database's replica in object storage (issue #3235). `DatabaseFleet::delete`
  and `autumn fleet delete` write a tombstone row on the control database
  first. If the row cannot be written, the delete fails and removes nothing.
- **db:** a failed replica purge does not roll back the local delete. The
  tombstone stays, and the maintenance task tries the purge again with a
  backoff (`DatabaseFleet::purge_deleted_replicas`).
- **db:** `restore_missing` does not restore a deleted database. A marker
  object in the replica stops the restore on hosts that do not share the
  control database. `fleet.restore` and `autumn fleet restore` still restore
  it, and drop the tombstone.
- **db:** `database.fleet.replica_retention_after_delete` (for example
  `"30d"`) keeps the replica for an undo window, then purges it. During the
  window, a create on demand answers `410`; `provision` ends the window.
  Duration strings accept a `d` (days) suffix.
- **db:** a restore check. Every `[replication] verify_interval_secs`, each
  worker-role process restores `database.fleet.restore_check_sample` random
  databases (default `1`) into scratch files. It checks
  `PRAGMA integrity_check` and the identity row. `/actuator/health` reports
  the check and the purges under `replication:fleet`.
- **db:** new API: `autumn_web::db::fleet_tombstones`,
  `DatabaseFleet::check_replica_sample`, `FleetReplication::with_tombstones`,
  `FleetReplication::with_retention_after_delete`,
  `FleetReplication::with_restore_check`, `FleetReplication::with_entropy`,
  `FleetReplication::purge_replica` and `FleetReplication::marker`. See
  "Deleting a database's replica" in the SQLite fleet guide.

### Breaking Changes

- **Breaking:** `DatabaseFleetConfig` has the public fields
  `replica_retention_after_delete` and `restore_check_sample` (issue #3235).
  A struct literal must set them
  ([migration guide](docs/migrations/next.md)).
