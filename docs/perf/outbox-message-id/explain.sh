#!/usr/bin/env bash
# EXPLAIN (ANALYZE, BUFFERS, WAL, VERBOSE, SETTINGS) of the shipped
# INSERT_SQL and INBOX_INSERT_SQL, three times each with a v4 id and three
# times with a v7 id, each set right after a CHECKPOINT, on the database
# run.sh left (PGDATABASE, default ledger_outbox_id). Rolled back.
# The plan is the same for both ids; the WAL line is what differs.
set -euo pipefail
export PGDATABASE="${PGDATABASE:-ledger_outbox_id}"
for v in v4 v7; do
  case "$v" in v4) IDFN='gen_random_uuid()::text' ;; v7) IDFN='uuid_v7()' ;; esac
  echo "=== $v ids (after CHECKPOINT)"
  psql -qX -c CHECKPOINT
  for n in 1 2 3; do
    id="$(psql -XAt -c "SELECT $IDFN")"
    psql -X -v id="$id" <<'SQL'
BEGIN;
EXPLAIN (ANALYZE, BUFFERS, WAL, VERBOSE, SETTINGS, COSTS OFF, TIMING OFF)
INSERT INTO autumn_outbox (id, aggregate, topic, payload, created_at, available_at)
VALUES (:'id', :'id', 'autumn.event', '{"type":"autumn.event"}', 1764000999999, 1764000999999);
EXPLAIN (ANALYZE, BUFFERS, WAL, VERBOSE, SETTINGS, COSTS OFF, TIMING OFF)
INSERT INTO autumn_inbox (consumer, message_id, seen_at) VALUES ('listeners', :'id', 1764000999999) ON CONFLICT DO NOTHING;
ROLLBACK;
SQL
  done
done
