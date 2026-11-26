#!/usr/bin/env bash
# Ledger harness: the outbox write -> relay claim -> Inbox::seen -> mark sent
# cycle on a production-shaped outbox/inbox history (fixture.sql). Every
# statement is the text outbox.rs ships (gen_scripts.py). Prints the
# pg_stat_statements profile, WAL, and full-page images by relation.
# Usage: [MSG_ID=v4|v7] [FIXTURE_IDS=v4|v7] run.sh [messages] [history].
# MSG_ID is the id the workload's writes make (default: what outbox.rs
# ships), FIXTURE_IDS the history's (default v4: every message before this
# change made one). Needs PG* env pointing at a Postgres 16 server that
# preloads pg_stat_statements with pg_stat_statements.track = all,
# autovacuum off, and pg_waldump on the PATH of the database host.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
MSGS="${1:-20000}"
HIST="${2:-1000000}"
NOW_MS="${NOW_MS:-1764000000000}"
export NOW_MS
DB="${DB:-ledger_outbox_id}"
WORK="$(mktemp -d)"
python3 "$HERE/gen_scripts.py" "$WORK" "$MSGS"

dropdb --if-exists "$DB"; createdb "$DB"
export PGDATABASE="$DB"
psql -qX -c 'CREATE EXTENSION IF NOT EXISTS pg_stat_statements' >/dev/null
psql -qX -v ON_ERROR_STOP=1 -f "$WORK/schema.sql" >/dev/null
psql -qX -v ON_ERROR_STOP=1 -f "$HERE/uuid_v7.sql" >/dev/null
case "${FIXTURE_IDS:-v4}" in v4) IDFN='gen_random_uuid()::text' ;; v7) IDFN='uuid_v7()' ;; *) exit 2 ;; esac
psql -qX -v ON_ERROR_STOP=1 -v hist="$HIST" -v idfn="$IDFN" -v now="$NOW_MS" -f "$HERE/fixture.sql" >/dev/null
echo "=== fixture: history=$HIST, history ids ${FIXTURE_IDS:-v4}; workload: messages=$MSGS, message ids $(cat "$WORK/msg_id")"
psql -X -c "SELECT (SELECT count(*) FROM autumn_outbox) AS outbox_rows,
                   (SELECT count(*) FROM autumn_outbox WHERE dispatched_at IS NULL) AS pending,
                   (SELECT count(*) FROM autumn_inbox) AS inbox_rows" \
  -c "SELECT c.relname, pg_size_pretty(pg_relation_size(c.oid)) AS size, pg_relation_size(c.oid) / 8192 AS blocks
        FROM pg_class c WHERE c.relname LIKE '%autumn\_outbox%' OR c.relname LIKE '%autumn\_inbox%' ORDER BY 1"
psql -qX -c 'CHECKPOINT' -c 'SELECT pg_stat_statements_reset()' -c 'SELECT pg_stat_reset()' -c "SELECT pg_stat_reset_shared('wal')" >/dev/null
LSN0="$(psql -XAt -c 'SELECT pg_current_wal_lsn()')"
echo "=== workload"
psql -qX -v ON_ERROR_STOP=1 -f "$WORK/workload.sql"
psql -qX -c 'SELECT pg_stat_force_next_flush()' >/dev/null
LSN1="$(psql -XAt -c 'SELECT pg_current_wal_lsn()')"
psql -X -c "SELECT count(*) FILTER (WHERE dispatched_at IS NULL) AS left_pending,
                   count(*) FILTER (WHERE seq > (SELECT max(seq) - $MSGS FROM autumn_outbox) AND dispatched_at IS NOT NULL) AS sent_by_workload
              FROM autumn_outbox" \
  -c "SELECT count(*) AS inbox_rows_after FROM autumn_inbox"
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
