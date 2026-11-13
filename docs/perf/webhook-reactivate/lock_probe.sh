#!/usr/bin/env bash
# Does a successful delivery's log transaction hold the subscription row
# lock until it commits? Session A runs log_with_outcome's success path for
# a healthy subscription ('1') and waits before COMMIT; session B then asks
# for the row with FOR UPDATE NOWAIT, as a concurrent delivery's
# REACTIVATE or FAILURE_SQL to the same receiver would.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
WORK="$(mktemp -d)"
python3 "$HERE/extract_sql.py" "$WORK"
SQL="$(sed 's/\$1/'"'1'"'/' "$WORK/reactivate.sql")"
export PGDATABASE="${DB:-ledger_webhook}"
psql -X -q -c "UPDATE autumn_webhook_subscriptions SET status = 'active', consecutive_failures = 0 WHERE id = '1'"
psql -X -q -c "BEGIN" -c "INSERT INTO autumn_webhook_deliveries VALUES ('probe', '1', 't', '{}', '{}', 200, 'ok', 1, 1, 5, false, NULL, 0)" \
     -c "$SQL" -c "SELECT pg_sleep(3)" -c "ROLLBACK" >/dev/null &
sleep 1
psql -X -At -c "SELECT locktype, mode, granted FROM pg_locks l JOIN pg_class c ON c.oid = l.relation WHERE c.relname = 'autumn_webhook_subscriptions' AND l.pid <> pg_backend_pid()" | sed 's/^/lock held by A: /'
if psql -X -q -At -v ON_ERROR_STOP=1 -c "BEGIN" -c "SELECT id FROM autumn_webhook_subscriptions WHERE id = '1' FOR UPDATE NOWAIT" -c "ROLLBACK" >/dev/null 2>"$WORK/err"; then
  echo "B: row '1' is free while A is open"
else
  echo "B: $(cat "$WORK/err")"
fi
wait
