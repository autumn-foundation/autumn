#!/usr/bin/env bash
# Ledger harness: job enqueue through the shipped pg_insert_job SQL on a
# production-shaped autumn_jobs (fixture.sql). N enqueues: 50% a plain job,
# 25% a running-window unique job, 15% a pending-window unique job, 10% a
# 1-hour TTL unique job (eviction UPDATE, then INSERT), keys zipf-drawn,
# seeded. Prints the pg_stat_statements profile, WAL and table counters.
# Usage: [JOB_RS=<file>] run.sh [N] [hist]. Needs PG* env pointing at a
# Postgres 16 server that preloads pg_stat_statements.
set -euo pipefail
# A seq scan of a table past shared_buffers/4 starts where the last one
# stopped; off, an early-exit scan reads the same blocks on every run.
export PGOPTIONS="${PGOPTIONS:-} -c synchronize_seqscans=off"
HERE="$(cd "$(dirname "$0")" && pwd)"
N="${1:-5000}"
HIST="${2:-1000000}"
DB="${DB:-ledger_jobenqueue}"
WORK="$(mktemp -d)"
python3 "$HERE/extract_sql.py" "$WORK" ${JOB_RS:+"$JOB_RS"}
python3 "$HERE/gen_pgbench.py" "$WORK"

dropdb --if-exists "$DB"; createdb "$DB"
export PGDATABASE="$DB"
psql -qX -c 'CREATE EXTENSION IF NOT EXISTS pg_stat_statements' >/dev/null
(cd "$HERE" && psql -qX -v ON_ERROR_STOP=1 -f schema.sql >/dev/null)
if [ -n "${EXTRA_SCHEMA:-}" ]; then psql -qX -v ON_ERROR_STOP=1 -f "$EXTRA_SCHEMA" >/dev/null; fi
psql -qX -v ON_ERROR_STOP=1 -v hist="$HIST" -f "$HERE/fixture.sql" >/dev/null
echo "=== fixture: hist=$HIST, N=$N"
psql -X -c "SELECT name, unique_window, status, count(*) FROM autumn_jobs
            GROUP BY 1,2,3 HAVING count(*) > 100 ORDER BY 1,2,3" \
  -c "SELECT pg_size_pretty(pg_relation_size('autumn_jobs')) AS heap,
             pg_relation_size('autumn_jobs') / 8192 AS heap_blocks"
psql -qX -c 'CHECKPOINT' -c 'SELECT pg_stat_statements_reset()' -c 'SELECT pg_stat_reset()' >/dev/null
LSN0="$(psql -XAt -c 'SELECT pg_current_wal_lsn()')"
echo "=== workload"
pgbench -n -M extended --random-seed=3742 -c 1 -t "$N" \
  -f "$WORK/plain.pgbench@50" -f "$WORK/running.pgbench@25" \
  -f "$WORK/pending.pgbench@15" -f "$WORK/ttl.pgbench@10" \
  | grep -E 'SQL script|weight|processed|failed'
psql -qX -c 'SELECT pg_stat_force_next_flush()' >/dev/null
psql -X -v lsn0="$LSN0" -f "$HERE/report.sql"
echo "=== end state"
psql -XAt -c "SELECT 'rows: ' || count(*) FROM autumn_jobs" \
  -c "SELECT 'unique holds by name/window/status: ' || string_agg(x, ', ' ORDER BY x) FROM
      (SELECT name || '/' || unique_window || '/' || status || '=' || count(*) AS x
       FROM autumn_jobs WHERE id LIKE 'e%' GROUP BY name, unique_window, status) s" \
  -c "SELECT 'evicted ttl holds (fixture rows): ' || count(*) FROM autumn_jobs
      WHERE id LIKE 'ur%' AND unique_key IS NULL"
rm -rf "$WORK"
