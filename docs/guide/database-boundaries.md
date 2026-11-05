# Database Boundaries: a Schema and a Role per Module

[Table ownership boundaries](table-ownership.md) find a cross-boundary query
in the code. They cannot see SQL that is built at run time, or a query in a
helper. The database sees every query. Give each boundary its own Postgres
role, and the database refuses a query on a table that the boundary does not
own.

```console
$ psql "postgres://marketing@db/app" -c 'SELECT * FROM deals'
ERROR:  permission denied for table deals
```

When a boundary works with its own role, a later move to its own database is
a URL change. Each read that it still needs from another boundary is an API
call or a `may_read` grant, because the database refused all other SQL.

This is opt-in. An app with no `role` and no `[database.roles]` keeps one role
and one schema.

## 1. Give the boundary a schema and a role

Add `schema` and `role` to the boundary in `boundaries.toml`:

```toml
# boundaries.toml
[boundaries.core]
owns = ["contacts", "deals"]
crates = ["crm-core"]
role = "core"

[boundaries.marketing]
owns = ["campaigns", "attribution_*"]
crates = ["crm-marketing"]
schema = "marketing"
role = "marketing"
may_read = ["contacts"]
```

| Key | What it does |
|-----|--------------|
| `schema` | The Postgres schema of the owned tables. When unset, `public`. |
| `role` | The Postgres role of the boundary. Only a boundary with a `role` gets grants. |

* An `owns` pattern matches a table in the schema of its boundary. A pattern
  with a `.` (`marketing.campaigns`) matches one schema and table.
* A table (not a view) in a schema other than `public`, that no pattern
  matches, belongs to the one boundary with that schema.
* A view belongs to a boundary only through an `owns` pattern. A view runs
  with the rights of its owner, so it can read tables outside the grant. Give
  such a view `security_invoker = true` (Postgres 15 and later).
* An exception pattern (`may_read`, `may_write`) with no `.` matches the
  table name in every schema, also in the schema of another boundary. Write
  `public.contacts` to match one table.
* Schema names match without case.

## 2. Put the tables in the schema

A migration that names the schema works as it is:

```sql
CREATE SCHEMA IF NOT EXISTS marketing;
CREATE TABLE marketing.campaigns (id BIGSERIAL PRIMARY KEY, name TEXT NOT NULL);
```

An embedded set can go into a schema at boot. Each migration of the set runs
with the schema first on the `search_path`. The schema is made when it does
not exist. So `CREATE TABLE campaigns` makes `marketing.campaigns`:

```rust
const MARKETING: EmbeddedMigrations = embed_migrations!("migrations-marketing");

autumn_web::app()
    .migrations(MIGRATIONS)
    .migrations_in_schema("marketing", MARKETING)
    .plugin_migrations_in_schema("my-plugin", "admin", my_plugin::MIGRATIONS)
    .run()
    .await;
```

`plugin_migrations_in_schema` puts the tables of a plugin into the schema of
the boundary that owns them. The plugin SQL does not change.

* Use it for a plugin that does not register its own migrations. When a
  plugin registers the same set with `plugin_migrations`, the boot stops: one
  migration is then registered with a schema and without one.
* The plugin code must find its tables. Run it as the boundary role, or put
  the schema on the `search_path` of the owner role too.
* The role that runs the migrations needs `CREATE` on the database. Postgres
  checks it before `IF NOT EXISTS`, also when the schema exists.

* `autumn migrate` runs `migrations/` only. It does not see an embedded set.
  Run the embedded sets with `AUTUMN_MIGRATE=1`, or at boot.
* SQLite has no schemas. A schema set on SQLite stops the boot with an error.
* `autumn_web::migrate::SchemaMigrations` runs a set in a schema with
  `run_pending`, for a test or a tool.

## 3. Declare the table in its schema

A `diesel::table!` block can name the schema. The module stays `campaigns`,
so a `#[model(table = "campaigns")]` does not change. Diesel writes
`"marketing"."campaigns"` in each query:

```rust
diesel::table! {
    marketing.campaigns (id) {
        id -> Int8,
        name -> Text,
    }
}
```

`autumn schema diff --write-migration` keeps the schema of a block when it
writes `src/schema.rs`. The width gate (`autumn schema width`) reads a
qualified block too.

Some framework features write raw SQL with the bare table name. The grants
put the schema first on the `search_path` of the boundary role, so these
features find the table when they run as that role. On the primary pool (a
repository, for example) the owner `search_path` applies, and a bare
`campaigns` in `marketing` is not found. Set it on the owner role too:

```sql
ALTER ROLE app_owner SET search_path = "$user", public, marketing;
```

## 4. Connect as the role

Each boundary connects with `[database.roles.<boundary>]` in `autumn.toml`:

```toml
[database]
url = "postgres://app_owner@db/app"

[database.roles.marketing]
user = "marketing"      # the host and database of database.url
# password: AUTUMN_DATABASE__ROLES__MARKETING__PASSWORD

[database.roles.core]
url = "postgres://core:secret@db/app"
pool_size = 4
```

| Key | What it does |
|-----|--------------|
| `url` | A full Postgres URL for the role. |
| `user` | The role name, on the host and database of `database.url`. |
| `password` | The password for `user`. The password of `database.url` is not used. |
| `pool_size` | The pool size. When unset, `database.pool_size`. |

Set `url` or `user`, not both. The name after `roles.` is the boundary name.
It matches `[a-z0-9_-]`. With `user`, a `user`, `password` or `passfile` in
the query of `database.url` is removed, so the role does not connect as the
owner. `user` needs `database.url`: a URL that a pool provider finds at boot
is not used. Set `url` then.

Each key has an env var: `AUTUMN_DATABASE__ROLES__<NAME>__URL`, `__USER`,
`__PASSWORD` and `__POOL_SIZE`. `<NAME>` is the name in upper case, with `-`
as `_`. `AUTUMN_DATABASE__ROLES=marketing,core` declares roles that
`autumn.toml` does not have.

Code in the boundary gets the pool of its role:

```rust
use autumn_web::db_roles::Boundary;

pub struct Marketing;

impl Boundary for Marketing {
    const NAME: &'static str = "marketing";
}

// A pool:
let pool = state.pool_for::<Marketing>().expect("marketing pool");

// Or a connection in a handler. `db` is a `Db` that connects as `marketing`.
#[get("/campaigns")]
async fn campaigns(mut db: autumn_web::db::BoundaryDb<Marketing>) -> AutumnResult<String> {
    let conn: &mut Db = &mut db;
    Ok("ok".to_owned())
}
```

| Config | `pool_for::<Marketing>()` |
|--------|---------------------------|
| No `[database.roles]` | The primary pool. The app has one role. |
| `[database.roles.marketing]` | The pool of `marketing`. |
| Other roles, no `marketing` | `None`, and one warning. The boundary does not fall back to the owner role. `BoundaryDb` answers 503. |

Each role pool opens its own connections. The boot log adds them to
`total_max_connections`, and `database.max_connections_warn_threshold`
counts them. `TestApp` builds the role pools of its config too.

## 5. Generate the grants

`autumn db grants` reads `boundaries.toml` and the live catalog, and prints
one transaction:

```console
$ autumn db grants
-- Generated by `autumn db grants` from boundaries.toml. Do not edit.
BEGIN;

-- Boundary `marketing`: role "marketing", schema "marketing".
REVOKE ALL ON ALL TABLES IN SCHEMA "marketing", "public" FROM "marketing";
REVOKE ALL ON ALL SEQUENCES IN SCHEMA "marketing", "public" FROM "marketing";
REVOKE ALL ON SCHEMA "marketing", "public" FROM "marketing";
GRANT USAGE ON SCHEMA "marketing", "public" TO "marketing";
GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE "marketing"."campaigns" TO "marketing";
GRANT SELECT ON TABLE "public"."contacts" TO "marketing";
GRANT USAGE, SELECT ON SEQUENCE "marketing"."campaigns_id_seq" TO "marketing";
ALTER ROLE "marketing" SET search_path = "marketing", "public";

COMMIT;
```

* Each role first loses every privilege on every table, sequence and schema.
  Then it gets its own tables, its `may_write` tables (read and write), and
  its `may_read` tables (read only). So a grant that the config does not have
  goes away.
* A sequence of an own or `may_write` table gets `USAGE, SELECT`, so an
  insert can use it. This includes a sequence that a column default calls
  with `nextval`.
* A role in a named schema gets that schema first on its `search_path`. A
  role in `public` gets `RESET search_path`, so an old schema goes away.
* A schema with no table gets no statement. A `REVOKE` on a missing schema
  would stop the transaction.
* The `owns` globs resolve against the live tables, with the same rules as
  the graph check. Plugin tables come from `cargo metadata`.

Run it after each migration, with the URL that runs the migrations:

```console
autumn migrate
autumn db grants --apply
```

`--apply` stops when a role does not exist, and prints the `CREATE ROLE`
statement. The command does not make roles, because a role has a password.
It also stops when a role is a superuser, is the role that runs the command,
or owns tables. A `REVOKE` on such a role takes the rights of the owner, or
has no effect.
`--json` prints the plan per role. `--config` names another
`boundaries.toml`. `--profile` selects the database URL.

A new table that a migration makes has no grant until the next
`autumn db grants --apply`. So the safe state is the default: the boundary
cannot read it.

## 6. Check it with doctor

`autumn doctor --strict` (or `--online`) connects as each role. It tries
`SELECT 1 FROM <table> LIMIT 0` on each table, and reads
`has_table_privilege` for the other rights:

* Each table of the grant must open.
* Each other table must give `permission denied`, and the role must have no
  other right on it (`INSERT`, `UPDATE`, ...). This is the negative check. It
  finds a grant that `REVOKE` cannot remove, for example a grant to `PUBLIC`
  or through role membership.
* A `may_read` table must not be writable.
* The role should not create objects in a schema, and should not be a member
  of another role.

Each check connection has `statement_timeout = 5s` and `lock_timeout = 2s`,
so a migration that locks a table does not stop doctor.

```text
✓ database_roles
  marketing: 2 of 2 granted table(s) readable; 3 of 3 other table(s) denied
  core: 2 of 2 granted table(s) readable; 2 of 2 other table(s) denied
```

| Result | Why |
|--------|-----|
| Fail | A role reads or changes a table outside its grant, writes a `may_read` table, or is a member of `pg_read_all_data` / `pg_write_all_data`. Doctor names the table or role. |
| Warn | A role cannot connect, connects as another role, cannot read a table of its grant, can create objects in a schema, is a member of another role, or a table cannot be checked. |
| Pass | Every role reads its grant and nothing else. |

Without `--strict` or `--online`, doctor does not connect, and does not run
this check.

## Connection poolers

Each role is a separate login. A pooler keeps one server pool for each pair
of database and user. So each boundary gets its own server pool.

* **PgBouncer.** Add each role to `auth_file`, or use `auth_query`. Size
  `default_pool_size` (or a per-user `pool_size` in `[users]`) for each role.
  `max_db_connections` limits all roles of one database together. Keep
  `max_client_conn` above the sum of all app pools.
* **Transaction mode.** A `SET search_path` at connect does not hold. The
  grants use `ALTER ROLE ... SET search_path`. Postgres applies it when the
  server connection starts, so it holds in every pooler mode.
* **Open connections.** `ALTER ROLE ... SET` changes new sessions only. Apply
  the grants before the app starts, or recycle the pools (PgBouncer
  `RECONNECT`).
* **RDS Proxy, Supavisor, Neon.** Each role needs its own credentials in the
  pooler. RDS Proxy reads each one from Secrets Manager.
* **Migrations and grants.** Run `autumn migrate` and `autumn db grants
  --apply` with a direct URL, as the owner of the tables. See
  [Running behind PgBouncer / RDS Proxy](connection-poolers.md).

## Limits

* Postgres only. SQLite has no schemas or roles.
* `ALTER ROLE ... SET search_path` needs a superuser or `CREATEROLE`. From
  Postgres 16, the role also needs `ADMIN OPTION` on each boundary role. The
  role that made the boundary roles has it.
* `REVOKE` removes only grants of the owner. A grant from another role, or
  to `PUBLIC`, stays. The doctor check finds it.
* Before Postgres 15, `PUBLIC` can create objects in schema `public`. Doctor
  warns. Remove it once: `REVOKE CREATE ON SCHEMA public FROM PUBLIC;`.
* A repository uses the primary pool. Use `BoundaryDb` or `pool_for` for the
  queries of a boundary.

## Related

* [Table Ownership Boundaries](table-ownership.md) — the static check.
* [Running behind PgBouncer / RDS Proxy](connection-poolers.md).
* [Migrations](migrations.md).
