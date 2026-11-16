#!/usr/bin/env bash
# Result equivalence for the transaction id change: run.sh's seeded workload
# twice on the same fixture, once with v4 post ids and once with v7, then
# compare everything a caller of the ledger can read, with the id itself
# replaced by the idempotency key it belongs to:
#   accounts: id, currency, allow_negative, balance_minor
#   transactions: idempotency_key, request_hash, currency, memo
#   postings: the owning key, seq, account_id, amount_minor, currency
#   trial balance per currency, and the read-back query's rows per key
# plus: every id is a canonical UUID (counted by version), unique, and
# every posting finds its transaction (the deferred FK held at COMMIT).
# Usage: equivalence.sh [T] [txns]
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
T="${1:-2000}"; TXNS="${2:-100000}"
for v in v4 v7; do TXN_ID=$v DB=lm_eq_$v "$HERE/run.sh" "$T" "$TXNS" >/dev/null 2>&1; done
dump() {
  psql -XAt -d "$1" <<'SQL'
SELECT 'accounts', count(*), md5(string_agg(concat_ws('|', id, currency, allow_negative, balance_minor), E'\n' ORDER BY id))
  FROM _autumn_money_accounts;
SELECT 'transactions', count(*), md5(string_agg(concat_ws('|', idempotency_key, request_hash, currency, memo), E'\n' ORDER BY idempotency_key))
  FROM _autumn_money_transactions;
SELECT 'postings', count(*), md5(string_agg(concat_ws('|', t.idempotency_key, p.seq, p.account_id, p.amount_minor, p.currency), E'\n' ORDER BY t.idempotency_key, p.seq))
  FROM _autumn_money_postings p JOIN _autumn_money_transactions t ON t.id = p.transaction_id;
SELECT 'orphan postings', count(*), '' FROM _autumn_money_postings p
 WHERE NOT EXISTS (SELECT 1 FROM _autumn_money_transactions t WHERE t.id = p.transaction_id);
SELECT 'trial balance', count(*), string_agg(currency || '=' || total, ',' ORDER BY currency)
  FROM (SELECT currency, sum(amount_minor) AS total FROM _autumn_money_postings GROUP BY currency) s;
SELECT 'read-back rows', count(*), md5(string_agg(concat_ws('|', t.idempotency_key, r.account_id, r.amount_minor, r.currency, r.ord), E'\n' ORDER BY t.idempotency_key, r.ord))
  FROM _autumn_money_transactions t,
       LATERAL (SELECT account_id, amount_minor, currency, row_number() OVER (ORDER BY seq) AS ord
                  FROM _autumn_money_postings WHERE transaction_id = t.id ORDER BY seq) r;
SELECT 'ids unique', (count(DISTINCT id) = count(*))::text, '' FROM _autumn_money_transactions;
SELECT 'ids by version', substr(id, 15, 1), count(*)::text || ' ids, all canonical: ' || bool_and(id ~ '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$')
  FROM _autumn_money_transactions GROUP BY 2 ORDER BY 2;
SQL
}
dump lm_eq_v4 > "${TMPDIR:-/tmp}/eq_v4.txt"; dump lm_eq_v7 > "${TMPDIR:-/tmp}/eq_v7.txt"
echo "=== v4 post ids"; cat "${TMPDIR:-/tmp}/eq_v4.txt"
echo "=== v7 post ids"; cat "${TMPDIR:-/tmp}/eq_v7.txt"
echo "=== diff (the version line is the only one expected to differ)"
diff "${TMPDIR:-/tmp}/eq_v4.txt" "${TMPDIR:-/tmp}/eq_v7.txt" || true
