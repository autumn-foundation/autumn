### Fixed

- **db (fleet):** two processes that share a fleet root and a control
  database no longer lose a replica tombstone (issue #3516). A create on
  demand now answers `410` while the delete is still pending, not only
  during the retention. This is also true with no retention: when the
  delete cannot read the replica, the create waits up to about 10 minutes
  for the retry task. Only `provision` ends the undo window early. Two
  deletes of one file at the same time now keep its tombstone.
- **db (fleet):** new API in `autumn_web::db::fleet_tombstones`:
  `create_on_demand_allowed`, `FleetTombstones::checkout`,
  `FleetTombstones::record_on` and `ControlConnection`.
