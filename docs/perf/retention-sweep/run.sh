#!/usr/bin/env bash
# One full retention pass over each sweepable Postgres table, as sweep_postgres
# in autumn/src/data_retention.rs runs it: resolve the cutoff, COUNT, DELETE
# batches of 500 (each its own autocommit statement) until one deletes nothing
# or 1,000 batches ran, then COUNT again. extract_sql.py reads the statements
# from the source at the checked-out commit. pg_stat_statements and the table
# counters are reset before each table's pass and printed after it.
#
#   run.sh <rows> [window_days]     (PG* env set; PGDATABASE default `ledger`)
set -euo pipefail
n=${1:?rows}; days=${2:-30}
here=$(cd "$(dirname "$0")" && pwd)
export PGDATABASE=${PGDATABASE:-ledger}
export PGOPTIONS="${PGOPTIONS:-} -c client_min_messages=warning"
eval "$(python3 "$here/extract_sql.py")"
psql -q -X -v ON_ERROR_STOP=1 -v n="$n" -f "$here/fixture.sql" >/dev/null

pass() { # table count-sql batch-sql
  local table=$1 count=$2 batch=$3
  sleep 1 # earlier sessions' table counters flush before the snapshot
  psql -q -X -c "SELECT pg_stat_statements_reset()" >/dev/null
  psql -q -X -c "CREATE UNLOGGED TABLE IF NOT EXISTS ledger_counters AS SELECT * FROM pg_stat_user_tables LIMIT 0; TRUNCATE ledger_counters; INSERT INTO ledger_counters SELECT * FROM pg_stat_user_tables WHERE relid = '$table'::regclass" >/dev/null
  {
    echo "\\set QUIET on"
    echo "\\set ON_ERROR_STOP on"
    # diesel-async prepares a raw sql_query as an unnamed statement on every call,
    # so every batch is a custom plan. A reused PREPARE would go generic after 5.
    echo "SET plan_cache_mode = force_custom_plan;"
    echo "SELECT NOW() - make_interval(secs => $((days * 86400))) AS cutoff \\gset"
    echo "PREPARE cnt(timestamptz) AS $count;"
    echo "PREPARE batch(timestamptz, bigint) AS $batch;"
    echo "EXECUTE cnt(:'cutoff');"
    for _ in $(seq 1 1000); do
      echo "EXECUTE batch(:'cutoff', 500);"
      echo "SELECT :ROW_COUNT = 0 AS done \\gset"
      echo "\\if :done \\else"
    done
    for _ in $(seq 1 1000); do echo "\\endif"; done
    echo "EXECUTE cnt(:'cutoff');"
  } | psql -X -At >/dev/null
  sleep 1 # let the driver's table counters flush
  echo "== $table (rows=$n, window=${days}d)"
  psql -X -P pager=off -c "
    SELECT CASE WHEN query LIKE 'PREPARE cnt%' THEN 'count' ELSE 'delete batch' END AS stmt,
           calls, rows,
           shared_blks_hit + shared_blks_read AS buffers,
           shared_blks_hit AS hit, shared_blks_read AS read,
           temp_blks_written AS temp_w, wal_bytes,
           round((shared_blks_hit + shared_blks_read)::numeric / NULLIF(calls, 0), 1) AS buffers_per_call
    FROM pg_stat_statements
    WHERE query LIKE 'PREPARE cnt%' OR query LIKE 'PREPARE batch%'
    ORDER BY 1" -c "
    SELECT t.seq_scan - c.seq_scan AS seq_scan, t.seq_tup_read - c.seq_tup_read AS seq_tup_read,
           t.idx_scan - c.idx_scan AS idx_scan, t.idx_tup_fetch - c.idx_tup_fetch AS idx_tup_fetch,
           t.n_tup_del - c.n_tup_del AS n_tup_del,
           pg_relation_size('$table') / 8192 AS heap_blocks
    FROM pg_stat_user_tables t JOIN ledger_counters c USING (relid)
    WHERE t.relid = '$table'::regclass"
}
pass "$JOBS_TABLE" "$JOBS_COUNT" "$JOBS_BATCH"
pass "$TRACK_TABLE" "$TRACK_COUNT" "$TRACK_BATCH"
pass "$HOOKS_TABLE" "$HOOKS_COUNT" "$HOOKS_BATCH"
pass "$EXP_TABLE" "$EXP_COUNT" "$EXP_BATCH"
