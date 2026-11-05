### Added

- **db:** `AppBuilder::plugin_migrations_for(name, MigrationScope, set)`. A
  plugin can keep its set on the control database or on the shards and fleet
  files (#3536).
- **db:** `MigrationScope::directory`. The CLI reads the scope of a set from
  its directory: `migrations/` (`Everywhere`), `migrations-control/`
  (`Control`), `migrations-shards/` (`Shards`). `autumn fleet migrate` applies
  `migrations/` and `migrations-shards/`. `autumn migrate` applies
  `migrations/` and `migrations-control/` to a `SQLite` control database
  (#3536).

### Changed

- **db:** with no `[[database.shards]]` and no `[database.fleet]`, the one
  database gets every scope, at boot, with `AUTUMN_MIGRATE=1` and from
  `autumn migrate`. Before, a `MigrationScope::Shards` set went to no
  database and boot logged a warning (#3536).
- **db:** `autumn migrate` stops before it applies anything when a Postgres
  target and a `migrations-control/` or `migrations-shards/` directory
  exist. Use `AUTUMN_MIGRATE=1` (#3536).
