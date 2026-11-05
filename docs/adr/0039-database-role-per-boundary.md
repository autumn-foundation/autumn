# ADR 0039: A Postgres Schema and Role per Boundary

- Status: Accepted
- Date: 2026-11-05
- Deciders: Autumn maintainers
- Tags: database, service-extraction, security

## Context

Issue #3550, part of #3548. The boundary check of ADR 0035 reads the
architecture graph. It cannot see SQL built at run time, or a query in a
helper. The database sees every query. A Postgres role per boundary makes the
database refuse a query on a table that the boundary does not own.

## Decision

- **Config split.** The boundary keeps its tables, its `schema` and its
  `role` in `boundaries.toml`. The connection of a role goes in
  `[database.roles.<boundary>]` in `autumn.toml`, because the runtime reads
  it and its password is a secret.
- **Opt-in.** A boundary with no `role` gets no grants. An app with no
  `[database.roles]` keeps one pool, and `pool_for` gives it.
- **No fallback.** When the config has roles but not the one asked for,
  `pool_for` gives `None`. A boundary never runs as the owner role by
  mistake.
- **Grants from the live catalog.** `owns` holds globs, and a `GRANT` needs
  table names. `autumn db grants` reads `pg_class` and resolves the globs in
  Rust, with the specificity rules of ADR 0035. A pure planner
  (`autumn_web::graph::grants`) makes the plan; the CLI does the IO.
- **Reconcile, do not add.** Each role loses every privilege on every table,
  sequence and schema, then gets its grant back, in one transaction. A stray
  grant goes away on the next apply.
- **The schema on the role.** `ALTER ROLE ... SET search_path` puts the
  boundary schema first. A session `SET` does not hold behind a pooler in
  transaction mode.
- **Migrations in a schema.** A wrapper migration runs `CREATE SCHEMA IF NOT
  EXISTS` and puts the schema first on the `search_path`: `SET LOCAL` in a
  transactional migration, a session setting and `RESET` in another. The
  rest of the path stays, so `__diesel_schema_migrations` stays in `public`.
- **Doctor reads, not trusts.** `autumn doctor --strict` connects as each
  role and tries `SELECT 1 FROM t LIMIT 0` on every table, and reads
  `has_table_privilege` for the other rights. A table outside the grant that
  the role can read or change fails. This finds a grant that `REVOKE` cannot
  remove, for example one to `PUBLIC`.
- **Only a boundary role.** `--apply` refuses a role that is a superuser,
  the role that runs the command, or a table owner. A `REVOKE` on it takes
  the rights of the owner, or has no effect.
- **Views by pattern only.** A view runs with the rights of its owner. A
  schema does not give a boundary its views; an `owns` pattern does.
- **One registration.** A migration registered with a schema and without
  one stops the boot. The first copy would run, in the wrong schema.

## Consequences

- A new table has no grant until the next `autumn db grants --apply`. The
  boundary cannot read it, which is the safe default.
- Each role pool opens its own connections. The boot budget counts them.
- A repository uses the primary pool. Boundary code uses `BoundaryDb` or
  `pool_for`.
- `autumn migrate` does not see an embedded schema set, as it does not see a
  plugin set today.

## Alternatives

- **A `DO` block with `LIKE` patterns.** It needs no catalog read in Rust,
  but `LIKE` is not the glob of ADR 0035, and the specificity rule cannot be
  written in it.
- **A schema grant only** (`GRANT ... ON ALL TABLES IN SCHEMA`). It cannot
  express `may_read` on one table of another schema.
- **Make the roles in `autumn db grants`.** A role has a password, so the
  command prints the `CREATE ROLE` statement and stops.
