# Table width: keep tables at 32 columns or fewer

Keep every database table at **32 columns or fewer**. Diesel compiles fastest
at that width, and Autumn builds on Diesel. Autumn ships no table wider than
32 columns, and CI fails the framework if one appears. Your app can go wider.
`autumn build` warns when it does, so you know what it costs.

## Why 32

Diesel implements its column-tuple traits (`Queryable`, `Selectable`,
`Insertable`, the `table!` column set, and the rest) once for every tuple size
up to a cap. A Cargo feature sets the cap:

| diesel feature                | widest table |
|-------------------------------|--------------|
| `32-column-tables` (default)  | 32           |
| `64-column-tables`            | 64           |
| `128-column-tables`           | 128          |

A table one column past the cap does not compile, so the first 33-column table
forces you to `64-column-tables`. Cargo unifies features across the whole
dependency graph. One crate that turns the feature on makes **every** crate
that depends on `diesel` compile and type-check the larger set of impls. The
cost grows much faster than the cap. These are the measurements for one
production Autumn app's database crate:

| tuple cap | peak rustc memory | build time |
|-----------|-------------------|------------|
| 32        | 1.16 GB           | 1m 18s     |
| 64        | 2.92 GB           | 2m 04s     |
| 65        | 3.06 GB           | 2m 26s     |
| 72        | 3.55 GB           | 3m 26s     |
| 128       | 9.95 GB           | 8m 16s     |

At 128, one crate needs almost 10 GB of memory to compile. That is more than
many CI runners and laptops have, and it is paid again on every clean build
and on every crate that compiles in parallel.

## Check your app

```bash
autumn schema width
```

`autumn schema width` reads three sources under the project root:

- **Migrations.** Each directory of `<version>_<name>/up.sql` steps is replayed
  in version order. It follows `CREATE TABLE`, `ALTER TABLE … ADD COLUMN`,
  `DROP COLUMN`, `RENAME COLUMN`, `RENAME TO` and `DROP TABLE`, and reports
  the final width of each table, with the migration that last widened it.
- **`diesel::table!` blocks** in any `.rs` file, such as `src/schema.rs`.
- **`#[model]` structs**, read the same way `autumn schema snapshot` reads
  them.

It also asks Cargo whether `diesel` resolves with `64-column-tables` or
`128-column-tables` anywhere in the workspace.

It exits non-zero when a table is over 32 columns or a wide feature is on, so
you can use it as a CI gate. `--max <N>` raises the limit if your team has
chosen a wider cap on purpose. `--all` prints every table and its width.

## The `autumn build` warning

`autumn build` runs the same check before it compiles. When it finds a wide
table, it prints a warning that names the table, its width and the file to
change, and then **builds anyway**. A wide table is your decision to make.
Autumn only makes sure you see what it costs.

```text
warning: Diesel table width over 32 columns
  A table wider than 32 columns needs diesel's `64-column-tables` or `128-column-tables` feature,
  and that feature makes every crate that uses diesel slower and hungrier to compile
  (measured: 32 → 1.2 GB / 1m18s, 64 → 2.9 GB / 2m04s, 128 → 10 GB / 8m16s).
  accounts has 41 columns (41 > 32) — migrations: migrations/2026_add_billing/up.sql
  Split rarely-read columns into a 1:1 side table, or group them in a JSONB column.
  The build continues. `autumn schema width` re-runs this check.
  See https://github.com/autumn-foundation/autumn/blob/trunk-dev/docs/guide/table-width.md
```

## Keeping a table narrow

A table that grows past 32 columns usually holds more than one thing. Split it:

- **A 1:1 side table** for columns that are read together and rarely, such as
  billing details, notification settings or import metadata. Give it the same
  primary key and a foreign key back, and load it only on the pages that need
  it. Most queries then select fewer columns too.
- **A JSONB column** for a group of optional settings that you do not filter
  or join on. One column replaces many, and the shape can change without a
  migration.
- **Child rows** for repeated fields (`phone_1`, `phone_2`, `phone_3`). Use a
  `has_many` association instead.

A query never needs more than 32 columns either. Select the columns a page
needs with a `#[derive(Selectable)]` struct rather than the whole row.

## If you need a wider table

You can still have one. Turn the feature on in your app, in one place:

```toml
[dependencies]
diesel = { version = "2", features = ["64-column-tables"] }
```

Pick the smallest cap that fits, because 64 costs far less than 128. Then run
`autumn schema width --max 64` in CI so the table cannot keep growing
unnoticed. `autumn build` keeps printing the warning so the cost stays
visible.

## The framework rule

Autumn holds itself to the default. Every migration the framework and its
plugins ship (`autumn/migrations*`, `autumn-billing`, `autumn-media-plugin`,
the CLI starters, and the examples) stays at 32 columns or fewer. No Autumn
crate turns on `64-column-tables` or `128-column-tables`. The `lint` job runs
`autumn schema width .` over the whole repository and fails the build when
either rule breaks. A framework table that needs a 33rd column must be split
first. Raising the cap pushes the compile cost onto every app built on Autumn.
