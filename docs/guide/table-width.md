# Table width: keep Diesel tables at 32 columns or fewer

Keep every Diesel `table!` declaration and every `#[model]` at **32 columns or
fewer**. Diesel compiles fastest at that width, and Autumn builds on Diesel.
The limit is on the **declaration**, not on the database table: a 40-column
table is fine when Rust declares it in two halves. Autumn ships no declaration
wider than 32 columns, and CI fails the framework if one appears. Your app can
go wider. `autumn build` warns when it does, so you know what it costs.

## Why 32

Diesel implements its column-tuple traits (`Queryable`, `Selectable`,
`Insertable`, the `table!` column set, and the rest) once for every tuple size
up to a cap. A Cargo feature sets the cap:

| diesel feature                | widest `table!` |
|-------------------------------|-----------------|
| `32-column-tables` (default)  | 32              |
| `64-column-tables`            | 64              |
| `128-column-tables`           | 128             |

A `table!` one column past the cap does not compile, so the first 33-column
declaration forces you to `64-column-tables`. Cargo unifies features across
the whole dependency graph. One crate that turns the feature on makes **every**
crate that depends on `diesel` compile and type-check the larger set of impls.
The cost grows much faster than the cap. These are the measurements for one
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

## Split the declaration, not the table

The cap counts the columns of one `table!`, not the columns of the database
table. Two declarations can map the same table. The second one names it with
`#[sql_name]`:

```rust
diesel::table! {
    wide (id) {
        id -> Int4,
        state -> Text,
        a -> Int4,
        // … up to 32 columns
    }
}

diesel::table! {
    #[sql_name = "wide"]
    wide_ext (id) {
        id -> Int4,
        paused -> Nullable<Text>,
        b -> Int4,
        // … the rest, up to 32 again
    }
}
```

Both compile at the default `32-column-tables`. `wide` and `wide_ext` query
the same rows, so the database needs no migration and no new table. Put the
columns a page reads together in the same half. Repeat the primary key in each
half so each one can load and update a row by id.

A `#[model]` sits on one half with `table`:

```rust
#[model(table = "wide_ext")]
pub struct WideSettings {
    #[id]
    pub id: i32,
    pub paused: Option<String>,
    pub b: i32,
}
```

Two rules keep the halves usable:

- **Inserts go through one half.** An `INSERT` names only that half's columns,
  so every `NOT NULL` column without a default must be in it. Give the columns
  in the other half a default or make them nullable, then fill them with an
  `UPDATE … WHERE id = …` through the other half.
- **Keep the split where a generator will not undo it.** `diesel print-schema`
  writes one `table!` per database table, so a regenerated `src/schema.rs`
  goes back to one wide block. Keep the split declarations in a file you
  maintain by hand, or filter that table out of the generator.

## Other ways to stay narrow

A table that grows well past 32 columns often holds more than one thing. When
you are changing the schema anyway, these are also worth considering:

- **A 1:1 side table** for columns that are read together and rarely, such as
  billing details, notification settings or import metadata. Give it the same
  primary key and a foreign key back.
- **A JSONB column** for a group of optional settings that you do not filter
  or join on. One column replaces many, and the shape can change without a
  migration.
- **Child rows** for repeated fields (`phone_1`, `phone_2`, `phone_3`). Use a
  `has_many` association instead.

A query never needs more than 32 columns either. Select the columns a page
needs with a `#[derive(Selectable)]` struct rather than the whole row.

## Check your app

```bash
autumn schema width
```

`autumn schema width` reads three sources under the project root:

- **`diesel::table!` blocks** in any `.rs` file, such as `src/schema.rs`. Each
  declaration counts on its own. `#[sql_name]` tells it which database table
  the declaration maps.
- **`#[model]` structs**, read the same way `autumn schema snapshot` reads
  them.
- **Migrations.** Each directory of `<version>_<name>/up.sql` steps is replayed
  in version order. It follows `CREATE TABLE`, `ALTER TABLE … ADD COLUMN`,
  `DROP COLUMN`, `RENAME COLUMN`, `RENAME TO` and `DROP TABLE`. A table over
  32 columns counts only when **no** `table!` or `#[model]` in the same Cargo
  package maps to it. In that
  case, a generated `src/schema.rs` would declare all of its columns in one
  `table!`. Once your Rust code declares the table, the declarations are what
  count.

It also asks Cargo whether `diesel` resolves with `64-column-tables` or
`128-column-tables` anywhere in the workspace.

It exits non-zero when a declaration is over 32 columns, when a wide migration
table has no declaration, or when a wide feature is on. You can use it as a CI
gate. `--max <N>` raises the limit if your team has chosen a wider cap on
purpose. `--all` prints every table and its width, and marks a wide migration
table that Rust declares as `(declared in Rust)`.

## The `autumn build` warning

`autumn build` runs the same check before it compiles. When it finds a wide
declaration, it prints a warning that names the table, its width and the file
to change, and then **builds anyway**. A wide table is your decision to make.
Autumn only makes sure you see what it costs.

```text
warning: Diesel table width over 32 columns
  A Diesel `table!` wider than 32 columns needs diesel's `64-column-tables` or `128-column-tables` feature,
  and that feature makes every crate that uses diesel slower and hungrier to compile
  (measured: 32 → 1.2 GB / 1m18s, 64 → 2.9 GB / 2m04s, 128 → 10 GB / 8m16s).
  accounts has 41 columns (41 > 32) — migrations: migrations/2026_add_billing/up.sql
    no `table!` maps to it, so a generated schema.rs declares all 41 columns in one `table!`
  Split the Diesel declaration: a second `table!` with `#[sql_name = "<table>"]`
  maps the same table, and each declaration stays at 32 or fewer.
  The build continues. `autumn schema width` re-runs this check.
  See https://github.com/autumn-foundation/autumn/blob/trunk-dev/docs/guide/table-width.md
```

## If you need a wider declaration

You can still have one. Turn the feature on in your app, in one place:

```toml
[dependencies]
diesel = { version = "2", features = ["64-column-tables"] }
```

Splitting the declaration costs nothing at compile time, so try that first.
If you do raise the cap, pick the smallest one that fits, because 64 costs far
less than 128. Then run `autumn schema width --max 64` in CI so the
declaration cannot keep growing unnoticed. `autumn build` keeps printing the
warning so the cost stays visible.

## The framework rule

Autumn holds itself to the default. Every Diesel declaration the framework and
its plugins ship stays at 32 columns or fewer. So does every migration table
that no declaration maps. That covers `autumn/migrations*`, `autumn-billing`,
`autumn-media-plugin`, the CLI starters and the examples. No Autumn crate
turns on `64-column-tables` or `128-column-tables`. The `lint` job runs
`autumn schema width .` over the whole repository and fails the build when
either rule breaks. A framework table that needs a 33rd column gets a second
declaration first. Raising the cap pushes the compile cost onto every app
built on Autumn.
