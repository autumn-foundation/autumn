#!/usr/bin/env bash
# Scheduler tick claim workload: R replicas claim the ticks of 30 tasks
# through the prune + claim that PostgresTickSchedulerCoordinator issues,
# against fixture.sql's steady-state table. Prints the pg_stat_statements
# profile, table and index counters, WAL and wait-event samples.
#
# Usage: run.sh [SECONDS] [REPLICAS]   (PG* env names the server; it must
# preload pg_stat_statements). SCHEDULER_RS picks the source to read the SQL
# from (default: the working tree).
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
secs="${1:-120}"
replicas="${2:-6}"
src="${SCHEDULER_RS:-$here/../../../autumn/src/scheduler.rs}"
work="$(mktemp -d)"
trap '[ -n "${KEEP:-}" ] || rm -rf "$work"' EXIT; echo "work=$work" >&2
python3 "$here/extract_sql.py" "$src" "$work" >/dev/null

# $1..$6 of the Rust binds -> the pgbench variables, cast to the bind types
# (Text x4, Double, BigInt): diesel sends typed parameters, pgbench untyped.
params() {
  sed -e 's/\$1/:prefix::text/; s/\$2/:task_name::text/; s/\$3/:tick_key::text/; s/\$4/:owner::text/; s/\$5/:hold::float8/; s/\$6/:akey::int8/' "$1" | tr -d '\n'
}
prune="$(tr -d '\n' < "$work/prune.sql")"
claim="$(params "$work/claim.sql")"
# A source with the prune of one key claims first (claim-split.bench).
template="$here/claim.bench"; prune_key=""
if [ -f "$work/prune_key.sql" ]; then
  template="$here/claim-split.bench"; prune_key="$(params "$work/prune_key.sql")"
fi
python3 - "$template" "$work/claim.bench" "$prune" "$claim" "$prune_key" <<'PY'
import sys
s = open(sys.argv[1]).read()
s = s.replace("@@PRUNE_KEY@@", sys.argv[5]).replace("@@PRUNE@@", sys.argv[3]).replace("@@CLAIM@@", sys.argv[4])
open(sys.argv[2], "w").write(s)
PY

psql -X -q -v ON_ERROR_STOP=1 -v sqldir="$work" -f "$here/fixture.sql"
psql -X -q -c "CHECKPOINT" -c "SELECT pg_stat_statements_reset()" -c "SELECT pg_stat_reset()" >/dev/null
lsn0="$(psql -X -At -c 'SELECT pg_current_wal_lsn()')"

# Wait-event sampler: every 20 ms, every backend running a workload statement.
( end=$((SECONDS + secs)); while [ $SECONDS -lt $end ]; do
    psql -X -At -c "SELECT coalesce(wait_event_type, 'CPU') || ':' || coalesce(wait_event, '-') FROM pg_stat_activity WHERE query LIKE '%autumn_scheduler_ticks%' AND state = 'active' AND pid <> pg_backend_pid()"
    sleep 0.02; done ) > "$work/waits.txt" 2>/dev/null &
sampler=$!

# Autovacuum, as the server would run it (the server's own is off, so the
# run does not depend on when it fires): every NAPTIME seconds (default 60,
# autovacuum_naptime), VACUUM when n_dead_tup passes 50 + 0.2 x n_live_tup and
# ANALYZE when the changes since the last one pass 50 + 0.1 x n_live_tup.
# NAPTIME=0 turns it off.
naptime="${NAPTIME:-60}"
( [ "$naptime" -gt 0 ] || exit 0
  end=$((SECONDS + secs)); while sleep "$naptime"; [ $SECONDS -lt $end ]; do
    psql -X -At -c "SELECT (n_dead_tup > 50 + 0.2 * n_live_tup)::int || ' ' || (n_mod_since_analyze > 50 + 0.1 * n_live_tup)::int FROM pg_stat_user_tables WHERE relname = 'autumn_scheduler_ticks'" | {
      read -r vac ana
      if [ "$vac" = 1 ]; then psql -X -q -c "VACUUM (ANALYZE) autumn_scheduler_ticks"; echo "t=$SECONDS vacuum analyze";
      elif [ "$ana" = 1 ]; then psql -X -q -c "ANALYZE autumn_scheduler_ticks"; echo "t=$SECONDS analyze"; fi
    }
  done ) > "$work/autovac.txt" 2>&1 &
autovac=$!

rate=$(python3 -c "print(round($replicas * (10 + 10/10 + 10/60), 2))")
pgbench -n -M prepared -c "$replicas" -j "$replicas" -R "$rate" -T "$secs" -f "$work/claim.bench" 2>&1 \
  | grep -vE "^(progress|pgbench \(|transaction type|scaling|query mode|number of clients|number of threads|maximum number|duration|initial conn)" || true
wait "$sampler" || true
kill "$autovac" 2>/dev/null || true
lsn1="$(psql -X -At -c 'SELECT pg_current_wal_lsn()')"

echo "replicas=$replicas seconds=$secs rate=$rate/s"
echo "wal_lsn_delta_bytes=$(psql -X -At -c "SELECT pg_wal_lsn_diff('$lsn1', '$lsn0')")"
psql -X -f "$here/report.sql"
echo "-- autovacuum emulator (naptime=${naptime}s)"
cat "$work/autovac.txt"
echo "-- wait-event samples (backend x sample) during the run"
sort "$work/waits.txt" | uniq -c | sort -rn
