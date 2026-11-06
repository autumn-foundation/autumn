### Added

- **db:** `AppBuilder::plugin_migrations_for(name, MigrationScope, set)`. A
  plugin can keep its set on the control database or on the shards and fleet
  files (#3536).
- **db:** with no `[[database.shards]]` and no `[database.fleet]`, the one
  database gets every `MigrationScope`, at boot, with `AUTUMN_MIGRATE=1` and
  from `autumn migrate`. A plugin can register a scoped set and not check the
  topology (#3536).
- **db:** `MigrationScope::directory`. The CLI reads the scope of a set from
  its directory: `migrations/` (`Everywhere`), `migrations-control/`
  (`Control`), `migrations-shards/` (`Shards`). `autumn fleet migrate` applies
  `migrations/` and `migrations-shards/`. `autumn migrate` applies
  `migrations/` and `migrations-control/` to a SQLite control database, and
  all three to the only database. `autumn migrate check` scans all three
  (#3536).
- **db:** `migrate::run_pending_sqlite_sets_with_framework_migrations` applies
  more than one app set to a SQLite database, with one collision map (#3536).

### Changed

- **db:** the CLI now reads `migrations-control/` and `migrations-shards/`.
  When a Postgres target takes one of them, `autumn migrate` stops before it
  applies anything. When one exists, `autumn migrate down` stops. Use
  `AUTUMN_MIGRATE=1` (#3536).
- **db:** the framework control-plane set (`FRAMEWORK_MIGRATIONS`) never
  reaches a shard or a fleet file, whatever scope registers it (#3536).
