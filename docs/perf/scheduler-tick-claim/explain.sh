#!/usr/bin/env bash
# EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS) of every statement an attempt
# issues, on the table run.sh leaves (the end of a run, before a vacuum).
# Each runs in a transaction that rolls back, so the table is not changed.
# A losing claim is a key that holds a live row; a winning claim a new key.
# Usage: explain.sh   (after run.sh; SCHEDULER_RS as in run.sh)
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
src="${SCHEDULER_RS:-$here/../../../autumn/src/scheduler.rs}"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
python3 "$here/extract_sql.py" "$src" "$work" >/dev/null
live="$(psql -X -At -c "SELECT tick_key FROM autumn_scheduler_ticks WHERE task_name = 'task-1' AND expires_at > now() ORDER BY tick_key DESC LIMIT 1")"
bind() {  # $1 = file, $2 = tick key
  sed -e "s/\\\$1/'autumn:scheduler'::text/; s/\\\$2/'task-1'::text/; s/\\\$3/'$2'::text/; s/\\\$4/'replica-0'::text/; s/\\\$5/301::float8/; s/\\\$6/42::int8/" "$1" | tr -d '\n'
}
explain() {  # $1 = label, $2 = statement
  echo "== $1"
  echo "$2"
  psql -X -q -c "BEGIN" -c "EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS) $2" -c "ROLLBACK" | grep -v '^ROLLBACK$'
}
psql -X -At -c "SELECT 'rows=' || count(*) || ' expired=' || count(*) FILTER (WHERE expires_at < now()) FROM autumn_scheduler_ticks"
psql -X -At -c "SELECT 'n_live_tup=' || n_live_tup || ' n_dead_tup=' || n_dead_tup FROM pg_stat_user_tables WHERE relname = 'autumn_scheduler_ticks'"
explain "prune (table-wide)" "$(tr -d '\n' < "$work/prune.sql")"
[ -f "$work/prune_key.sql" ] && explain "prune of one key (live row)" "$(bind "$work/prune_key.sql" "$live")"
explain "claim, losing (key holds a live row)" "$(bind "$work/claim.sql" "$live")"
explain "claim, winning (new key)" "$(bind "$work/claim.sql" "task-1:explain")"
