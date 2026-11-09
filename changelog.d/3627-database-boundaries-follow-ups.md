### Added

- **db:** `#[model(schema = "marketing")]` puts a model in a Postgres schema
  (issue #3627). `autumn schema diff --write-migration` writes
  `CREATE SCHEMA IF NOT EXISTS` and qualified DDL. A new model gets a
  qualified `table!` block in `src/schema.rs`. `autumn schema pull --schema
  <name>`, `schema doctor` and `schema diff --dev-url` read the schema.
- **db:** `#[repository(Model, boundary = Type)]` runs the repository as the
  Postgres role of that boundary. Reads stay on the role pool.

### Fixed

- **schema:** the `src/schema.rs` sync renames a block with a spaced header
  (`marketing . articles (`).
