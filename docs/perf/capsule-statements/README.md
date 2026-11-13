# Data-capsule export/import: statements per unit of work

Ledger harness (`autumn/tests/integration/ledger_capsule_profile.rs`, driven by `run.sh`).

**Fixture** (seeded, deterministic shape): `users` (200 rows) + 30 tables
`t0..t29`, 20,000 rows each (600k rows), 7 columns each, FK `owner_id -> users`,
~30% of every table owned by subject `1` (skew), 25% NULL `body`, 33% NULL
`note`. Exporting subject `1` yields 180,001 records (30% of rows, via `g%10<3`, no `random()`).

**Workload**: public entry points `export_subject(models, PgCapsuleStore, "1")`
then `import_capsule(capsule, models, PgCapsuleStore)` into an empty copy.
`pg_stat_statements` is reset before each phase and snapshotted after
(`baseline/pgss-{export,import}.csv`; `baseline/digest.txt` is the md5 of every imported table plus sequence state, the equivalence reference). Postgres 16, local, default config.

## Baseline (before any change)

Phase boundaries are acknowledged by `run.sh` (the test blocks at each mark
until the `pg_stat_statements` reset/snapshot is done), and the target tables
are truncated with `RESTART IDENTITY`, so import runs the `setval` path
(31 `setval` calls in `baseline/pgss-import.csv`).

| phase  | statements | total buffers | WAL bytes | temp blks |
| ------ | ---------- | ------------- | --------- | --------- |
| export | 129        | 32,133        | 1,381     | 0         |
| import | 466        | 1,274,000     | 33,791,362| 0         |

Top by calls, import: `pg_get_serial_sequence($1,$2)` **213 calls = 45.7%** of
statements (0.1% of buffers): one call per non-generated field of every
imported table, inside the import transaction (`serial_sequence` in
`autumn/src/gdpr/portability/pg.rs`). Top by buffers, export:
`describe_table` catalog query, 59.9% (31 calls, one per model) — inherent to
the per-table design; recorded as a finding, not changed.

## After (same harness, same fixture, same session)

Change: `serial_sequences` looks up every imported column's sequence with one
`unnest($1::text[], $2::text[]) WITH ORDINALITY` statement instead of one
`pg_get_serial_sequence` per column. No migration, no index, no lock change.

| import                   | before     | after      | tool |
| ------------------------ | ---------- | ---------- | ---- |
| statements               | 466        | **254** (-45.5%) | pg_stat_statements `calls` |
| `pg_get_serial_sequence` | 213 calls  | **1 call** | same |
| `setval`                 | 31 calls   | 31 calls   | same |
| total buffers            | 1,274,000  | 1,274,000  | same (the lookup was 0.1% of buffers) |
| WAL bytes                | 33,791,362 | 33,791,362 | same |
| temp blocks written      | 0          | 0          | same |

Export runs untouched code (129 statements before and after).

**Equivalence**: `baseline/digest.txt` and `after/digest.txt` are
byte-identical (md5 `2e3c73360432de3d65a1782d65d42e92`): row count and md5 of
every imported table in key order, plus `last_value` of every sequence, after
the `setval` path ran on both sides.

**Not covered**: the Docker round-trip suite (`data_capsule_pg`) could not run
in this sandbox (no Docker daemon); CI's Docker lane runs it unchanged. The
impact claim is round trips (N+1 elimination, 213 -> 1), not buffers.

## Reproduce

`LEDGER_PG_URL=postgres://u@host:port ./run.sh <outdir>` (Postgres with
`shared_preload_libraries=pg_stat_statements`, superuser role). It builds with
`--features ledger-profile` and runs the `#[ignore]`d
`capsule_export_import_profile` test in the consolidated `integration_tests`
binary.
