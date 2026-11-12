#!/usr/bin/env bash
# EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS) of the shipped enqueue INSERT,
# prepared with the store's typed $n binds, on the database run.sh left
# behind. Each EXECUTE is the first of a fresh PREPARE, so it is a custom
# plan, as every diesel-async raw sql_query call is. Rolled back.
# Cases: a TTL key with no row in the window (the scan finds nothing), a TTL
# key held in the window, a running-window key.
# Usage: [JOB_RS=<file>] explain.sh
set -euo pipefail
# A seq scan of a table past shared_buffers/4 starts where the last one
# stopped; off, an early-exit scan reads the same blocks on every run.
export PGOPTIONS="${PGOPTIONS:-} -c synchronize_seqscans=off"
HERE="$(cd "$(dirname "$0")" && pwd)"
export PGDATABASE="${DB:-ledger_jobenqueue}"
WORK="$(mktemp -d)"
python3 "$HERE/extract_sql.py" "$WORK" ${JOB_RS:+"$JOB_RS"}
INSERT="$(cat "$WORK/insert.sql")"
TYPES='text, text, text, integer, bigint, text, text, bigint, text, integer, timestamptz, text, bigint'
run() {
  echo "=== $1"
  psql -X -q <<SQL
BEGIN;
PREPARE ins($TYPES) AS $INSERT;
EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS, COSTS OFF, TIMING OFF) EXECUTE ins($2);
ROLLBACK;
SQL
}
run "ttl, key with no row in the window (acct:19999)" \
  "'x1', 'report.build', '{}', 5, 250, 'acct:19999', 'ttl', 3600000, NULL, NULL, NULL, 'default', NULL"
run "ttl, key held in the window (acct:1)" \
  "'x2', 'report.build', '{}', 5, 250, 'acct:1', 'ttl', 3600000, NULL, NULL, NULL, 'default', NULL"
run "running window, key with no in-flight row (user:19999)" \
  "'x3', 'digest.send', '{}', 5, 250, 'user:19999', 'running', NULL, NULL, NULL, NULL, 'default', NULL"
rm -rf "$WORK"
