#!/usr/bin/env bash
# Result equivalence of REACTIVATE_SQL at BASE (default: the parent of the
# change) and in the working tree. Each version runs on its own copy of an
# edge-case table: every status the store writes or could read
# (active, disabled, failed, and an unknown one) x consecutive_failures
# 0, 1, 49, 50, plus an id with no row. Each statement runs once per id, then
# again (a second success). Prints the rows after each pass and their md5.
# Usage: equivalence.sh [BASE]
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../../.." && pwd)"
BASE="${1:-HEAD~1}"
WORK="$(mktemp -d)"
git -C "$ROOT" show "$BASE:autumn/src/webhook_outbound/sql_store.rs" > "$WORK/base.rs"
SQL_STORE_RS="$WORK/base.rs" python3 "$HERE/extract_sql.py" "$WORK/base"
python3 "$HERE/extract_sql.py" "$WORK/tree"
export PGDATABASE="${DB:-ledger_webhook_eq}"
psql -X -q -d postgres -c "DROP DATABASE IF EXISTS $PGDATABASE" -c "CREATE DATABASE $PGDATABASE"
psql -X -q -v ON_ERROR_STOP=1 -f "$WORK/tree/schema.sql"
for side in base tree; do
  psql -X -q -v ON_ERROR_STOP=1 <<SQL
DELETE FROM autumn_webhook_subscriptions;
INSERT INTO autumn_webhook_subscriptions
SELECT s || '-' || f, 'https://r.example', '["t"]', 'k', s, f
FROM unnest(ARRAY['active','disabled','failed','paused']) s, unnest(ARRAY[0,1,49,50]) f;
SQL
  for pass in 1 2; do
    ids="$(psql -X -At -c "SELECT id FROM autumn_webhook_subscriptions ORDER BY id")
missing"
    for id in $ids; do
      psql -X -q -v ON_ERROR_STOP=1 -c "$(sed "s/\\\$1/'$id'/" "$WORK/$side/reactivate.sql")" >/dev/null
    done
    echo "== $side, pass $pass"
    psql -X -At -c "SELECT string_agg(id || '=' || status || '/' || consecutive_failures, ' ' ORDER BY id) FROM autumn_webhook_subscriptions"
    psql -X -At -c "SELECT 'md5 ' || md5(string_agg(id || '|' || target_url || '|' || event_topics || '|' || secret || '|' || status || '|' || consecutive_failures, ',' ORDER BY id)) FROM autumn_webhook_subscriptions"
  done
done
