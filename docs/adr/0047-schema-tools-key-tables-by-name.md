# ADR 0047: Schema Tools Key a Table by Name

- Status: Accepted
- Date: 2026-11-12
- Deciders: Autumn maintainers
- Tags: schema, cli, database-boundaries

## Context

Issue [#3674](https://github.com/autumn-foundation/autumn/issues/3674).

`#[model(schema = "...")]` puts a table in a Postgres schema. The schema
tools (`autumn schema diff`, `pull`, `doctor`, `sync`) identify a table by
its name only. A foreign key target is bare (`authors`) or qualified
(`blog.authors`). Three helpers matched a target to a table. Each helper
used a different rule. One more site compared only bare names. Thus some
correct changes were refused, and some diffs showed a change that did not
exist.

The issue asks two questions:

1. Must the match use the schema of the table that owns the foreign key?
2. Must the tools identify a table by `(schema, name)`?

## Options

1. Key by name. One type, `TableRef`, and one match,
   `table_ref::fk_points_at`, for every site.
2. Key by `(schema, name)` in every map, plan change and snapshot.
3. Option 1, and resolve a bare target in the schema of the owner table.

## Decision

Option 1.

- A qualified target names the table of that name in that schema. `public`
  is the default schema: `public.users` is `users`.
- A bare target identifies the table with that name. In the tables that the
  tools read, a table name can be in only one schema. Thus a bare target
  identifies one of these tables. A table outside this set (an external
  `auth.users`) is reached only by a qualified target.
- The SQLite rebuild path keeps its own match. SQLite has no schemas.

The maintainers reject option 2 at this time. The reasons are:

- A `diesel::table!` block is a Rust module with the name of the table. Two
  `users` tables give two `users` modules in one `schema.rs`. Rust does not
  permit two modules with the same name in one file.
- `SchemaChange`, the snapshot, `#[renamed_from]` hints and the
  introspection all name a table by one string. Option 2 changes each of
  them. It also changes the snapshot format.

Option 3 is rejected: the owner schema does not tell which table a bare
target names.

- Introspection writes a target in `public` without its schema. It does
  this for a foreign key in any schema.
- The snapshot writes a modeled target in another schema without its
  schema.

## Consequences

- A foreign key to a table with the same name in another schema (an
  external `auth.users`) no longer blocks a type change of the managed
  `users`.
- A model that writes a target with its schema, and a snapshot that writes
  it without, give the same foreign key. The diff shows no change.
- A rename updates a qualified foreign key to the renamed table.
- The rename planner checks table and index names in the schema of the
  renamed table only.
- This limit continues: a table name can be in only one schema. To remove
  the limit, use option 2. Then `TableRef` gets the owner schema for a bare
  target. Each map uses `TableRef` as its key.
