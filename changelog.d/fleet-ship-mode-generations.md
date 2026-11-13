### Added

- **db:** `ShipMode::Generations { every }` ships a plugin engine's fleet
  databases to object storage (ADR 0040). Core takes a `snapshot` every
  `every` and at close, and ships only the files whose SHA-256 is not in the
  previous generation. A missing database restores from the newest
  generation. The tombstone rules apply as for `SQLite`. `ShipMode::Rebuild`
  ships nothing: `restore_latest` calls the new `FleetEngine::rebuild`.
- **db:** core checks the fence before each write to a fleet replica, in
  every ship mode (`replication::FencedDestination`). A newer epoch marker
  refuses the write (`FleetError::Fenced`, `421`;
  `DestinationError::Fenced`), and this host loses the slot. WAL shipping and
  sidecar scopes write through it too. The check is one list for each put.
- **db:** `DatabaseFleet::ship_now` and `DatabaseFleet::restore_latest`, for
  every ship mode. `install_replication` works for any engine.
  `FleetEngine::epoch_stamp` lets core restore a stale file of any engine at
  a fail-back. `FleetStats` reports `ship_mode` and `rpo_secs`, and the
  `replication:fleet` health indicator reports `ship_mode`, `rpo_seconds` and
  the lag of each generation.
- **plugins:** the fleet conformance kit adds `fleet-fenced-ship`,
  `fleet-unchanged-files` and `fleet-epoch-stamp`.

### Breaking Changes

- **Breaking:** `ShipMode::Continuous` carries an `Arc<dyn
  ContinuousShipper>`, and `ShipMode` is no longer `Copy`, `PartialEq` or
  `Eq`. Match with `ShipMode::Continuous(_)`. `ContinuousShipper` is sealed:
  a plugin engine returns `ShipMode::Generations { every }`
  ([migration guide](docs/migrations/next.md)).
