### Added

- **fleet:** `FleetReplication::replicated_through` and
  `DatabaseFleet::replicated_through` give an instant for each database
  (issue #3658). Every commit at or before it is in the replica.
  `wait_replicated` waits for it. Delete the source row of a move only after
  the new row is in the replica. Then a takeover cannot lose the row.
  `ReplicationSnapshot::replicated_through` gives the same bound for one
  replicator.
