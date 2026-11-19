#!/usr/bin/env bash
# Apply and roll back 20261115143207 on the production-shaped fixture made
# without it. Runs up.sql in a transaction, lists the locks it holds on the
# queue table before COMMIT, then down.sql. Also re-runs up.sql on a database
# made from the standalone set, where the two-status index already exists, to
# show the guard skips it. Usage: migration.sh [hist]. Needs PG* env.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../../.." && pwd)"
HIST="${1:-1000000}"
NEW=20261115143207_add_commit_hooks_after_hook_recovery_index
M="$ROOT/autumn/migrations/$NEW"
WORK="$(mktemp -d)"
SKIP_MIGRATIONS="$NEW" python3 "$HERE/extract_sql.py" "$WORK/full"
MIGRATIONS_DIR=autumn/repository_commit_hook_migrations python3 "$HERE/extract_sql.py" "$WORK/standalone"
for set in full standalone; do
  db="ledger_hookmig_$set"
  dropdb --if-exists "$db"; createdb "$db"
  psql -qX -v ON_ERROR_STOP=1 -d "$db" -f "$WORK/$set/schema.sql" >/dev/null
  psql -qX -v ON_ERROR_STOP=1 -d "$db" -v hist="$HIST" -f "$HERE/fixture.sql" >/dev/null
done
export PGDATABASE=ledger_hookmig_full
echo "=== full set, hist=$HIST: $(psql -XAt -c "SELECT count(*) || ' rows, ' || pg_relation_size('autumn_repository_commit_hooks') / 8192 || ' heap blocks' FROM autumn_repository_commit_hooks")"
echo "--- up.sql"
{ echo '\timing on'; echo 'BEGIN;'; cat "$M/up.sql"; echo '\timing off';
  echo "SELECT mode, granted FROM pg_locks WHERE relation = 'autumn_repository_commit_hooks'::regclass AND pid = pg_backend_pid() ORDER BY mode;";
  echo 'COMMIT;'; } | psql -X -v ON_ERROR_STOP=1 | grep -v -E '^(BEGIN|COMMIT|DO)$'
psql -XAt -c "SELECT 'created: ' || indexname || ', ' || pg_relation_size(indexname::regclass) / 8192 || ' blocks' FROM pg_indexes WHERE indexname = 'idx_autumn_repository_commit_hooks_after_hook_recovery'"
echo "--- up.sql again (idempotent)"
psql -XAt -v ON_ERROR_STOP=1 -f "$M/up.sql"
psql -XAt -c "SELECT 'indexes on claimed_at: ' || count(*) FROM pg_indexes WHERE tablename = 'autumn_repository_commit_hooks' AND indexdef LIKE '%(claimed_at)%'"
echo "--- down.sql"
{ echo '\timing on'; cat "$M/down.sql"; } | psql -X -v ON_ERROR_STOP=1
psql -XAt -c "SELECT 'after rollback, after_hook_recovery present: ' || EXISTS (SELECT 1 FROM pg_indexes WHERE indexname = 'idx_autumn_repository_commit_hooks_after_hook_recovery')"
export PGDATABASE=ledger_hookmig_standalone
echo "=== standalone set (pending_recovery already covers both statuses)"
psql -XAt -v ON_ERROR_STOP=1 -f "$M/up.sql"
psql -XAt -c "SELECT 'claimed_at indexes: ' || string_agg(indexname, ', ' ORDER BY indexname) FROM pg_indexes WHERE tablename = 'autumn_repository_commit_hooks' AND indexdef LIKE '%(claimed_at)%'"
dropdb ledger_hookmig_full; dropdb ledger_hookmig_standalone
rm -rf "$WORK"
