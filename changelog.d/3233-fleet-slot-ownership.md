### Added

- **db:** slot ownership with epochs and fencing for the SQLite fleet (issue
  #3233). With `[database.fleet.ownership]`, a host claims its slot ranges on
  the control database and renews a lease on them. A key whose slot this host
  does not own answers `421` (`FleetError::NotOwner`), and `restore_missing`
  runs only for an owned key. Each replica generation records its epoch: a
  restore takes the newest epoch, and a replicator that sees a newer epoch
  stops. `/actuator/health` reports `fleet:ownership`. New:
  `autumn_web::db::fleet_ownership`, `DatabaseFleet::install_ownership`,
  `DatabaseFleet::spawn_ownership`, `DatabaseFleet::release_ownership`,
  `DatabaseFleet::owns`,
  `DatabaseFleet::fence_unowned`, `FleetDatabase::epoch` and
  `Replicator::with_epoch`. The built-in table is on the control database, so
  it fences the processes of one host; hosts on different machines implement
  `SlotOwnershipStore`. See "Failover: slot ownership" in the SQLite fleet
  guide.
