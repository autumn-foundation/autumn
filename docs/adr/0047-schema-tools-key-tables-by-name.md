# ADR 0047: Schema Tools Key a Table by Name

- Status: Accepted
- Date: 2026-11-12
- Deciders: Autumn maintainers
- Tags: schema, cli, database-boundaries

## Context

Issue [#3674](https://github.com/autumn-foundation/autumn/issues/3674).

`#[model(schema = "...")]` puts a table in a Postgres schema. The schema
tools (`autumn schema diff`, `pull`, `doctor`, `sync`) key a table by its
bare name. A foreign key target is bare (`authors`) or qualified
(`blog.authors`). Three helpers matched a target to a table, each in a
different way, and one site compared bare names. Each gave a false refusal
in some case.

The issue asks two questions:

1. Must the match use the schema of the table that owns the foreign key?
2. Must the tools key a table by `(schema, name)`?

## Options

1. Key by name. One type, `TableRef`, and one match,
   `table_ref::fk_points_at`, for every site.
2. Key by `(schema, name)` in every map, plan change and snapshot.
3. Option 1, and resolve a bare target in the schema of the owner table.

## Decision

Option 1.

- A qualified target names the table of that name in that schema. `public`
  is the default schema: `public.users` is `users`.
- A bare target names the one table of that name. The tools keep one table
  name for all schemas, so it cannot be ambiguous.
- The SQLite rebuild path keeps its own match. SQLite has no schemas.

Option 2 is rejected for now:

- A `diesel::table!` block is a Rust module with the name of the table. Two
  `users` tables give two `users` modules in one `schema.rs`.
- `SchemaChange`, the snapshot, `#[renamed_from]` hints and the
  introspection all name a table by one string. The change touches each of
  them, and the snapshot format.

Option 3 is rejected: the owner schema does not tell which table a bare
target names.

- Introspection writes a target in `public` bare, from any schema.
- The snapshot writes a modeled target in another schema bare.

## Consequences

- A foreign key to a table of the same name in another schema (an external
  `auth.users`) no longer blocks a type change or a rename of the managed
  `users`.
- A model that spells a target qualified, and a snapshot that spells it
  bare, are the same foreign key.
- The rename planner checks table and index names in the schema of the
  renamed table only.
- The limit stays: one table name for all schemas. To lift it, adopt option
  2. Then `TableRef` gets the owner schema for a bare target, and the maps
  key by `TableRef`.
