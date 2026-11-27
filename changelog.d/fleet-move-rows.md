### Added

- **fleet:** `DatabaseFleet::replica_bound` and
  `FleetReplication::replica_bound` give the replica bound with its epoch
  and the start of its file (`since`). `ReplicaBound::coverage(committed_at,
  epoch)` answers `Covered`, `Pending` or `Stale`. `Stale` means another
  epoch, or a commit that is not newer than the file. A rewind can have
  removed such a commit, even when its time is at or before the bound.

### Documentation

- **fleet:** "Move rows between fleet databases" in
  `docs/guide/sqlite-fleet.md` (#3771). Do not delete the source copy on the
  destination's acknowledgement: a takeover can rewind the destination and
  lose the row. Delete it only when the destination's bound is `Covered` for
  the arrival, deliver again when it is `Stale`, and keep tombstones.
  `verification/fleet_move.rs` proves that the rule loses no row.
