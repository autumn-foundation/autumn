### Added

- **db:** the fleet lake has a sink seam (issue #3645, ADR 0040 §6). Core
  runs the inclusion rule: the listing, the tombstones, `as_of` and the
  restore. A `LakeSink` builds the lake from each included member: its key,
  its `LakeGeneration` and its restored file. Core checks the identity row
  of each restored file before the sink gets it. `FleetLake::load(sink)`
  runs any sink and gives a `LakeLoad`. `materialize()` runs the `SQLite` sink, as
  before. `autumn fleet lake` does not change.
- **db:** `LakeSink::has` lets a sink keep its lake between loads. Core then
  restores only the members whose generation or WAL position changed. A sink
  with `TAKES_SLOTS = true` gets slot databases. The `SQLite` sink still
  refuses slot fleets. A sink keeps its catalog in files under its own
  prefix. Core adds no lake table to the control database.
