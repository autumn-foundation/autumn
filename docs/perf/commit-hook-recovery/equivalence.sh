#!/usr/bin/env bash
# Result equivalence of the stale-recovery tick with and without the
# after-hook recovery index. One fixture (fixture.sql + equivalence_cases.sql)
# is cloned into two databases: `before` has the full framework migration set
# without 20261115143207, `after` has it. now() is frozen at the same instant
# in both: a public.now() with search_path = public, pg_catalog shadows
# pg_catalog.now(). Each database runs the shipped tick (both recovery
# UPDATEs, in worker order) once, then the full table is dumped sorted by id
# (the primary key) and compared. Usage: equivalence.sh [hist]. Needs PG* env.
set -euo pipefail
export PGOPTIONS="${PGOPTIONS:-} -c synchronize_seqscans=off"
HERE="$(cd "$(dirname "$0")" && pwd)"
HIST="${1:-100000}"
NEW=20261115143207_add_commit_hooks_after_hook_recovery_index
FROZEN='2026-11-15 12:00:00+00'
WORK="$(mktemp -d)"
python3 "$HERE/extract_sql.py" "$WORK/after"
SKIP_MIGRATIONS="$NEW" python3 "$HERE/extract_sql.py" "$WORK/before"

for side in before after; do
  db="ledger_hookeq_$side"
  dropdb --if-exists "$db"; createdb "$db"
  {
    echo "CREATE FUNCTION public.now() RETURNS timestamptz LANGUAGE sql IMMUTABLE AS \$\$ SELECT '$FROZEN'::timestamptz \$\$;"
    echo "SET search_path = public, pg_catalog;"
    cat "$WORK/$side/schema.sql"
    echo "\\set hist $HIST"
    cat "$HERE/fixture.sql" "$HERE/equivalence_cases.sql"
  } | psql -qX -v ON_ERROR_STOP=1 -d "$db" >/dev/null
done

for side in before after; do
  db="ledger_hookeq_$side"
  echo "=== $side"
  psql -XAt -d "$db" -c "SELECT indexname FROM pg_indexes WHERE tablename = 'autumn_repository_commit_hooks' ORDER BY 1"
  for stmt in recover_running recover_pending; do
    sql="$(tr -d ';' < "$WORK/$side/$stmt.sql")"
    printf '%s: ' "$stmt"
    psql -XAt -d "$db" -c "SET search_path = public, pg_catalog" \
      -c "EXPLAIN (COSTS OFF) $sql" | grep -E 'Scan' | sed 's/^ *//' | tr '\n' ' '
    psql -XAt -d "$db" -c "SET search_path = public, pg_catalog" -c "$sql" | tail -1
  done
  psql -XAt -d "$db" -c "COPY (SELECT * FROM autumn_repository_commit_hooks ORDER BY id) TO STDOUT" > "$WORK/$side.tsv"
  psql -XAt -d "$db" -c "SELECT 'rows: ' || count(*) FROM autumn_repository_commit_hooks"
  psql -XAt -d "$db" -c "SELECT 'edge rows by status after the tick: ' || string_agg(id || '=' || status, ', ' ORDER BY id) FROM autumn_repository_commit_hooks WHERE id LIKE 'edge-%-noerr-payload' AND id NOT LIKE 'edge-%-null-%'"
  echo "md5 of the full table, sorted by id: $(md5sum < "$WORK/$side.tsv" | cut -d' ' -f1)"
done
echo "=== diff"
if diff -q "$WORK/before.tsv" "$WORK/after.tsv" >/dev/null; then
  echo "identical: $(wc -l < "$WORK/after.tsv") rows, every column"
else
  diff "$WORK/before.tsv" "$WORK/after.tsv" | head -40; echo "DIFFERENT"; exit 1
fi
rm -rf "$WORK"
