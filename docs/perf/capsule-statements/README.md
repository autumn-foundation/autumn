# Data-capsule export/import: statements per unit of work

Ledger harness (`autumn/tests/ledger_capsule_profile.rs`, driven by `run.sh`).

**Fixture** (seeded, deterministic shape): `users` (200 rows) + 30 tables
`t0..t29`, 20,000 rows each (600k rows), 7 columns each, FK `owner_id -> users`,
~30% of every table owned by subject `1` (skew), 25% NULL `body`, 33% NULL
`note`. Exporting subject `1` yields 180,001 records (30% of rows, via `g%10<3`, no `random()`).

**Workload**: public entry points `export_subject(models, PgCapsuleStore, "1")`
then `import_capsule(capsule, models, PgCapsuleStore)` into an empty copy.
`pg_stat_statements` is reset before each phase and snapshotted after
(`baseline/pgss-{export,import}.csv`; `baseline/digest.txt` is the md5 of every imported table plus sequence state, the equivalence reference). Postgres 16, local, default config.

## Baseline (before any change)

| phase  | statements | total buffers | WAL bytes | temp blks |
| ------ | ---------- | ------------- | --------- | --------- |
| export | 86         | 29,973        | 722       | 0         |
| import | 435        | 1,273,732     | 33,788,293| 0         |

Top by calls, import: `pg_get_serial_sequence($1,$2)` **213 calls = 49.0%** of
statements (0.1% of buffers). One call per non-generated field of every
imported table, issued inside the import transaction (`serial_sequence` in
`autumn/src/gdpr/portability/pg.rs`). Top by buffers, export:
`describe_table` catalog query, 59.4% (17,793 buffers over 28 calls) — one per
table, inherent to the per-table design; recorded as a finding, not changed.

## After (same harness, same fixture, same session)

Change: `serial_sequences` looks up every imported column's sequence with one
`unnest($1::text[], $2::text[]) WITH ORDINALITY` statement instead of one
`pg_get_serial_sequence` per column. No migration, no index, no lock change.

| import                  | before    | after     | tool            |
| ----------------------- | --------- | --------- | --------------- |
| statements              | 435       | **223**   | pg_stat_statements `calls` (-48.7%) |
| `pg_get_serial_sequence`| 213 calls | **1 call**| same            |
| total buffers           | 1,273,732 | 1,273,732 | same (unchanged; the lookup was 0.1% of buffers) |
| WAL bytes               | 33,788,293| 33,788,293| same            |
| temp blocks written     | 0         | 0         | same            |

Export is untouched code (86 vs 89 statements is connection-setup noise in the
snapshot window; `describe_table` is 59% of export buffers, 28-29 calls, left
as a finding: one catalog read per table is the design).

**Equivalence**: `baseline/digest.txt` and `after/digest.txt` are byte-identical
(md5 `e4950b25b32b3a9864ae4fc26837b3a7`): row count and md5 of every imported
table in key order, plus `last_value` of every sequence. The sequences all
moved (`setval`) identically.

**Not covered**: the Docker round-trip suite (`data_capsule_pg`) could not run
in this sandbox (no Docker daemon); CI's Docker lane runs it unchanged.
Impact claim is round trips (N+1 elimination, 213 -> 1), not buffers.
