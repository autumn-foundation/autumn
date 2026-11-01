### Breaking Changes

- **Breaking:** `DatabaseFleetConfig` has the public field
  `replication_workers`. A struct literal must set it
  ([migration guide](docs/migrations/next.md)).

### Added

- **fleet:** `database.fleet.replication_workers` (default `4`) sets the
  number of threads that ship fleet databases. A pass starts every
  `sync_interval_secs`. The database that shipped longest ago goes first.
  `replication:fleet` reports `ship_workers` and `last_pass_ms`. API:
  `FleetReplication::with_ship_workers`, `FleetReplication::ship_workers`
  and `DEFAULT_SHIP_WORKERS` in `autumn_web::db::fleet_replication`.
- **replication:** `ReplicationError::is_transient`. Only a blocked
  checkpoint is transient.

### Fixed

- **fleet:** one thread shipped every open fleet database, so with many busy
  databases the replication lag went above `rpo_secs` (3 to 14 s against 2 s
  with 150 to 230 databases per host).
- **fleet:** `replication:fleet` reported `unowned` for a database that closed
  and opened again during a pass. The pass kept the old replicator lock until
  it ended.
- **db:** the boot warning for a high connection count spoke of Postgres to a
  SQLite app. It now names `database.fleet.max_open` and the open-file limit.
- **replication:** a blocked checkpoint logs at `debug` the first time. When
  the next tick is also blocked, it logs a warning.

### Documentation

- **fleet:** "Replication lag" in the fleet guide tells when the RPO holds
  and what to monitor. The failover procedure gives the cost of restores and
  the loss at a takeover.
