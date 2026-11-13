#!/usr/bin/env bash
# Ledger harness: concurrency-saturated single-queue job claim.
# Usage: [CLAIM=claim_after.sql] run.sh <saturated-backlog> [hist] > out.txt
# Needs Postgres 16 with pg_stat_statements preloaded; PG* env selects the server.
set -euo pipefail
cd "$(dirname "$0")"
SAT=${1:?saturated backlog rows}
HIST=${2:-1000000}
DB=ledger_jobclaim
CLAIM=${CLAIM:-claim.sql}
echo "=== claim: $CLAIM"
dropdb --if-exists "$DB"; createdb "$DB"
psql -qX -d "$DB" -c 'CREATE EXTENSION IF NOT EXISTS pg_stat_statements' >/dev/null
psql -qX -v ON_ERROR_STOP=1 -d "$DB" -f schema.sql >/dev/null
psql -qX -v ON_ERROR_STOP=1 -d "$DB" -v hist="$HIST" -v sat="$SAT" -f fixture.sql >/dev/null
echo "=== fixture: hist=$HIST sat=$SAT"
psql -X -d "$DB" -c "SELECT status, count(*) FROM autumn_jobs GROUP BY 1 ORDER BY 1" \
  -c "SELECT pg_size_pretty(pg_relation_size('autumn_jobs')) AS heap"
echo "=== EXPLAIN first claim (rolled back)"
psql -X -d "$DB" <<SQL
\i $CLAIM
SET max_parallel_workers_per_gather = 2;
BEGIN;
EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS) EXECUTE claim('worker-1', 'default', '{}');
ROLLBACK;
SQL
echo "=== workload: 100 claim+ack cycles"
OUT=$(mktemp)
psql -qX -v ON_ERROR_STOP=1 -d "$DB" -v out="$OUT" -v claim="$CLAIM" -f workload.sql >/dev/null
echo "claimed-id md5 (in order): $(grep -v '^$' "$OUT" | md5sum | cut -d' ' -f1)"
echo "claimed names: $(grep -v '^$' "$OUT" | tr -d ' ' | cut -c1 | sort | uniq -c | tr '\n' ' ')"
rm -f "$OUT"
psql -X -d "$DB" -f report.sql
