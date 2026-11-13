#!/usr/bin/env bash
# Write cost of idx_autumn_jobs_unique_key_enqueued. Two fresh databases with
# the same fixture (schema_before.sql, schema.sql), then the same writes, one
# statement per row, after a CHECKPOINT: 2,000 plain inserts, 2,000
# unique-key inserts, a claim (-> running) and an ack (-> completed) of the
# unique rows. WAL from pg_stat_statements (records, full-page images, bytes)
# and the LSN delta. Then the index build on the baseline database (time,
# lock, size) and down.sql.
# Usage: write_cost.sh [hist] (default 1000000)
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
HIST="${1:-1000000}"
MIG="$HERE/../../../autumn/migrations/20261112170000_add_jobs_unique_key_enqueued_index"
for s in schema_before.sql schema.sql; do
  DB=ledger_jobenqueue_wc_${s%.sql}
  dropdb --if-exists "$DB" 2>/dev/null; createdb "$DB"
  export PGDATABASE="$DB"
  psql -qX -c 'CREATE EXTENSION IF NOT EXISTS pg_stat_statements' >/dev/null
  (cd "$HERE" && psql -qX -v ON_ERROR_STOP=1 -f "$s" >/dev/null 2>&1)
  psql -qX -v ON_ERROR_STOP=1 -v hist="$HIST" -f "$HERE/fixture.sql" >/dev/null
  echo "=== $s, hist=$HIST"
  psql -X -q -v ON_ERROR_STOP=1 <<'SQL'
CHECKPOINT;
SELECT pg_stat_statements_reset() \gset
SELECT pg_current_wal_lsn() AS l0 \gset
DO $$ BEGIN FOR g IN 1..2000 LOOP
  INSERT INTO autumn_jobs (id, name, status, queue) VALUES ('wp' || g, 'webhook.send', 'enqueued', 'default');
END LOOP; END $$;
SELECT pg_current_wal_lsn() AS l1 \gset
DO $$ BEGIN FOR g IN 1..2000 LOOP
  INSERT INTO autumn_jobs (id, name, status, queue, unique_key, unique_window)
  VALUES ('wu' || g, 'digest.send', 'enqueued', 'default', 'wc:' || g, 'running');
END LOOP; END $$;
SELECT pg_current_wal_lsn() AS l2 \gset
DO $$ BEGIN FOR g IN 1..2000 LOOP
  UPDATE autumn_jobs SET status = 'running', started_at = now(), claimed_by = 'w1', claimed_at = now()
  WHERE id = 'wu' || g;
END LOOP; END $$;
DO $$ BEGIN FOR g IN 1..2000 LOOP
  UPDATE autumn_jobs SET status = 'completed', finished_at = now() WHERE id = 'wu' || g;
END LOOP; END $$;
SELECT pg_current_wal_lsn() AS l3 \gset
SELECT CASE WHEN query LIKE 'INSERT%' AND query NOT LIKE '%unique_key%' THEN '1 plain insert'
            WHEN query LIKE 'INSERT%' THEN '2 unique-key insert'
            WHEN query LIKE '%claimed_by%' THEN '3 claim update'
            ELSE '4 ack update' END AS op,
       calls, wal_records, wal_fpi, wal_bytes, shared_blks_hit + shared_blks_read AS buffers
FROM pg_stat_statements
WHERE query ~ '^(INSERT|UPDATE) autumn_jobs|^INSERT INTO autumn_jobs' ORDER BY 1;
SELECT pg_wal_lsn_diff(:'l1', :'l0') AS lsn_plain, pg_wal_lsn_diff(:'l2', :'l1') AS lsn_unique,
       pg_wal_lsn_diff(:'l3', :'l2') AS lsn_claim_ack;
SQL
done
echo "=== build on the baseline fixture (hist=$HIST, plus the writes above)"
export PGDATABASE=ledger_jobenqueue_wc_schema_before
psql -X -v ON_ERROR_STOP=1 <<SQL
\timing on
BEGIN;
\i $MIG/up.sql
SELECT mode, granted FROM pg_locks WHERE relation = 'autumn_jobs'::regclass AND pid = pg_backend_pid();
COMMIT;
\timing off
SELECT pg_size_pretty(pg_relation_size('idx_autumn_jobs_unique_key_enqueued')) AS index_size,
       pg_relation_size('idx_autumn_jobs_unique_key_enqueued') / 8192 AS index_blocks,
       (SELECT count(*) FROM autumn_jobs WHERE unique_key IS NOT NULL) AS indexed_rows,
       (SELECT count(*) FROM autumn_jobs) AS table_rows,
       pg_size_pretty(pg_relation_size('autumn_jobs')) AS heap_size;
\echo '=== rollback (down.sql)'
\timing on
\i $MIG/down.sql
\timing off
SELECT count(*) AS left_behind FROM pg_indexes WHERE indexname = 'idx_autumn_jobs_unique_key_enqueued';
SQL
