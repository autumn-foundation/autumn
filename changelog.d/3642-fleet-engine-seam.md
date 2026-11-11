### Added

- **db:** the fleet core does not depend on the database engine (issue
  #3642, ADR 0040). The new `FleetEngine` trait opens, closes, copies and
  removes one database.
  `DatabaseFleet<E: FleetEngine>` keeps keys, paths, the path lock,
  single-flight opens, eviction, leases, ownership and fencing. With
  `sqlite`, `SqliteEngine` is the default engine, so `DatabaseFleet` still
  means the `SQLite` fleet. The core, `fleet_ownership` and the tombstone
  rules now compile with `db`, without `sqlite`.
- **db:** `[database.fleet] max_resident_bytes` (default `0`, off): a soft
  memory budget next to `max_open`. Eviction counts what each engine reports
  for an open database. `FleetStats` reports `resident_bytes` and
  `max_resident_bytes`.
- **plugins:** `plugin_conformance::fleet` checks a `FleetEngine`: identity
  and epoch refusals, open after close, a snapshot restore, a delete and a
  refused restore, and the memory budget. CI runs it against `SqliteEngine`.

### Breaking Changes

- **Breaking:** `DatabaseFleetConfig` has a new field `max_resident_bytes`,
  and `FleetStats` has the new fields `resident_bytes` and
  `max_resident_bytes`. An open now refuses a database that records a newer
  ownership epoch (`FleetError::NewerEpoch`, `421`)
  ([migration guide](docs/migrations/next.md)).
