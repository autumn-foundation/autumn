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
a URL change. Each read that it still needs from another boundary is already
an API call, because the database refused the SQL.

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

An `owns` pattern matches a table in the schema of its boundary. A pattern
with a `.` (`marketing.campaigns`) matches one schema and table. A table in a
schema that only one boundary has, and that no pattern matches, belongs to
that boundary. An exception pattern (`may_read`, `may_write`) with no `.`
matches the table name in every schema.

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
    .plugin_migrations_in_schema("autumn-admin-plugin", "admin", autumn_admin_plugin::MIGRATIONS)
    .run()
    .await;
```

`plugin_migrations_in_schema` puts the tables of a plugin into the schema of
the boundary that owns them. The plugin SQL does not change.

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
put the schema first on the `search_path` of the role, so these features
find the table when they run as that role.

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
It matches `[a-z0-9_-]`.

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
counts them.

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
  insert can use it.
* The `owns` globs resolve against the live tables, with the same rules as
  the graph check. Plugin tables come from `cargo metadata`.

Run it after each migration, with the URL that runs the migrations:

```console
autumn migrate
autumn db grants --apply
```

`--apply` stops when a role does not exist, and prints the `CREATE ROLE`
statement. The command does not make roles, because a role has a password.
`--json` prints the plan per role. `--config` names another
`boundaries.toml`. `--profile` selects the database URL.

A new table that a migration makes has no grant until the next
`autumn db grants --apply`. So the safe state is the default: the boundary
cannot read it.

## 6. Check it with doctor

`autumn doctor --strict` (or `--online`) connects as each role. It tries
`SELECT 1 FROM <table> LIMIT 0` on each table:

* Each table of the grant must open.
* Each other table must give `permission denied`. This is the negative
  check. It finds a grant that `REVOKE` cannot remove, for example a grant to
  `PUBLIC` or through role membership.

```text
✓ database_roles
  marketing: 2 of 2 granted table(s) readable; 3 of 3 other table(s) denied
  core: 2 of 2 granted table(s) readable; 2 of 2 other table(s) denied
```

| Result | Why |
|--------|-----|
| Fail | A role reads a table outside its grant. Doctor names the table. |
| Warn | A role cannot connect, connects as another role, cannot read a table of its grant, or a table cannot be checked. |
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
* **RDS Proxy, Supavisor, Neon.** Each role needs its own credentials in the
  pooler. RDS Proxy reads each one from Secrets Manager.
* **Migrations and grants.** Run `autumn migrate` and `autumn db grants
  --apply` with a direct URL, as the owner of the tables. See
  [Running behind PgBouncer / RDS Proxy](connection-poolers.md).

## Limits

* Postgres only. SQLite has no schemas or roles.
* `ALTER ROLE ... SET search_path` needs a superuser, or `CREATEROLE` with
  admin rights on the role (Postgres 16 and later). The role that made the
  boundary roles has them.
* `REVOKE` removes only grants of the owner. A grant from another role, or
  to `PUBLIC`, stays. The doctor check finds it.
* A repository uses the primary pool. Use `BoundaryDb` or `pool_for` for the
  queries of a boundary.

## Related

* [Table Ownership Boundaries](table-ownership.md) — the static check.
* [Running behind PgBouncer / RDS Proxy](connection-poolers.md).
* [Migrations](migrations.md).
