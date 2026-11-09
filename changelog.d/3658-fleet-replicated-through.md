### Added

- **fleet:** `FleetReplication::replicated_through` and
  `DatabaseFleet::replicated_through` give, per database, the instant up to
  which every commit is in the replica (issue #3658). `wait_replicated`
  waits for it. Use it to delete the source row of a move only when the new
  row is in the replica, so a takeover cannot lose it.
  `ReplicationSnapshot::replicated_through` is the same bound for one
  replicator.
