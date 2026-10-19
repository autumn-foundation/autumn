### Added

- **db:** the SQLite fleet sweeper (issue #3234). Outbox rows, commit hooks
  and derivation backfills now work on fleet databases, also when the
  database is closed. A write marks its database in `_autumn_fleet_pending`
  on the control database. A worker process opens each marked database
  (never past `max_open`), does the work and closes it again.
  `/actuator/health` reports `fleet:sweeper` with the backlog and the oldest
  pending age. New: `DatabaseFleet::open_background`,
  `DatabaseFleet::close_if_unused`, `DatabaseFleet::get_open` and
  `autumn_web::db::fleet_sweeper`.
