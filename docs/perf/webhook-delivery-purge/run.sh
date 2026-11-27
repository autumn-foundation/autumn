#!/usr/bin/env bash
# Webhook delivery-log retention: one hour of steady state on a 30-day log.
# LOGS logs in the fixture, LOGS/720 deliveries through the shipped store SQL
# (the hour's traffic), then the hourly purge_deliveries_before with a 30-day
# cutoff. Prints the pg_stat_statements profile, WAL and table counters.
# Usage: run.sh [LOGS] (default 1000000). Needs PG* env pointing at a
# Postgres 16 server that preloads pg_stat_statements (autovacuum off).
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
LOGS="${1:-1000000}"
N=$((LOGS / 720))
DB="${DB:-ledger_webhook_purge}"
# now = 1762909200000 (end of the workload hour); 30-day retention.
CUTOFF=$((1762909200000 - 2592000000))
WORK="$(mktemp -d)"
python3 "$HERE/gen_sql.py" "$WORK"

psql -X -q -d postgres -c "DROP DATABASE IF EXISTS $DB" -c "CREATE DATABASE $DB"
export PGDATABASE="$DB"
psql -X -q -v ON_ERROR_STOP=1 -v logs="$LOGS" \
    -c 'CREATE EXTENSION IF NOT EXISTS pg_stat_statements' \
    -f "$WORK/schema.sql" -f "$HERE/fixture.sql" >/dev/null
psql -X -q -c 'VACUUM ANALYZE' -c 'CHECKPOINT'
psql -X -q -c 'SELECT pg_stat_statements_reset()' -c 'SELECT pg_stat_reset()' >/dev/null
LSN0="$(psql -X -At -c 'SELECT pg_current_wal_lsn()')"

echo "== fixture: $LOGS logs; workload: $N deliveries, then one purge (cutoff $CUTOFF)"
pgbench -n -M prepared --random-seed=3742 -c 1 -t "$N" \
    -f "$WORK/deliver_ok.pgbench@98" -f "$WORK/deliver_retry.pgbench@2" \
    | grep -E 'processed|failed'
LSN1="$(psql -X -At -c 'SELECT pg_current_wal_lsn()')"
if [ -n "${EXPLAIN:-}" ]; then
    # explain.sh: the purge under EXPLAIN ANALYZE (it still deletes).
    { echo 'EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS)'; cat "$WORK/purge.sql"; } \
        | psql -X -v ON_ERROR_STOP=1 -v cutoff="$CUTOFF"
    exit 0
fi
psql -X -v ON_ERROR_STOP=1 -v cutoff="$CUTOFF" -f "$WORK/purge.sql"
psql -X -q -c 'SELECT pg_stat_force_next_flush()' >/dev/null
psql -X -v lsn0="$LSN0" -v lsn1="$LSN1" -f "$HERE/report.sql"
# Result identity: every log the purge left, in id order.
echo "== end state (rows left, md5 of every row in id order)"
psql -X -At -c "SELECT count(*), md5(string_agg(t::text, E'\\n' ORDER BY id)) \
    FROM autumn_webhook_deliveries t"
