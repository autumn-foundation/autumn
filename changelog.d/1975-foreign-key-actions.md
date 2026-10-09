### Added

- **schema:** `#[references]` accepts `on_delete` and `on_update`
  (`cascade`, `restrict`, `set_null`, `set_default`, `no_action`) (issue
  #1975). `autumn schema diff` writes the action, `schema pull` reads it back
  on Postgres and SQLite, and `schema doctor` reports a different action as
  drift. The macro rejects an unknown value, and `set_null` on a field that is
  not `Option<_>`. Before, a pulled `ON DELETE CASCADE` became `NO ACTION` when
  the table was made again.

### Fixed

- **schema:** on SQLite, `autumn schema pull` no longer records the shadow
  tables of a non-FTS virtual table (for example rtree `_node`, `_parent` and
  `_rowid`) as app tables (issue #1975).
