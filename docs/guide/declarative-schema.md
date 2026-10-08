# Declarative schema

Autumn's classic workflow generates a Diesel migration every time you add or
change a `#[model]` — see [Migrations](./migrations.md). The **declarative
schema** command group (`autumn schema`, tracking issue #1975) offers a
complementary, snapshot-based workflow: you edit your `#[model]` structs freely,
keep a checked-in **snapshot** of the intended schema, and let `autumn schema
diff` compute the migration between the two. It works on both the **Postgres**
and **SQLite** backends.

> **Experimental.** The `autumn schema` group is under active development. The
> verbs and behaviour documented here are those shipped across the slices in the
> current `## [Unreleased]` changelog. Commands print to stdout on success and
> write an `error: …` line to stderr and exit non-zero on failure.

---

## The mental model

There are three sources of truth the commands reconcile:

1. **Your models** — the `#[model]` structs under `src/models/` (or the
   single-file `src/models.rs`). This is the *desired* state.
2. **The snapshot** — a canonical, versioned, dialect-tagged JSON file at
   `.autumn/schema-snapshot.json` (by default). This is the *baseline* the diff
   engine compares against.
3. **The database** — the live schema, evolved by applying migration files.

For a managed model (see [below](#adopt-a-model-modelmanaged)), you write
only (1). The CLI derives the migration, the snapshot, and the
`diesel::table!` block in `src/schema.rs` from it.

`autumn schema diff` compares (1) against (2) and emits the migration that
converges the database on (1). `autumn schema migrate` applies pending migration
files to (3). `autumn schema pull` reads (3) back into (2). `autumn schema
doctor` reports on how all three line up.

The offline, model-facing commands — `snapshot`, `diff`, and `pull` — accept an
explicit `--backend pg|sqlite` to override the dialect; without it they fall back
to the project's configured backend (from `autumn.toml` / the resolved database
URL). The database-facing commands — `migrate` and `doctor` — have **no
`--backend` flag**: they take `--profile` (and `doctor` also `--json`) and derive
the backend from that profile's database URL. The snapshot is
**provider-locked**: a command targeting one backend refuses to act on a snapshot
tagged for another, so a SQLite snapshot can never be diffed or overwritten as
Postgres by accident.

---

## Adopt a model: `#[model(managed)]`

You select the declarative workflow for each model.

- The CLI controls the table of a **managed** model, `#[model(managed)]`.
  `schema diff` writes its migrations and its `src/schema.rs` block.
- An **unmanaged** model, plain `#[model]`, is the default. You write its
  migrations by hand (`autumn generate migration`). `schema diff` does not
  change its table or its `src/schema.rs` block.

Managed and unmanaged models can be in one app. `schema doctor` reports drift
for both kinds (see [below](#autumn-schema-doctor)).

To adopt a model:

1. Add `managed` to its `#[model]` attribute.
2. If the project has no snapshot, run `autumn schema snapshot`.
3. Run `autumn schema diff --write-migration`. The command records the model
   as managed in the snapshot and writes its `src/schema.rs` block.

These field attributes change the table of a managed model:

| Attribute | Table effect |
| --- | --- |
| `#[id]` | The primary key column. |
| `#[indexed]` | An index `idx_<table>_<column>`. |
| `#[unique]` | A `UNIQUE` column and an index `idx_<table>_<column>_unique`. |
| `#[references]` | A foreign key. The field name gives the target table (`author_id` → `authors`). Use `#[references(table = "users")]` for another table. The column also gets an index. |
| `#[renamed_from("old")]` | A rename. See [Renames](#renames-renamed_from). |
| `#[diesel(column_name = "...")]` | The SQL name of the column. |

`Option<T>` makes a column nullable. The `#[model]` macro accepts `managed`,
`#[unique]`, `#[references]` and `#[renamed_from]`. It rejects an incorrect
attribute at compile time.

A table stays managed in the snapshot after you remove `managed` from its
model. If you then delete the model, `schema diff --allow-destructive` drops
the table.

---

## `autumn schema snapshot`

Write the initial baseline from your declared models — the checked-in file the
diff engine compares the desired state against.

```sh
# Snapshot src/models (or src/models.rs) to .autumn/schema-snapshot.json,
# tagged with the project's configured backend.
autumn schema snapshot

# Explicit source, output, and dialect.
autumn schema snapshot --from src/models --out .autumn/schema-snapshot.json --backend pg

# Print the canonical JSON instead of writing a file (useful for diffing/tests).
autumn schema snapshot --stdout
```

| Flag | Meaning |
| --- | --- |
| `--from <PATH>` | A `.rs` model file or a directory of them. Defaults to `src/models`, else `src/models.rs`. |
| `--out <PATH>` | Where to write the snapshot. Defaults to `.autumn/schema-snapshot.json`. Mutually exclusive with `--stdout`. |
| `--backend <pg\|sqlite>` | The dialect to tag the snapshot with. Defaults to the project's configured backend. |
| `--stdout` | Print the canonical JSON to stdout instead of writing a file. |

Commit the snapshot alongside your models — it is the diff baseline every later
`schema diff` reads.

> When any input falls back to a project-relative default (the source, the
> default output path, or the auto-detected backend), the command must be run
> from the project root. A fully explicit invocation (`--from` + `--out`/
> `--stdout` + `--backend`) can run from anywhere.

---

## `autumn schema diff`

Diff the declared models against the snapshot baseline. With no flags it prints
the pending change list plus the `up.sql` / `down.sql` it would generate; with
`--write-migration` it writes the migration to disk **and advances the
snapshot**.

```sh
# Preview the pending migration (prints, writes nothing).
autumn schema diff

# Write migrations/<timestamp>_add_body/{up,down}.sql and advance the snapshot.
autumn schema diff --write-migration --name add_body
```

| Flag | Meaning |
| --- | --- |
| `--from <PATH>` | Models source. Defaults to `src/models`, else `src/models.rs`. |
| `--snapshot <PATH>` | Baseline snapshot. Defaults to `.autumn/schema-snapshot.json`. |
| `--backend <pg\|sqlite>` | The dialect. Defaults to the project's configured backend. |
| `--write-migration` | Write `migrations/<timestamp>_<name>/{up,down}.sql` instead of printing. |
| `--name <NAME>` | Migration directory suffix when writing. Defaults to `schema_update`. |
| `--allow-destructive` | Permit destructive drops / an independent drop+add (a tier-2 guard; otherwise refused). |
| `--dev-url <URL>` | Use an empty dev database as the baseline, not the snapshot. See [below](#shadow-database-baseline---dev-url). |

**The snapshot advances at generation time (#2042).** When you pass
`--write-migration`, the snapshot is moved forward to the state the generated
migration converges the database on — *not* wholesale to your models, but to the
plan the guards actually allowed. Two consequences:

- Re-running `schema diff` after generating is a **no-op** — the snapshot already
  matches, so no duplicate migration is written.
- A later, still-ungenerated model edit diffs **on top of** the already-generated
  change, so the next migration contains only the new delta. Un-generated model
  drift stays visible as drift.

The migration files and the snapshot advance together: if the snapshot write
fails after the migration is on disk, the migration directory is rolled back so a
retry regenerates a single migration rather than a duplicate.

### The diesel schema: `src/schema.rs`

`--write-migration` also writes `src/schema.rs`. For each managed model, the
command:

- writes the `diesel::table!` block when it is missing or stale;
- keeps a block that has the same columns, types and key. The comparison
  ignores type aliases (`BigInt` for `Int8`), type paths and column order;
- removes the block of a dropped table;
- gives the block of a renamed table the new name, and keeps its attributes;
- removes or renames the table in `joinable!` and
  `allow_tables_to_appear_in_same_query!`. A changed call loses its comments;
- renames the column of a `joinable!` when you rename a foreign key column,
  and removes a `joinable!` whose column the managed table no longer has.

The command does not add a `joinable!` for a new foreign key. Write it by
hand.

The command does not change the block of an unmanaged model.

The command also updates `src/schema.rs` when the plan has no changes. Thus,
after you add `managed` to a model, `schema diff --write-migration` writes its
block with no migration.

The command does not write a block in these conditions. It shows a warning:

- The model has a field that the parser cannot read (for example an enum).
- A name is not a Rust identifier, or a column type has no diesel type.
- The block has attributes (for example `#[sql_name]`) and is different. Edit
  that block by hand.
- The command cannot read the block (for example a header with no key,
  `posts {`). Edit that block by hand.

Without `src/schema.rs`, the command does not make one. If `src/schema.rs`
exists but the command cannot read it, the command stops before it writes
anything. If the command cannot write `src/schema.rs`, it removes the new migration and restores the
snapshot. The command writes the file through a temporary file, so a failed
write does not change it.

### Renames: `#[renamed_from]`

Without a hint, the diff sees a renamed field as a drop and an add. It refuses
this change, because it can be a rename.

- To rename a column, put `#[renamed_from("old_name")]` on the field.
- To rename a table, put it on the model, after `#[model]`. Before `#[model]`,
  the compiler cannot find the attribute.

```rust
#[autumn_web::model(managed)]
#[renamed_from("articles")]
pub struct Post {
    #[id]
    pub id: i64,
    #[renamed_from("title")]
    pub headline: String,
}
```

```sql
-- up.sql
ALTER TABLE articles RENAME TO posts;

ALTER TABLE posts RENAME COLUMN title TO headline;
```

- A rename keeps the data, so it does not need `--allow-destructive`.
- `down.sql` renames back.
- A hint has an effect only on a `#[model(managed)]` model.
- The diff also renames each index that has a convention name
  (`idx_<table>_<field>` or `idx_<table>_<field>_unique`) and the same shape.
  Postgres uses `ALTER INDEX ... RENAME`. SQLite drops the index and creates
  it again. An index with `COLLATE` or `DESC` keeps its SQL.
- The diff uses a hint only when the old name is in the baseline and the new
  name is not. After the migration, the hint has no effect. You can keep it or
  remove it.
- The macro accepts one `snake_case` name only, and removes the hint from the
  generated code.

The diff refuses a hint in these conditions. `--allow-destructive` does not
override this.

- The old name is still declared.
- Two hints use the same old name.
- The parser cannot read the field (an unsupported type).
- The hints make a chain or a swap. Do each rename in its own migration.
- An index definition or a `CHECK` names the old column in a position that is
  not clearly a column (for example an operator class or an `EXTRACT` field
  with the same name). Write that rename as a manual migration.
- A new name is a table or index name that is already in use.

On Postgres, a new name longer than 63 bytes is refused too. The offline
snapshot does not know views or sequences; use `--dev-url` to check a new name
against them.

### Shadow-database baseline: `--dev-url`

`--dev-url <URL>` uses a database, not the snapshot, as the baseline. You can
also set the URL in `AUTUMN_DEV_URL`, which keeps the password out of the
process list.

```sh
AUTUMN_DEV_URL=postgres://localhost/postgres autumn schema diff
```

1. On Postgres, the command creates a scratch database on the server of the
   URL. The role in the URL needs the `CREATEDB` privilege. On SQLite, the
   command uses an in-memory database; the URL only selects SQLite.
2. It applies every migration in `migrations/` to the scratch database, as
   `autumn schema migrate` does. On Postgres, the migrations write to the
   `public` schema.
3. It reads the schema back and drops the scratch database. The database in
   the URL does not change.
4. It diffs the models against that schema.

Use it when the snapshot cannot see the full schema, for example after a
hand-written migration or with a `#[belongs_to]` foreign key.

- The snapshot is optional. When it is present and does not match the
  migrations, the command writes a warning to stderr.
- A replayed table is managed only when the snapshot records it as managed,
  or when a managed model declares it. The diff never drops a table that only
  a hand-written migration made. With no snapshot, the diff never drops a
  table.
- `--write-migration` writes the migration and the snapshot, as usual.
- The command refuses a new or renamed table or index whose name is already a
  table, an index, a view, a sequence, a data type or another relation in the
  migrated schema.
- On SQLite, a replayed table or column name that differs from the model name
  only in case (`Users` and `users`) is read as the model name, as SQLite
  does.
- The URL backend must match the schema backend. A `sqlite:` URL needs a CLI
  built with `--features sqlite`.
- The replay applies your migrations only, not the framework migrations.
- A migration with `run_in_transaction = false` (for example
  `CREATE INDEX CONCURRENTLY`) replays as it applies.
- Like `schema pull`, the command connects to Postgres without TLS.
- Errors never show the password. A connection error shows only the host and
  port.

### SQLite ALTER support via table-recreate (#2035)

On SQLite, `schema diff` emits real migrations for the ALTER-family changes
SQLite's `ALTER TABLE` cannot express directly (`ALTER COLUMN TYPE`,
`DROP NOT NULL`, `SET DEFAULT`, `ADD CHECK`) using the
standard **table-recreate** procedure — create a new table, `INSERT..SELECT` the
common columns, drop the old table, rename, and recreate indexes, all wrapped in
`PRAGMA foreign_keys=OFF` … `foreign_key_check` … `ON` and coalesced to one
recreate block per table. Postgres output is byte-for-byte unchanged. When a
recreate cannot be expressed safely the command **refuses loudly** rather than
emitting unsafe SQL.

Making an existing **nullable column required** (`SET NOT NULL`) is the one
exception that is *not* rebuilt: on **both** backends the plan guard refuses it
*before* any SQL is emitted (SQLite never recreates the table for it). The
offline engine has no backfill value to synthesize for the rows that are already
NULL, so `schema diff` stops with a message telling you to backfill the column
and apply the change manually — or keep it nullable (`Option<...>`). The inverse
change, `DROP NOT NULL` (required → nullable), is always safe and *is* handled by
the table-recreate path above.

**Adding a foreign key to a pre-existing column** (attaching `#[references]` to a
column that already exists) is likewise *not* rebuilt — the plan guard refuses it
on **both** backends before any SQL is emitted. The offline snapshot cannot
confirm the database doesn't already carry a generated `<table>_<column>_fkey`
association constraint (a `#[belongs_to(...)]` `<name>_id` column parses as a
plain integer with no visible foreign key), so re-adding that constraint could
collide. `schema diff` stops and directs you to add the foreign key via a manual
migration (or re-snapshot from an authoritative source). A brand-new foreign-key
column is unaffected: it arrives as an ordinary `ADD COLUMN` with an inline
`REFERENCES` and needs no table-recreate at all.

**Hand-written triggers and dependent views are *not* preserved and do *not*
refuse the rebuild.** The table-recreate copies columns and re-creates indexes
only — it does not see triggers or views, which live outside the offline model.
The generated `DROP TABLE` drops any triggers on the table (they are *not*
re-created by the migration), and views that reference the table are left
dangling and may block the rename. This is a case the command does **not** fail
closed on: instead of refusing, the emitted migration carries an
`-- autumn-safety:` advisory comment naming the gap, so if your SQLite table has
hand-written triggers or dependent views you must re-create the triggers and
repair the views in a manual migration after applying the rebuild.

---

## `autumn schema migrate`

Apply pending migration files against the configured database.

```sh
autumn schema migrate
autumn schema migrate --profile prod
```

| Flag | Meaning |
| --- | --- |
| `--profile <PROFILE>` | Config profile whose database URL to apply against. Defaults to the ambient profile resolution. |

- On **Postgres** it is advisory-locked, so concurrent migrators serialize; on
  **SQLite** it applies unlocked under the single-writer backend.
- **Applying against a `sqlite://` URL requires a CLI built with the non-default
  `sqlite` cargo feature** (`cargo build -p autumn-cli --no-default-features
  --features sqlite`). The default/published `autumn` binary is **Postgres-only**;
  point it at a SQLite backend and `autumn schema migrate` stops with a "rebuild
  with `--features sqlite`" error and never touches the database. Only *applying*
  migrations is gated this way — `autumn schema diff` still generates SQLite
  migration SQL offline in the default build.
- It is **provider-locked** against the snapshot's dialect before applying —
  *when a snapshot is present*. If `.autumn/schema-snapshot.json` is missing (a
  pre-snapshot or adopting project), `schema migrate` prints a note and applies
  the pending migrations **without** a provider-lock check rather than failing;
  run `autumn schema snapshot` to establish the baseline and arm the guard.
- It **does not touch the snapshot** — the baseline already advanced at
  generation time (`schema diff --write-migration`), so this command only applies
  the pending files. The destructive-change guards already ran at diff time, so
  migration files apply verbatim.

> This declarative `autumn schema migrate` verb is distinct from the classic
> `autumn migrate` up/down CLI documented in [Migrations](./migrations.md). The
> classic verb is currently Postgres-only; `autumn schema migrate` can apply on
> both backends, but — as noted above — the SQLite path needs a CLI built
> `--features sqlite` (the default binary is Postgres-only). For SQLite, `schema pull`
> and the database-schema-drift check of `schema doctor` also need a
> `--features sqlite` build. The pending-migrations check is Postgres-only.

---

## `autumn schema pull`

Read the schema of a live **Postgres** or **SQLite** database, and write it as
a snapshot. With `--dry-run`, show the changes and write nothing. This is the
DB-derived counterpart to `schema snapshot`. Use it to adopt a brownfield
schema, or to re-baseline a snapshot that drifted from the database.

```sh
# Introspect the profile-resolved DB into .autumn/schema-snapshot.json.
autumn schema pull

# Show what pulling would change without writing anything.
autumn schema pull --dry-run
```

| Flag | Meaning |
| --- | --- |
| `--profile <PROFILE>` | Config profile whose database URL to introspect. Defaults to the ambient profile resolution. |
| `--out <PATH>` | Where to write the snapshot. Defaults to `.autumn/schema-snapshot.json`. |
| `--backend <pg\|sqlite>` | Override the dialect tag / apply path. Defaults to the backend implied by the resolved database URL. |
| `--dry-run` | Print the would-be diff and write nothing. |

`pull` is read-only with respect to the database (catalog reads only). It
introspects tables, columns (types, nullability, defaults), primary keys, foreign
keys, unique constraints, and indexes into the **same** IR the model parser
produces. Before overwriting, a **provider-lock** guard refuses to clobber a
snapshot tagged for another backend.

> **SQLite needs the `sqlite` build.** A CLI built with `--features sqlite`
> introspects a SQLite database. The default binary refuses a SQLite URL and
> writes no snapshot. A `--dry-run` reports bidirectionally so a manually-dropped
> default / FK / CHECK in the live DB (which the forward pass cannot express) is
> still surfaced as a removal.

---

## `autumn schema doctor`

A read-only health report over the declarative-schema state. It never mutates
anything and exits non-zero **only** on an actionable error (a warning never
fails), so it is safe to run offline and in CI.

```sh
autumn schema doctor
autumn schema doctor --json
autumn schema doctor --profile prod
```

| Flag | Meaning |
| --- | --- |
| `--profile <PROFILE>` | Config profile whose database URL to probe. Defaults to the ambient profile resolution. |
| `--json` | Emit the checks as a machine-readable JSON array instead of the aligned text report. |

Each check reports `OK` / `WARN` / `ERROR` with a one-line remediation. The
checks are:

- **project-root** — the command is running inside an Autumn project.
- **snapshot-present** — `.autumn/schema-snapshot.json` exists and is readable.
- **snapshot-drift** — the declared models match the snapshot baseline.
- **schema-rs-drift** — each managed model has a matching block in
  `src/schema.rs`. A missing or stale block is a **WARN**, and so is a
  `joinable!` that names a column the managed table does not have. To fix it, run
  `autumn schema diff --write-migration`. A table that the row cannot compare
  (for example a model with an enum field) is also a **WARN**, and the row
  names it. Without a managed
  model, the row is **OK**.
- **provider-lock** — the snapshot's backend tag matches the detected backend.
- **snapshot-dialect-vs-db** — the snapshot dialect matches the configured
  database URL's backend.
- **pending-migrations** — whether generated migration files are still unapplied
  (Postgres).
- **database-schema-drift** (#2045) — when the database is reachable, the
  snapshot is introspected against the live schema bidirectionally; drift is an
  actionable **WARN**. Offline, it stays a non-failing WARN. SQLite needs the
  `sqlite` build.
- **unmanaged-drift** — each unmanaged model matches its table in the live
  database. The check finds a missing table, a model column that the table
  does not have, a different primary key, and a different type or `NULL`
  rule. It does not compare columns that only the table has, indexes,
  defaults or constraints. It does not compare a type that the CLI keeps as
  an opaque type (for example `VARCHAR(40)`, or `BOOLEAN` on SQLite). A model
  with a field that the parser cannot read (for example an enum) is also a
  **WARN**, and the row names it: the check cannot compare that column.
  Drift is a **WARN**. To fix it, write a migration with
  `autumn generate migration`, or change the model.

---

## A typical loop

```sh
# One-time: capture the current models as the baseline (or `pull` an existing DB).
autumn schema snapshot

# Edit your #[model] structs, then generate the migration, advance the
# snapshot, and update src/schema.rs.
autumn schema diff --write-migration --name add_published_at

# Apply it (and later, on other environments / profiles).
autumn schema migrate --profile prod

# Anytime: check that models, snapshot, and database agree.
autumn schema doctor
```

Commit the snapshot, `src/schema.rs`, and the generated migration directory
together.

---

## See also

- [Migrations](./migrations.md) — the classic embedded-migration workflow, the
  advisory-lock serialisation, and the `autumn migrate` CLI.
- [SQLite in production](./sqlite-in-production.md) — the SQLite backend's
  support contract, including the table-recreate migration mechanics.
