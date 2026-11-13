#!/usr/bin/env bash
# EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS) of the shipped REACTIVATE_SQL
# on the database run.sh left: once for a healthy hot subscription ('1', 0
# failures) and once for a flaky one ('7'), each in a rolled-back transaction.
# Also reports whether the statement took a transaction id (it does when it
# writes a row version).
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
WORK="$(mktemp -d)"
python3 "$HERE/extract_sql.py" "$WORK"
SQL="$(cat "$WORK/reactivate.sql")"
export PGDATABASE="${DB:-ledger_webhook}"
psql -X -q -c "UPDATE autumn_webhook_subscriptions SET consecutive_failures = 3 WHERE id = '7'"
for id in 1 7; do
  echo "== subscription '$id'"
  psql -X -q <<SQL
SELECT id, status, consecutive_failures FROM autumn_webhook_subscriptions WHERE id = '$id';
BEGIN;
PREPARE reactivate(text) AS $SQL;
EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS, COSTS OFF, TIMING OFF) EXECUTE reactivate('$id');
SELECT pg_current_xact_id_if_assigned() IS NOT NULL AS wrote_a_row_version;
ROLLBACK;
SQL
done
