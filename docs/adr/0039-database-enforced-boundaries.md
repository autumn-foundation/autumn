# ADR 0039: Enforce Table Ownership in Postgres

- Status: Accepted
- Date: 2026-11-05
- Deciders: Autumn maintainers
- Tags: architecture, database, service-extraction

## Context

Issue #3550, part of #3548. ADR 0035 checks table ownership on the
architecture graph. The graph cannot see SQL built at run time, or a query in
a helper. Postgres sees every query, and can refuse one. The check adds no work to a
query.

## Decision

- **Config.** A boundary in `boundaries.toml` gets `schema` and an optional
  `role` (default: the boundary name). Only a boundary with `schema` takes
  part. The connection of a role goes in `autumn.toml`, under
  `[database.roles.<boundary>]`, because the runtime reads it.
- **Migrations.** A migration can name its schema. For a set that does not,
  `migrations_in_schema` and `plugin_migrations_in_schema` wrap each migration:
  `CREATE SCHEMA IF NOT EXISTS`, then the schema first on `search_path`, then
  the old path back. The old path stays after the schema, so Diesel's version
  table in `public` still resolves. A registered set carries its schema as a
  fourth tuple field.
- **Grants.** `autumn_web::graph::grants` is a pure planner. It takes the
  config and the live catalog, and gives one SQL transaction: revoke all
  privileges and default privileges for each role in every schema, then
  grant. Revoke-first makes the database match
  the config after an exception is removed. A glob exception resolves against
  the catalog. An unqualified name in two schemas is an error.
- **Pools.** `AppState::pool_for::<B>()` reads a `BoundaryPools` extension.
  With no role for `B`, it returns the primary pool, so the feature is opt-in.
- **Doctor.** `autumn doctor` connects as each role. It reads each granted
  table, and each table of another boundary that the role must not read, in a
  read-only transaction that is rolled back. A PL/pgSQL block turns
  `insufficient_privilege` into a fixed marker, so the result does not depend
  on `lc_messages`. A failed check is a warning, so `--strict` fails.

## Consequences

- One login role per boundary. A pooler keeps a pool per (database, user)
  pair, so the connection count grows with the roles. The guide shows the
  setup for PgBouncer, RDS Proxy and Supavisor.
- `autumn db grants` does not create roles or set passwords.
- `autumn schema diff` still reads `public` only.

## Alternatives

- **One login and `SET ROLE` per boundary.** No extra server connections. But
  `SET ROLE` is session state: through a transaction pooler, it can reach
  another client. Rejected.
- **Row-level security.** It filters rows, not tables, and needs a policy per
  table. A schema and a role are simpler and match the later split into
  databases.
- **Grants in `autumn migrate`.** The migration step would need
  `boundaries.toml`, which the runtime does not read. A separate command keeps
  the migrator unchanged. The deploy job runs both.
