# Commit-hook worker: stale after-hook recovery (baseline)

Harness: `extract_sql.py` writes the Postgres SQL that
`autumn/src/repository_commit_hooks.rs` ships as pgbench scripts, and the
queue schema from the shipped migrations. `fixture.sql`, `report.sql`,
`run.sh [T] [hist]`, `explain.sh`.

Reproduce: Postgres 16 with `pg_stat_statements` preloaded and autovacuum
off, PG* env set. `run.sh 2000 1000000`, then `explain.sh` on the database
it leaves. Baseline artifacts: `baseline-<hist>.txt`,
`explain-before-<hist>.txt`.
