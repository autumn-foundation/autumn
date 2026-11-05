# Database Boundaries: a Schema and a Role per Boundary

[Table ownership boundaries](table-ownership.md) find a cross-boundary query in
the code. A static check cannot see SQL that the app builds at run time, or a
query in a helper. The database sees every query. Use this guide to make
Postgres refuse a query on the table of a different boundary.

Each boundary gets:

* a Postgres **schema** for its tables, for example `marketing.campaigns`;
* a Postgres **role** that may use its own schema, plus the tables that its
  `may_read` and `may_write` lists name;
* a **pool** in the app that connects as that role.

The feature is opt-in. An app with no `schema` in `boundaries.toml` and no
`[database.roles]` keeps one role and one schema, as before. Schemas and roles
need Postgres.

```console
$ autumn doctor --strict
✓ database_roles: 2 role(s) read only their grants
```

## 1. Give each boundary a schema

Add `schema` to the boundary in `boundaries.toml`:

```toml
[boundaries.core]
owns = ["contacts", "deals", "notes"]
schema = "core"

[boundaries.marketing]
owns = ["campaigns", "attribution_*"]
schema = "marketing"
role = "marketing_app"   # optional; the default is the boundary name
may_read = ["contacts"]
may_write = ["core.deals"]
```

| Key | What it holds |
|-----|---------------|
| `schema` | The Postgres schema of the boundary. All tables in it belong to the boundary. |
| `role` | The Postgres role of the boundary. Default: the boundary name. Needs `schema`. |

Two boundaries cannot share a schema or a role. A name must not be empty, must
not be longer than 63 bytes, and must not start with `pg_`. The role must not
be `public`.

A `may_read` or `may_write` entry is a table name, or `schema.table`. Both
parts are globs. An entry without a schema matches the table name in every
schema. When one name is in two schemas, the plan stops with an error; write
`schema.table`.

## 2. Put the tables in the schema

### App migrations

A migration can name the schema itself:

```sql
CREATE SCHEMA IF NOT EXISTS marketing;
CREATE TABLE marketing.campaigns (id BIGSERIAL PRIMARY KEY, name TEXT NOT NULL);
```

Or register a set of migrations in a schema. Then the SQL needs no schema:

```rust
autumn_web::app()
    .migrations(MIGRATIONS)
    .migrations_in_schema("marketing", MARKETING_MIGRATIONS)
    .run()
    .await;
```

### Plugin migrations

A plugin writes `CREATE TABLE audit_log`. The app chooses the schema `audit`:

```rust
autumn_web::app()
    .plugin_migrations_in_schema("audit-plugin", "audit", audit_plugin::MIGRATIONS)
    .run()
    .await;
```

`migrations_in_schema` and `plugin_migrations_in_schema` run each migration
like this:

1. `CREATE SCHEMA IF NOT EXISTS <schema>`.
2. Put the schema first on `search_path`. An unqualified table name then means
   a table in that schema.
3. Run the migration.
4. Put back the old `search_path`. In a transaction, this occurs at the end of
   the transaction. Outside a transaction (`run_in_transaction = false`), the
   old path is saved first and set again after the migration.

The old path stays after the schema, so Diesel still finds
`__diesel_schema_migrations` in `public`. A `SQLite` app that registers a set in
a schema does not start.

The app applies these sets when it starts, or in its own migrate mode. The CLI
command `autumn migrate` applies the `migrations/` directory with the Diesel
CLI, with no schema. For a set in `migrations/`, write the schema in the SQL.

### `schema.rs`

Diesel `table!` supports a schema:

```rust
diesel::table! {
    marketing.campaigns (id) {
        id -> Int8,
        name -> Text,
    }
}
```

The module is still `campaigns`, so `#[model(table = "campaigns")]` and
`use crate::schema::campaigns` do not change. `autumn db pull --schema
marketing` reads the tables of that schema and writes this form.

## 3. Create the roles

`autumn db grants` does not create roles, because a login role needs a
password. Create each role once, as an administrator:

```sql
CREATE ROLE core LOGIN PASSWORD '…';
CREATE ROLE marketing_app LOGIN PASSWORD '…';
```

The roles must not own the tables, and must not be superusers. An owner or a
superuser passes every privilege check. Run the migrations as another role,
for example the role in `database.url`.

## 4. Generate and apply the grants

```console
autumn db grants                                # print the SQL
autumn db grants --apply                        # run it
autumn db grants --config crm/boundaries.toml   # another boundaries file
autumn db grants --profile prod --apply         # the prod database
```

The command reads `boundaries.toml` and the live catalog, so run it after
`autumn migrate`. The SQL is one transaction:

1. Create each boundary schema if it is missing. Revoke all from `PUBLIC` on
   the schema and on its tables.
2. For each role, revoke all privileges and all default privileges in every
   schema. When you remove an exception, the next run removes its grant.
3. Grant the role `USAGE` on its schema, and `SELECT, INSERT, UPDATE, DELETE`
   on all tables in it. Grant the use of its sequences.
4. Set the default privileges of the schema, so a new table in it is also
   granted. This applies to tables that the role that runs the grants creates.
5. Grant `SELECT` on each `may_read` table. Grant `SELECT, INSERT, UPDATE,
   DELETE` on each `may_write` table and the use of its sequences.

The command prints a warning for:

* an exception that matches no table;
* a table that a boundary `owns` by name, but that is in another schema. The
  boundary's role cannot reach it, and the other roles are checked against it;
* a boundary schema that has no tables yet.

Run `autumn db grants --apply` after each `autumn migrate` in your deploy job.
You can run it again at any time. Each run gives the same result.

The command changes only the roles of the boundaries in the config. When you
remove a boundary, or its `schema`, its role keeps its grants. Remove them
yourself, for example with `DROP OWNED BY <role>`.

## 5. Connect as the role

Add a role connection to `autumn.toml`. Set `user` to use the primary URL with
another login, or set `url`:

```toml
[database]
url = "postgres://app@db:5432/crm"

[database.roles.marketing]
user = "marketing_app"
pool_size = 5

[database.roles.core]
url = "postgres://core@db:5432/crm"
```

| Key | What it holds |
|-----|---------------|
| `url` | The full Postgres URL of the role. |
| `user` | The login. It replaces the user of `database.url`. |
| `password` | The password of `user`. Use the environment override. |
| `pool_size` | The pool size. Default: the primary pool size. |

Set `url` or `user`, not both. Each key has an environment override for a role
that `autumn.toml` declares:

```console
AUTUMN_DATABASE__ROLES__MARKETING__PASSWORD=…
AUTUMN_DATABASE__ROLES__CORE__URL=postgres://core:…@db:5432/crm
```

The key of `[database.roles.<name>]` is the boundary name. Code in the boundary
gets the pool of its role:

```rust
use autumn_web::db_boundary::DbBoundary;

struct Marketing;
impl DbBoundary for Marketing {
    const NAME: &'static str = "marketing";
}

let pool = state.pool_for::<Marketing>().expect("database configured");
let mut conn = pool.get().await?;
```

`state.boundary_pool("marketing")` does the same by name. With no
`[database.roles.<name>]`, both return the primary pool, as before. The pools
connect on first use.

A query on another boundary's table now fails:

```text
ERROR:  permission denied for table notes
```

Change that read to an API call to the boundary that owns the table. Or add
the table to `may_read`.

## 6. Check it with the doctor

`autumn doctor` runs the `database_roles` check when a boundary in
`boundaries.toml` sets `schema`. The check:

1. reads the catalog as the primary role, and makes the grant plan;
2. connects as each role in `[database.roles]`;
3. reads each table that the role must be able to read;
4. reads each table of another boundary that the role must **not** read, and
   expects Postgres to refuse it.

Each read is `SELECT 1 FROM <table> LIMIT 0` in a read-only transaction that
is rolled back. It reads no row and writes nothing. A read waits at most 5
seconds for a lock.

The check reports one line per role. These results are warnings, so
`autumn doctor --strict` fails on them:

* a role reads a table outside its grant (`CAN READ core.notes`);
* a role cannot read a table inside its grant;
* the connection logs in as another role than `boundaries.toml` names;
* a boundary with a schema has no `[database.roles]` entry;
* the doctor cannot connect as a role;
* no database URL is set, so the check cannot run.

A bad grant plan, for example two boundaries with one schema, fails the check.

## Connection poolers

Each role is a separate login, so each role needs its own server connections.
A pooler keeps a pool per (database, user) pair.

**PgBouncer.**

* Add each role to `auth_file`, or set `auth_query` with an `auth_user` that
  may read `pg_shadow`. Then PgBouncer finds the role passwords itself.
* The pool size is per (database, user). With `default_pool_size = 20` and
  three roles, PgBouncer can open 60 server connections. Set
  `max_user_connections` for each user in `[users]`, or set
  `max_db_connections` for the database.
* Transaction mode works. The role is the login, not session state, so it
  cannot leak to another client.
* Do not use one login and `SET ROLE` per boundary through a transaction
  pooler. `SET ROLE` is session state. A later client can get the session with
  the wrong role.

**RDS Proxy.** Add a secret per role to the proxy's authentication. Each role
gets its own connection pool in the proxy.

**Supavisor and the Neon pooler.** The pool is per user. Use a separate
connection string per role.

Count the connections: the sum of the role `pool_size` values, plus the
primary pool, must stay below `max_connections` on the server. See
[Running behind PgBouncer / RDS Proxy](connection-poolers.md).

## Move a boundary to its own database

If a boundary works with its own role, each read from a different boundary
is a grant in `boundaries.toml`, or an API call. To move the boundary to a
different database, copy its schema. Then set `[database.roles.<name>] url` to
the new database. Change each `may_read` table to an API call or a copy.

## Limits

* `autumn db grants` does not create or drop roles, or set passwords.
* A view or a `SECURITY DEFINER` function runs with the rights of its owner.
  A view in the marketing schema that reads `core.contacts` gives the
  marketing role that data. The doctor does not find this. Use
  `security_invoker` views (Postgres 15 and later), or review views in code
  review.
* A role keeps `USAGE` on schema `public` through `PUBLIC`, as Postgres gives
  it by default. A framework table in `public` (jobs, sessions) is readable by
  a boundary role only when you grant it, for example in `may_read`.
* The default privileges apply to tables that the role that runs
  `autumn db grants` creates. Run the migrations and the grants as one role.
* The doctor reads the tables of other boundary schemas, and the tables that a
  boundary `owns` by name in other schemas. It does not read every table in
  `public`.
* `autumn schema diff` reads schema `public` only.

## Related

* [Table Ownership Boundaries](table-ownership.md) — the static check.
* [Running behind PgBouncer / RDS Proxy](connection-poolers.md).
* [ADR 0039](../adr/0039-database-enforced-boundaries.md).
