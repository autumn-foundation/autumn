### Fixed

- **`generate scaffold`:** the migration-history scan now tracks every table,
  so `ALTER TABLE legacy_comments RENAME TO comments` carries the source
  table's columns across instead of landing on an empty column list (issue
  #2282). A polymorphic table renamed into `comments` is recognised as the
  shared comments table and the generator reuses it rather than emitting a
  duplicate `CREATE TABLE comments` that fails on "already exists".
