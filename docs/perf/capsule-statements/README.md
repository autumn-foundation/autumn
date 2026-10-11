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
