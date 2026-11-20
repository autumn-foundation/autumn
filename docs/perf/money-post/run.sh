#!/usr/bin/env bash
# Ledger harness: money::ledger::post on a production-shaped marketplace
# ledger (fixture.sql). T pgbench transactions, seeded: 85% a new 3-leg
# charge, 10% a retried charge (replay), 5% a new 20-leg payout. Every
# statement is the text ledger.rs ships (gen_scripts.py). Prints the
# pg_stat_statements profile (top-level and nested), WAL and table counters.
# Usage: [TXN_ID=v4|v7] [FIXTURE_IDS=v4|v7] run.sh [T] [txns]. TXN_ID is the
# id the workload's posts make (default: what ledger.rs ships), FIXTURE_IDS
# the history's (default v4: every post before this change made one). Needs PG* env pointing
# at a Postgres 16 server that preloads pg_stat_statements with
# pg_stat_statements.track = all, autovacuum off.
set -euo pipefail
export PGOPTIONS="${PGOPTIONS:-} -c synchronize_seqscans=off"
HERE="$(cd "$(dirname "$0")" && pwd)"
T="${1:-2000}"
TXNS="${2:-1000000}"
DB="${DB:-ledger_moneypost}"
WORK="$(mktemp -d)"
python3 "$HERE/gen_scripts.py" "$WORK"

dropdb --if-exists "$DB"; createdb "$DB"
export PGDATABASE="$DB"
psql -qX -c 'CREATE EXTENSION IF NOT EXISTS pg_stat_statements' >/dev/null
psql -qX -v ON_ERROR_STOP=1 -f "$WORK/schema.sql" >/dev/null
psql -qX -v ON_ERROR_STOP=1 -f "$HERE/uuid_v7.sql" >/dev/null
case "${FIXTURE_IDS:-v4}" in v4) IDFN='gen_random_uuid()::text' ;; v7) IDFN='uuid_v7()' ;; *) exit 2 ;; esac
psql -qX -v ON_ERROR_STOP=1 -v txns="$TXNS" -v idfn="$IDFN" -f "$HERE/fixture.sql" >/dev/null
echo "=== fixture: txns=$TXNS, history ids ${FIXTURE_IDS:-v4}; workload: T=$T, post ids $(cat "$WORK/txn_id")"
psql -X -c "SELECT (SELECT count(*) FROM _autumn_money_accounts) AS accounts,
                   (SELECT count(*) FROM _autumn_money_accounts WHERE balance_minor IS NULL) AS null_balance,
                   (SELECT count(*) FROM _autumn_money_transactions) AS transactions,
                   (SELECT count(*) FROM _autumn_money_postings) AS postings" \
  -c "SELECT relname, pg_size_pretty(pg_relation_size(oid)) AS heap, pg_relation_size(oid) / 8192 AS heap_blocks,
             pg_size_pretty(pg_indexes_size(oid)) AS indexes
        FROM pg_class WHERE relname IN ('_autumn_money_accounts', '_autumn_money_transactions', '_autumn_money_postings') ORDER BY 1"
psql -qX -c 'CHECKPOINT' -c 'SELECT pg_stat_statements_reset()' -c 'SELECT pg_stat_reset()' -c "SELECT pg_stat_reset_shared('wal')" >/dev/null
LSN0="$(psql -XAt -c 'SELECT pg_current_wal_lsn()')"
echo "=== workload"
QS=()
for i in $(seq 0 19); do QS+=(-D "q$i=$i"); done
pgbench -n -M extended --random-seed=3743 -c 1 -t "$T" \
  -D txns="$TXNS" -D cur=USD -D fees=platform:fees -D pool=payout:pool "${QS[@]}" \
  -f "$WORK/charge.pgbench@85" -f "$WORK/replay.pgbench@10" -f "$WORK/payout.pgbench@5" \
  | grep -E 'SQL script|weight|processed|failed'
psql -qX -c 'SELECT pg_stat_force_next_flush()' >/dev/null
LSN1="$(psql -XAt -c 'SELECT pg_current_wal_lsn()')"
psql -X -v lsn0="$LSN0" -f "$HERE/report.sql"
# Full-page images by relation, from the WAL itself (pg_waldump reads the
# server's pg_wal: run this on the database host).
echo "=== full-page images in the workload's WAL, by relation (pg_waldump)"
PGWAL="$(psql -XAt -c 'SHOW data_directory')/pg_wal"
pg_waldump -p "$PGWAL" -s "$LSN0" -e "$LSN1" 2>/dev/null \
  | grep -oE 'rel [0-9]+/[0-9]+/[0-9]+ (fork [a-z]+ )?blk [0-9]+ FPW' \
  | awk '{print $2}' | sort | uniq -c \
  | while read -r n rel; do
      node="${rel##*/}"
      name="$(psql -XAt -c "SELECT coalesce(pg_filenode_relation(0, $node)::text, '$rel')")"
      echo "$n $name"
    done | sort -rn | awk '{printf "%8d  %s\n", $1, $2}'
rm -rf "$WORK"
