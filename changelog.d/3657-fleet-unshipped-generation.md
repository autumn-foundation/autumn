### Fixed

- **db:** in `ShipMode::Generations`, core no longer drops a close-time
  generation that did not ship (a failed put, a passed shutdown cutoff, or
  a lapse of the lease). Core keeps it as unshipped work. The replication
  loop and the shutdown flush try the ship again. A delete, a restore or a
  newer epoch drops it. A failed delete or restore keeps it. While one is
  kept, `DatabaseFleet::release_ownership` lets the ranges lapse: it does
  not hand them on early. The `replication:fleet` health details name it
  in `unshipped`.

### Added

- **db:** `FleetReplication::unshipped_generations` counts the closed
  databases whose last generation did not ship.
