### Fixed

- **migrate:** `autumn migrate check` now flags a SQLite `RENAME a TO b`
  (without `COLUMN`) as `RENAME COLUMN`. A comma in a string literal no
  longer counts as a second `ALTER TABLE` action. A `down.sql` that restores
  a removed column keeps the `UNIQUE` of a prior index that has the
  conventional name (issue #2580).
