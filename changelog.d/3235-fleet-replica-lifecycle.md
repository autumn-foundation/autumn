### Added

- **db:** a deleted SQLite fleet database's replica goes with it (issue
  #3235). With `[replication]` on, `DatabaseFleet::delete` writes a tombstone
  row on the control database, removes the files, then purges the replica
  in object storage. When the purge fails, the tombstone stays and a
  background task retries it with backoff. `restore_missing` does not
  restore a tombstoned key; `fleet.restore` and `autumn fleet restore` still
  do, and drop the tombstone. `database.fleet.replica_retention_after_delete`
  (for example `"30d"`) keeps the replica for undo, then purges it. A marker
  object in the replica blocks a restore on hosts that do not share the
  control database. A new database with the same key purges the old replica
  first. Every `[replication] verify_interval_secs`, the fleet restores
  `database.fleet.restore_check_sample` random databases (default `1`) into
  scratch files and checks `PRAGMA integrity_check` and the identity row.
  `/actuator/health` reports both under `replication:fleet`. Duration
  strings accept a `d` (days) suffix. New: `autumn_web::db::fleet_tombstones`,
  `DatabaseFleet::purge_deleted_replicas`,
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
