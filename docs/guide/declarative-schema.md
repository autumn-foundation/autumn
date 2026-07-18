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

### SQLite ALTER support via table-recreate (#2035)

On SQLite, `schema diff` emits real migrations for the ALTER-family changes
SQLite's `ALTER TABLE` cannot express directly (`ALTER COLUMN TYPE`,
`DROP NOT NULL`, `SET DEFAULT`, `ADD CHECK`, `ADD CONSTRAINT`) using the
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
> `--features sqlite` (the default binary is Postgres-only). `schema pull` remains
> Postgres-only in this slice, and `schema doctor`'s database-touching checks
> (pending-migrations, database-schema-drift) are likewise Postgres-only.

---

## `autumn schema pull`

Introspect a live **Postgres** database and write (or, with `--dry-run`,
describe) a snapshot of its actual shape — the DB-derived counterpart to
`schema snapshot`. Use it to adopt a brownfield schema, or to re-baseline a
snapshot that drifted from the database.

```sh
# Introspect the profile-resolved Postgres DB into .autumn/schema-snapshot.json.
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

> **Postgres only in this slice.** A resolved SQLite URL is **refused loudly** —
> SQLite database introspection is a future slice of #1975, so no partial
> snapshot is written. A `--dry-run` reports bidirectionally so a manually-dropped
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
- **provider-lock** — the snapshot's backend tag matches the detected backend.
- **snapshot-dialect-vs-db** — the snapshot dialect matches the configured
  database URL's backend.
- **pending-migrations** — whether generated migration files are still unapplied
  (Postgres).
- **database-schema-drift** (#2045) — when a Postgres database is reachable, the
  snapshot is introspected against the live schema bidirectionally; drift is an
  actionable **WARN**. Offline, it stays a non-failing WARN.

---

## A typical loop

```sh
# One-time: capture the current models as the baseline (or `pull` an existing DB).
autumn schema snapshot

# Edit your #[model] structs, then generate the migration + advance the snapshot.
autumn schema diff --write-migration --name add_published_at

# Apply it (and later, on other environments / profiles).
autumn schema migrate --profile prod

# Anytime: check that models, snapshot, and database agree.
autumn schema doctor
```

Commit the snapshot and the generated migration directory together.

---

## See also

- [Migrations](./migrations.md) — the classic embedded-migration workflow, the
  advisory-lock serialisation, and the `autumn migrate` CLI.
- [SQLite in production](./sqlite-in-production.md) — the SQLite backend's
  support contract, including the table-recreate migration mechanics.
