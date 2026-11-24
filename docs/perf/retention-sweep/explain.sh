#!/usr/bin/env bash
# EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS) of one sweep batch per table,
# mid-pass: load the fixture, run `skip` batches of the checked-out source's
# statement (400 by default, about 200,000 rows), then explain the next batch
# inside a transaction that rolls back.
#
#   explain.sh <rows> [skip]     (PG* env set)
set -euo pipefail
n=${1:?rows}; skip=${2:-400}
here=$(cd "$(dirname "$0")" && pwd)
export PGDATABASE=${PGDATABASE:-ledger}
export PGOPTIONS="${PGOPTIONS:-} -c client_min_messages=warning"
eval "$(python3 "$here/extract_sql.py")"
psql -q -X -v ON_ERROR_STOP=1 -v n="$n" -f "$here/fixture.sql" >/dev/null
for v in JOBS TRACK HOOKS EXP; do
  t=${v}_TABLE; b=${v}_BATCH
  echo "== ${!t}: batch $((skip + 1)) of a pass over $n rows"
  {
    echo "\\set QUIET on"; echo "\\set ON_ERROR_STOP on"
    echo "SET plan_cache_mode = force_custom_plan;"
    echo "SELECT NOW() - make_interval(secs => 2592000) AS cutoff \\gset"
    echo "PREPARE batch(timestamptz, bigint) AS ${!b};"
    for _ in $(seq 1 "$skip"); do echo "EXECUTE batch(:'cutoff', 500);"; done
    echo "BEGIN;"
    echo "\\unset QUIET"
    echo "EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS, TIMING OFF) EXECUTE batch(:'cutoff', 500);"
    echo "ROLLBACK;"
  } | psql -X -P pager=off | grep -v '^BEGIN$\|^ROLLBACK$'
done
