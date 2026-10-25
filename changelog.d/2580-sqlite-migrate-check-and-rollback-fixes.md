### Fixed

- **migrate:** `autumn migrate check` now reports a SQLite `RENAME a TO b`
  (without `COLUMN`) as `RENAME COLUMN` (issue #2580).
- **migrate:** `autumn migrate check` no longer counts a comma inside quoted
  text as a second `ALTER TABLE` action (issue #2580).
- **generate:** the `down.sql` of a column removal no longer replaces a prior
  `UNIQUE` index with a plain index of the same name. The rollback keeps the
  `UNIQUE` (issue #2580).
