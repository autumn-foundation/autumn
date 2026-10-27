#!/usr/bin/env bash
# Usage: run.sh <variant> <outdir> [hot sizes...]   (variant: baseline|after)
# Needs a Postgres with pg_stat_statements preloaded and the autumn_jobs migrations
# applied (PGHOST/PGPORT/PGUSER/PGDATABASE env). One poll = enqueue + claim (deferral on) + ack.
set -euo pipefail
cd "$(dirname "$0")"
variant=$1; out=$2; shift 2; sizes=${*:-"50000 200000 800000"}
mkdir -p "$out"
for hot in $sizes; do
  psql -q -v hot="$hot" -f fixture.sql >/dev/null
  [ "$variant" = after ] && psql -q -f after_index.sql
  psql -q -c "SELECT pg_stat_statements_reset(); SELECT pg_stat_reset();" >/dev/null
  { echo "INSERT INTO autumn_jobs (id, name, status, run_at, queue, payload) VALUES (md5(random()::text), 'job.n1', 'enqueued', NOW(), 'default', '{}');"
    cat claim_$variant.sql
    echo "\\gset"
    cat poll_tail.sql; } > "$out/poll.$hot.sql"
  wal0=$(psql -Atc "select pg_current_wal_lsn()")
  pgbench -n -M prepared -c 1 -t 300 -f "$out/poll.$hot.sql" > "$out/pgbench.$hot.txt" 2>&1
  psql -Atc "select pg_wal_lsn_diff(pg_current_wal_lsn(), '$wal0')" > "$out/wal_bytes.$hot.txt"
  psql -c "select calls, shared_blks_hit+shared_blks_read as blks, shared_blks_hit, shared_blks_read, temp_blks_written, wal_bytes, rows, left(regexp_replace(query,'\s+',' ','g'),90) q from pg_stat_statements where dbid=(select oid from pg_database where datname=current_database()) order by blks desc limit 10" > "$out/pgss.$hot.txt"
  psql -c "select relname, seq_scan, seq_tup_read, idx_scan from pg_stat_user_tables where relname='autumn_jobs'" > "$out/tables.$hot.txt"
  psql -c "select indexrelname, idx_scan, pg_size_pretty(pg_relation_size(indexrelid)) from pg_stat_user_indexes where relname='autumn_jobs' order by indexrelname" > "$out/indexes.$hot.txt"
  psql -f claim_explain_$variant.sql > "$out/explain.$hot.txt"
done
