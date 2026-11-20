#!/usr/bin/env bash
# EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS, WAL) of post's two INSERTs for
# three consecutive 3-leg charges right after a CHECKPOINT, on the database
# run.sh left, rolled back. The first insert into any page after a checkpoint
# writes a full-page image; the second and third charges show whether the
# next id lands on a page the first already dirtied (v7) or a new one (v4).
# Usage: [DB=..] explain.sh v4|v7
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
export PGDATABASE="${DB:-ledger_moneypost}"
case "${1:-v4}" in v4) IDFN='gen_random_uuid()::text' ;; v7) IDFN='uuid_v7()' ;; *) exit 2 ;; esac
psql -qX -f "$HERE/uuid_v7.sql"
{
  echo "CHECKPOINT;"
  echo "BEGIN;"
  for i in 1 2 3; do
    cat <<SQL
SELECT $IDFN AS tid, encode(sha256(('explain:$i:' || clock_timestamp())::bytea), 'hex') AS key \gset
\echo === charge $i: INSERT postings (id :tid)
EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS, WAL, COSTS OFF, TIMING OFF)
INSERT INTO _autumn_money_postings (transaction_id, seq, account_id, amount_minor, currency, posted_at)
VALUES (:'tid', 0, 'customer:$((1000 * i))', -1000, 'USD', CURRENT_TIMESTAMP),
       (:'tid', 1, 'seller:$i', 900, 'USD', CURRENT_TIMESTAMP),
       (:'tid', 2, 'platform:fees', 100, 'USD', CURRENT_TIMESTAMP);
\echo === charge $i: INSERT transaction
EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS, WAL, COSTS OFF, TIMING OFF)
INSERT INTO _autumn_money_transactions (id, idempotency_key, request_hash, currency, memo, posted_at)
VALUES (:'tid', :'key', :'key', 'USD', 'explain', CURRENT_TIMESTAMP)
ON CONFLICT (idempotency_key) DO NOTHING;
\echo === charge $i: read back the postings by transaction id
EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS, COSTS OFF, TIMING OFF)
SELECT account_id, amount_minor, currency FROM _autumn_money_postings WHERE transaction_id = :'tid' ORDER BY seq;
SQL
  done
  echo "ROLLBACK;"
} | psql -X -v ON_ERROR_STOP=1
