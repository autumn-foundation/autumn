### Added

- **db:** `autumn_web::fleet::resolve::<E>(parts, state)` runs the request
  path of a fleet for an engine plugin's extractor (ADR 0040). It finds the
  routing key, makes the database key, checks the owner (`421 NotOwner`),
  opens the database and returns a `FleetLease<E>`.
  `resolve_key::<E>(state, key)` does the same for jobs and tasks. Core
  ships no generic extractor.
  New `FleetError` variants: `NoRoutingKey` (`400`) and `NoFleet` (`503`).
- **db:** `FleetDatabase::sidecar_replica(engine)` gives a lifecycle hook a
  `ScopedDestination` (`autumn_web::replication`) at `<key prefix>/<engine>/`,
  next to the primary's replica. It refuses keys outside that prefix, and
  writes after the database closes or this host loses the slot. A purge after a delete
  removes the sidecar's objects. The hooks already see the epoch through
  `FleetDatabase::epoch`.
- **db:** `AppBuilder::fleet_ownership_store(store)` replaces the SQL store
  that boot installs for `[database.fleet.ownership]`. `PgSlotOwnership`
  (feature `postgres`) keeps the ownership table in Postgres. `SlotOwnershipStore` has a new
  method `ensure_schema`, with a default that does nothing.
