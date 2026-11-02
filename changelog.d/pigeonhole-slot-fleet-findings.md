### Fixed

- **db:** a `Db` held past `database.slow_query_threshold` no longer logs
  `slow database query` with `sql: "SELECT ?"`. It logs `database connection
  held past slow_query_threshold` with `route` and `held_ms`, inside the
  connection span. A log filter on `slow database query` no longer sees
  these holds.
- **db:** the boot connection warning on SQLite gives the open-file count:
  `2 × connections + databases` (database and `-wal` per connection, one
  `-shm` per database). A fleet counts `max_open` databases. The warning
  also says that `database.fleet.max_open` is a soft limit.
- **db:** on a SQLite build, the `db.connection` and `db.transaction` spans
  set `db.system = "sqlite"`, not `"postgresql"`. The `db.connection` span
  also records `db.pool`.

### Documentation

- **db:** the SQLite fleet guide says that a slot fleet takes any routing
  key, so a move from tenant to slot mode loses the id check. Check ids with
  `TenantDbId::parse`. It also says what a held connection logs, and to take
  `LazyDb` for a fan-out.
