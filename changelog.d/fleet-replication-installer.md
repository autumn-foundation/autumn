### Added

- **fleet:** `autumn_web::fleet::install_replication` connects a fleet to
  `[replication]` at boot (issue #3768). An engine plugin calls it for a
  fleet from `DatabaseFleet::with_engine`, and gets the same parts as the
  `SQLite` fleet: the replica, the tombstones, the `replication:fleet`
  indicator and the loop. A standby then restores a missing database on
  takeover. One replica root holds one fleet.

### Fixed

- **fleet:** a fleet with ownership and no replica now logs one warning: a
  standby that becomes the owner opens empty databases (issue #3768). The
  installer logs it at boot. A fleet whose plugin installs no replica logs
  it at its first open.
