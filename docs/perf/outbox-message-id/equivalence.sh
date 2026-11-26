#!/usr/bin/env bash
# Result equivalence for the message id change: run the same workload with
# v4 and with v7 message ids on the same fixture, and compare everything the
# relay and the inbox did, with each id replaced by its row's seq. Also
# checks that every workload id is a distinct, well-formed UUID of the
# expected version, and that a v7 id's timestamp is the write time.
# Usage: equivalence.sh [messages] [history]. Same server needs as run.sh.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
MSGS="${1:-20000}"
HIST="${2:-100000}"
export NOW_MS="${NOW_MS:-1764000000000}"
for v in v4 v7; do
  DB="ledger_outbox_eq_$v" MSG_ID="$v" "$HERE/run.sh" "$MSGS" "$HIST" >/dev/null 2>&1
  export PGDATABASE="ledger_outbox_eq_$v"
  echo "=== message ids $v"
  # The relay's work, in seq order: what was sent, when, after how many
  # attempts, and whether the message is its own aggregate.
  psql -XAt -c "SELECT md5(string_agg(concat_ws('|', seq, aggregate = id, topic, payload, created_at,
                         available_at, attempts, dispatched_at, claim_token, locked_until, last_error, dead_at), E'\n' ORDER BY seq))
                  FROM autumn_outbox WHERE seq > $HIST" | sed 's/^/outbox rows digest:  /'
  psql -XAt -c "SELECT md5(string_agg(concat_ws('|', i.consumer, o.seq, i.seen_at), E'\n' ORDER BY o.seq))
                  FROM autumn_inbox i JOIN autumn_outbox o ON o.id = i.message_id WHERE o.seq > $HIST" | sed 's/^/inbox rows digest:   /'
  psql -XAt -c "SELECT count(*) FROM autumn_inbox i LEFT JOIN autumn_outbox o ON o.id = i.message_id WHERE o.seq IS NULL" \
    | sed 's/^/inbox rows with no outbox row: /'
  psql -X -c "SELECT count(*) AS ids, count(DISTINCT id) AS distinct_ids,
                     count(*) FILTER (WHERE id ~ '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[89ab][0-9a-f]{3}-[0-9a-f]{12}\$') AS rfc4122,
                     string_agg(DISTINCT substr(id, 15, 1), ',') AS versions,
                     bool_and(id = id::uuid::text) AS canonical_text,
                     count(*) FILTER (WHERE substr(id, 15, 1) = '7'
                                        AND ('x' || substr(replace(id, '-', ''), 1, 12))::bit(48)::bigint
                                            NOT BETWEEN extract(epoch FROM now() - interval '1 hour') * 1000
                                                    AND extract(epoch FROM now()) * 1000) AS v7_time_off
                FROM autumn_outbox WHERE seq > $HIST"
done
