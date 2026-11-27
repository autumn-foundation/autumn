#!/usr/bin/env bash
# The upgrade path on the database run.sh left: drop the delivered-logs index,
# build it the way ensure_schema does for existing tables (DELIVERED_INDEX_SQL,
# CREATE INDEX CONCURRENTLY), sample the locks it holds while a writer keeps
# inserting, then roll it back with DROP INDEX CONCURRENTLY. Same PG* env.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
export PGDATABASE="${DB:-ledger_webhook_purge}"
SQL="$(python3 - "$HERE/../../../autumn/src/webhook_outbound/sql_store.rs" <<'PY'
import re, sys
src = open(sys.argv[1]).read()
m = re.search(r'const DELIVERED_INDEX_SQL: &str =\s*"((?:[^"\\]|\\.)*)"', src, re.S)
if not m:
    sys.exit("DELIVERED_INDEX_SQL not found")
print(re.sub(r'\\\n\s*', '', m.group(1)))
PY
)"
echo "== $SQL"
psql -X -q -c 'DROP INDEX IF EXISTS idx_autumn_webhook_deliveries_delivered'
psql -X -At -c "SELECT count(*) || ' rows, heap ' || pg_size_pretty(pg_relation_size('autumn_webhook_deliveries')) FROM autumn_webhook_deliveries"
# A writer that inserts while the build runs: it must never wait.
( for i in $(seq 1 200); do
    psql -X -q -c "INSERT INTO autumn_webhook_deliveries (id, subscription_id, topic, payload, request_headers, response_status, elapsed_ms, attempt, max_attempts, is_dlq, logged_at) VALUES ('mig_$i', '1', 't', '{}', '{}', 200, 1, 1, 5, false, 1762909300000)"
  done ) &
WRITER=$!
( sleep 0.3; for i in $(seq 1 20); do
    psql -X -At -c "SELECT 'lock ' || l.mode || ' granted=' || l.granted || ' by ' || left(a.query, 30) FROM pg_locks l JOIN pg_stat_activity a USING (pid) WHERE l.relation = 'autumn_webhook_deliveries'::regclass AND a.pid <> pg_backend_pid()"
    psql -X -At -c "SELECT 'wait ' || coalesce(wait_event_type, '-') || '/' || coalesce(wait_event, '-') || ' ' || left(query, 30) FROM pg_stat_activity WHERE datname = current_database() AND pid <> pg_backend_pid() AND state = 'active'"
    sleep 0.2
  done ) > /tmp/migration-samples.txt &
SAMPLER=$!
START=$(date +%s%N)
psql -X -q -c "$SQL"
END=$(date +%s%N)
wait $WRITER $SAMPLER
echo "build: $(( (END - START) / 1000000 )) ms (wall clock, for scale only)"
echo "lock samples on autumn_webhook_deliveries (distinct):"
sort /tmp/migration-samples.txt | uniq -c | sort -rn
psql -X -At -c "SELECT 'valid=' || i.indisvalid || ' size ' || pg_size_pretty(pg_relation_size(i.indexrelid)) FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid WHERE c.relname = 'idx_autumn_webhook_deliveries_delivered'"
psql -X -At -c "SELECT 'writer rows inserted during the build: ' || count(*) FROM autumn_webhook_deliveries WHERE id LIKE 'mig\_%'"
echo "== rollback: DROP INDEX CONCURRENTLY"
START=$(date +%s%N)
psql -X -q -c 'DROP INDEX CONCURRENTLY idx_autumn_webhook_deliveries_delivered'
END=$(date +%s%N)
echo "drop: $(( (END - START) / 1000000 )) ms"
psql -X -At -c "SELECT 'index present after rollback: ' || (to_regclass('idx_autumn_webhook_deliveries_delivered') IS NOT NULL)"
psql -X -q -c "DELETE FROM autumn_webhook_deliveries WHERE id LIKE 'mig\_%'" -c "$SQL"
