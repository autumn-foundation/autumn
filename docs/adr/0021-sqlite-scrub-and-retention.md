# ADR 0021: `autumn db scrub` and retention sweeps on SQLite

- Status: Accepted
- Date: 2026-10-20
- Deciders: Autumn maintainers
- Tags: sqlite, scrub, retention, privacy

## Context

Issue #2553. Two subsystems work only on Postgres:

- `autumn db scrub` (#1602) opens a `PgConnection`. It reads Postgres catalogs.
  It builds Postgres SQL (`md5`, `ctid`, `::text`, `"public".`).
- The runtime retention sweep (#1605) is compiled only when the `sqlite`
  feature is off. With it on, every sweep dataset reports "Postgres-only".

`autumn db retention` is already backend-neutral. It runs the app binary.

## Planning

### Brainstorming

| Idea | Verdict |
| --- | --- |
| Rewrite scrub on a generic `Backend` trait | Rejected. About 11,000 lines of code. High risk to Postgres. |
| Copy `scrub.rs` into a SQLite twin | Rejected. Two classifiers drift apart. |
| Keep one classifier. Add a `Dialect` to the SQL builders. Add a SQLite probe and apply module. | **Chosen.** |
| Hash in SQL with a SQLite extension | Rejected. No `md5`/`sha256` built in. |
| Register one `autumn_sha256` function on the connection | **Chosen.** |
| Reuse the Postgres sweep with `?` binds | Rejected. Time columns differ (`BIGINT` ms vs `TEXT`). |
| One SQLite sweep per dataset, same report shape | **Chosen.** |

### Reverse brainstorming: how do we make the scrub leak?

| Leak | Guard |
| --- | --- |
| Unclassified column passes | Same `build_plan`. Test: abort. |
| `CHECK` column gets a fake value | Parse `CHECK(...)` text. Refuse any column named in it. |
| Old bytes stay in free pages or WAL | `secure_delete=ON`, `VACUUM`, `wal_checkpoint(TRUNCATE)`. Test reads raw bytes. |
| Typo path creates an empty file and reports "clean" | Check the file exists before opening. |
| `ON DELETE CASCADE` removes user rows during a purge | `foreign_keys=ON`. `foreign_key_check` before commit. |
| Trigger copies old rows | Report triggers. Refuse a purge target with a delete trigger. |
| Mixed Postgres and SQLite targets half-scrubbed | Refuse mixed topologies before any write. |
| `--sample` has no SQLite form | Refuse. Never ignore. |
| Dry run prints a path that looks like a secret | Print the file path only. SQLite has no password. |
| Sweep deletes rows that are still live | Repeat the predicate on the outer `DELETE`. |
| Sweep uses the app clock | Resolve the cutoff on the database. |

### Six hats

- **White (facts).** SQLite lacks `md5`, `translate`, `ctid`, `::`. `rowid`,
  `replace`, `quote`, `CAST` exist. The CLI links diesel SQLite always.
- **Red (feelings).** Operators must trust that a "clean" copy is clean. Refuse
  loudly. Do not pass quietly.
- **Black (risks).** See the table above. The highest risk is a changed
  Postgres plan. Pin it with the existing tests plus a golden-SQL test.
- **Yellow (benefits).** One binary and one file. A laptop copy of a SQLite
  app in one command.
- **Green (creative).** Use the `.sqlite` backup artifact path
  (`--artifact`). It reuses the restore code.
- **Blue (process).** Red tests first. Then green. Then refactor. Then review
  from four angles.

## Decision

1. Add `Dialect { Postgres, Sqlite }` to `DatabaseFacts`. Default is Postgres.
   The SQL builders read it. Postgres output stays byte-identical.
2. Add `db/scrub/sqlite.rs`: connect, probe, apply, verify.
3. `scrub()` picks the path from the target URL. A mixed set is refused.
4. `--sample` on SQLite is refused.
5. Add `sweep_sqlite` in `data_retention.rs` for `job_history`,
   `commit_hooks`, and `experiment_assignments`.
6. Mark both rows **Available now** in `sqlite-in-production.md`.

## Consequences

- One classifier. Same fail-closed rules on both backends.
- The dry run for SQLite prints SQL that calls `autumn_sha256`. Autumn
  registers it. Plain `sqlite3` cannot run the script.
- `--sample` stays Postgres-only.
