### Fixed

- **db:** a `Db` held past `database.slow_query_threshold` no longer logs
  `slow database query` with `sql: "SELECT ?"`. The time covers all work
  while the handler held the connection (a fan-out, for example), not one
  statement. It now logs `database connection held past
  slow_query_threshold` with `route` and `held_ms`, inside the connection
  span. A log filter on `slow database query` no longer sees these holds.
- **db:** the boot connection warning on SQLite gives the open-file count:
  `2 × connections + databases` (the database and its `-wal` for each
  connection, one `-shm` for each database, `max_open` for a fleet). It also
  says that `database.fleet.max_open` is a soft limit.

### Documentation

- **db:** the SQLite fleet guide says that a slot fleet takes any routing
  key, so a move from tenant to slot mode drops the id check. Check ids with
  `TenantDbId::parse`. It also says what a held connection logs, and to take
  `LazyDb` for a fan-out.
