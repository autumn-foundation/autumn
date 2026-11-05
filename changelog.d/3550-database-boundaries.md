### Added

- **db:** a Postgres schema and role per table-ownership boundary
  (issue #3550). The database refuses a query on another boundary's table.
  - `schema` and `role` in `boundaries.toml`.
  - `AppBuilder::migrations_in_schema` and
    `AppBuilder::plugin_migrations_in_schema` run a migration set in a
    named schema.
  - `[database.roles.<boundary>]` (`url`, or `user` and `password`, and
    `pool_size`) and `AppState::pool_for::<B>()` give a boundary a pool for
    its own role. With no role, the primary pool is used, as before.
  - `autumn db grants` writes the GRANT and REVOKE SQL from the boundaries
    and the live catalog. `--apply` runs it.
  - `autumn doctor` connects as each role and checks that it cannot read
    another boundary's tables. `--strict` fails on a leak.
  - `autumn db pull --schema <name>` writes `schema.table` in `schema.rs`.
  - See `docs/guide/database-boundaries.md`.
