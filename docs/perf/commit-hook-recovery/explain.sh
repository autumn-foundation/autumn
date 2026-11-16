#!/usr/bin/env bash
# EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS) of the two stale-recovery
# UPDATEs the worker issues every tick, each in a rolled-back transaction, on
# the database run.sh left. Usage: explain.sh. Needs PG* env.
set -euo pipefail
export PGOPTIONS="${PGOPTIONS:-} -c synchronize_seqscans=off"
HERE="$(cd "$(dirname "$0")" && pwd)"
export PGDATABASE="${DB:-ledger_hookrecovery}"
WORK="$(mktemp -d)"
python3 "$HERE/extract_sql.py" "$WORK"
psql -XAt -c "SELECT 'rows: ' || count(*) || ', heap blocks: ' || pg_relation_size('autumn_repository_commit_hooks') / 8192 FROM autumn_repository_commit_hooks"
psql -XAt -c "SELECT indexdef FROM pg_indexes WHERE tablename = 'autumn_repository_commit_hooks' ORDER BY indexname"
for stmt in recover_pending recover_running; do
  echo "=== $stmt"
  psql -X -c BEGIN -c "EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS) $(tr -d ';' < "$WORK/$stmt.sql")" -c ROLLBACK \
    | grep -v -E '^(BEGIN|ROLLBACK)$'
done
rm -rf "$WORK"
