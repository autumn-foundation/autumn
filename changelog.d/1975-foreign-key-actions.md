### Added

- **schema:** `#[references]` accepts `on_delete` and `on_update` (issue
  #1975). The values are `cascade`, `restrict`, `set_null`, `set_default` and
  `no_action`. `autumn schema diff` writes the action. `schema pull` reads it
  back on Postgres and SQLite. `schema doctor` reports a different action as
  drift. The macro rejects an unknown value. It also rejects `set_null` and
  `set_default` on a field that is not `Option<_>`.

### Breaking Changes

- **Breaking:** **schema:** `autumn_schema_core::ForeignKey` gains the public
  fields `on_delete` and `on_update`. A struct literal needs the two fields,
  or use `ForeignKey::new` ([migration guide](docs/migrations/next.md)).
- **Breaking:** **schema:** a snapshot with a foreign-key action is written as
  `snapshot_version` 2. An older CLI refuses it with "unsupported
  snapshot_version", so it cannot drop the action in silence. Upgrade the CLI
  ([migration guide](docs/migrations/next.md)). A snapshot without an action
  stays at version 1.

### Changed

- **schema:** `schema diff` refuses a changed `ON DELETE` / `ON UPDATE` action
  on an existing foreign key, as it refuses a new target (issue #1975). The
  error shows the two keys. A bare `#[references]` is `NO ACTION`. If the
  database has `ON DELETE CASCADE`, declare `on_delete = "cascade"`.
- **schema:** on SQLite, `schema diff` refuses a table-recreate of a table that
  a table (also the same table) references with `ON DELETE CASCADE`, `SET NULL` or
  `SET DEFAULT`. The migration transaction ignores `PRAGMA foreign_keys=OFF`,
  so the recreate would delete or change the child rows.

### Fixed

- **schema:** `schema pull` lost the `ON DELETE` / `ON UPDATE` action of a
  foreign key. A later SQLite recreate, or the `down.sql` of a dropped table,
  then wrote `NO ACTION`. Run `autumn schema pull` again to record the
  actions in an existing snapshot.
- **schema:** on SQLite, `autumn schema pull` no longer records the shadow
  tables of a non-FTS virtual table as app tables (issue #1975). Examples are
  the rtree `_node`, `_parent` and `_rowid` tables. An app table such as
  `boxes_data` beside an rtree `boxes` is no longer skipped.
