### Fixed

- **`generate scaffold`:** two false positives in the migration-history scan
  behind the shared comments-table check (issue #3035). Whitespace around the
  schema dot (`CREATE TABLE archive. legacy_comments (…)`) no longer parses
  the schema as the table name, so a later `RENAME TO comments` keeps the
  table in its own schema instead of carrying its columns into the
  default-schema record. And an `ALTER TABLE … ADD` action now adds exactly
  the column it declares (`ADD COLUMN author_id BIGINT REFERENCES users(id)`
  adds `author_id` only — the `(id)` in the REFERENCES clause is a mention,
  not a declaration), instead of reading every discriminator name the
  statement mentions as added.
