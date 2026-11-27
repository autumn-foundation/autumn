#!/usr/bin/env bash
# The purge deletes the same rows with and without the delivered-logs index,
# on the boundary cases of its predicate. Same PG* env as run.sh.
# Each case is run twice on one table: once with the index dropped (Seq Scan),
# once with it and enable_seqscan off (the index must serve the plan), each in
# a rolled-back transaction; the deleted ids must match.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
DB="${DB:-ledger_webhook_purge_eq}"
WORK="$(mktemp -d)"
python3 "$HERE/gen_sql.py" "$WORK"
grep -q idx_autumn_webhook_deliveries_delivered "$WORK/schema.sql" \
    || { echo "schema has no idx_autumn_webhook_deliveries_delivered"; exit 1; }
PURGE="$(sed 's/;$//' "$WORK/purge.sql")"
psql -X -q -d postgres -c "DROP DATABASE IF EXISTS $DB" -c "CREATE DATABASE $DB"
export PGDATABASE="$DB"
psql -X -q -v ON_ERROR_STOP=1 -f "$WORK/schema.sql"
# Boundary rows: cutoff 1000. logged_at at cutoff-1 / cutoff / cutoff+1,
# status 199 / 200 / 204 / 299 / 300 / 500 / NULL, is_dlq true and false,
# a 2xx DLQ row (a replayed DLQ log), logged_at 0 and negative.
psql -X -q -v ON_ERROR_STOP=1 <<'SQL'
INSERT INTO autumn_webhook_deliveries
    (id, subscription_id, topic, payload, request_headers, response_status, response_body,
     elapsed_ms, attempt, max_attempts, is_dlq, last_error, logged_at)
SELECT format('e_%s_%s_%s', coalesce(st::text, 'null'), dlq, ts), '1', 't', '{}', '{}', st, NULL,
       1, 1, 5, dlq, NULL, ts
FROM unnest(ARRAY[199, 200, 204, 299, 300, 500, NULL]::int[]) st,
     unnest(ARRAY[false, true]) dlq,
     unnest(ARRAY[-5, 0, 998, 999, 1000, 1001]::bigint[]) ts;
ANALYZE autumn_webhook_deliveries;
SQL
run() {
    psql -X -At -v ON_ERROR_STOP=1 -v cutoff=1000 <<SQL
BEGIN;
$1
CREATE TEMP TABLE gone ON COMMIT DROP AS SELECT id FROM autumn_webhook_deliveries;
$PURGE;
SELECT 'deleted ' || count(*) || ' ' || md5(string_agg(id, ',' ORDER BY id)) || ' ' || string_agg(id, ',' ORDER BY id)
FROM gone WHERE id NOT IN (SELECT id FROM autumn_webhook_deliveries);
ROLLBACK;
SQL
}
plan() {
    psql -X -At -c "BEGIN" -c "$1" -c "EXPLAIN (COSTS OFF) ${PURGE//:cutoff/1000}" -c "ROLLBACK" \
        | grep -E 'Scan'
}
NOIDX="DROP INDEX idx_autumn_webhook_deliveries_delivered;"
IDX="SET LOCAL enable_seqscan = off;"
echo "without the index: $(plan "$NOIDX")"
A="$(run "$NOIDX" | grep '^deleted')"
echo "with the index:    $(plan "$IDX")"
B="$(run "$IDX" | grep '^deleted')"
echo "$A"
echo "$B"
plan "$IDX" | grep -q idx_autumn_webhook_deliveries_delivered || { echo "FAIL: index not used"; exit 1; }
[ "$A" = "$B" ] && echo "PASS: identical deleted sets" || { echo "FAIL: deleted sets differ"; exit 1; }
