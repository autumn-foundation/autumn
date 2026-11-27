### Fixed

- **db:** in `ShipMode::Generations`, each database starts its ship timer at
  a random time in `[0, every)` after its open, then ships each `every`.
  Databases that open together (at boot, after a bulk load, or on a
  takeover) now snapshot, copy and hash at different times in each
  interval. The first ship comes sooner, never later, so the RPO stays
  `every`. A slow epoch marker does not move the phase. The phase comes
  from the replication's `Entropy`: a test that counts generations can set
  `FleetReplication::with_entropy` to a seeded entropy, and it replays the
  same phases.
