### Added

- **db:** a Postgres schema and role per boundary (issue #3550). Opt-in: an
  app with no `role` keeps one role and one schema.
  - `boundaries.toml` boundaries take `schema` and `role`.
  - `[database.roles.<boundary>]` sets `url`, or `user` and `password` on the
    primary URL, and `pool_size`. Env:
    `AUTUMN_DATABASE__ROLES__<NAME>__{URL,USER,PASSWORD,POOL_SIZE}`.
  - `AppState::pool_for::<B>()` and the `BoundaryDb<B>` extractor give the
    pool of the role of boundary `B` (`autumn_web::db_roles::Boundary`).
  - `AppBuilder::migrations_in_schema` and
    `AppBuilder::plugin_migrations_in_schema` run a set in a named schema.
    `autumn_web::migrate::SchemaMigrations` does the same with `run_pending`.
  - `autumn db grants` prints the `REVOKE` / `GRANT` statements of each role,
    from `boundaries.toml` and the live catalog. `--apply` runs them.
  - `autumn doctor --strict` connects as each role. It fails when a role can
    read a table outside its grant.
  - `src/schema.rs` sync keeps the schema of a `marketing.campaigns` block.
  - See `docs/guide/database-boundaries.md`.

### Breaking Changes

- **Breaking:** `DatabaseConfig` has the public field `roles` (issue #3550).
  A struct literal must set it
  ([migration guide](docs/migrations/next.md)).
