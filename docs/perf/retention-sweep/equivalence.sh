#!/usr/bin/env bash
# Result equivalence of the retention sweep before and after a change.
#
#   equivalence.sh <before-rev> <after-rev> [rows]     (PG* env set)
#
# For each revision: load the fixture, add the edge-case rows, run one full
# pass of every sweepable table with that revision's statements (extract_sql.py
# on `git show <rev>:autumn/src/data_retention.rs`) at one fixed cutoff, and
# hash the surviving rows sorted by primary key. The two revisions must leave
# byte-identical tables. Edge cases, all at the cutoff instant itself or on a
# NULL: rows exactly at the cutoff (kept: the predicate is `<`), one microsecond
# before it (swept), NULL finished_at on a terminal status (kept), a 'ttl'
# unique job (kept) next to a 'running'-window one (swept), one with a NULL
# window (kept: NOT (true AND NULL) is NULL, so the predicate never matches it), a live
# enqueued job past the cutoff (kept), and assignments of a deleted experiment
# (swept) and of a draft one (kept).
set -euo pipefail
before=${1:?before rev}; after=${2:?after rev}; n=${3:-100000}
here=$(cd "$(dirname "$0")" && pwd)
root=$(git -C "$here" rev-parse --show-toplevel)
export PGDATABASE=${PGDATABASE:-ledger}
export PGOPTIONS="${PGOPTIONS:-} -c client_min_messages=warning"
CUTOFF='2026-09-01 00:00:00+00'
tmp=$(mktemp -d)
# The fixture dates rows back from now(). Pin that to one instant, so both
# revisions sweep the same table against the fixed cutoff.
sed "s/now()/timestamptz '2026-11-18 00:00:00+00'/g" "$here/fixture.sql" > "$tmp/fixture.sql"

edge_cases() {
  psql -q -X -v ON_ERROR_STOP=1 <<SQL
-- The fixture's own time is now(); move a slice of it onto the fixed cutoff.
UPDATE autumn_jobs SET finished_at = '$CUTOFF' WHERE id IN (SELECT id FROM autumn_jobs WHERE finished_at IS NOT NULL ORDER BY id LIMIT 50);
UPDATE autumn_jobs SET finished_at = timestamptz '$CUTOFF' - interval '1 microsecond' WHERE id IN (SELECT id FROM autumn_jobs WHERE finished_at IS NOT NULL ORDER BY id OFFSET 50 LIMIT 50);
INSERT INTO autumn_jobs (id, name, payload, status, finished_at, unique_key, unique_window) VALUES
  ('edge-null-finished', 'e', '{}', 'completed', NULL, NULL, NULL),
  ('edge-ttl-unique', 'e', '{}', 'completed', '2026-01-01', 'u1', 'ttl'),
  ('edge-running-unique', 'e', '{}', 'failed', '2026-01-01', 'u2', 'running'),
  ('edge-null-window', 'e', '{}', 'discarded', '2026-01-01', 'u3', NULL),
  ('edge-live-old', 'e', '{}', 'enqueued', '2026-01-01', NULL, NULL);
UPDATE autumn_job_tracking SET updated_at = '$CUTOFF' WHERE key IN (SELECT key FROM autumn_job_tracking ORDER BY key LIMIT 50);
UPDATE autumn_job_tracking SET updated_at = timestamptz '$CUTOFF' - interval '1 microsecond' WHERE key IN (SELECT key FROM autumn_job_tracking ORDER BY key OFFSET 50 LIMIT 50);
UPDATE autumn_repository_commit_hooks SET finished_at = '$CUTOFF' WHERE id IN (SELECT id FROM autumn_repository_commit_hooks ORDER BY id LIMIT 50);
INSERT INTO autumn_repository_commit_hooks (id, handler_key, hook_name, status, finished_at) VALUES
  ('edge-null-finished', 'posts', 'h', 'completed', NULL),
  ('edge-pending-old', 'posts', 'h', 'pending_after_hook', '2026-01-01');
INSERT INTO autumn_experiment_assignments (experiment, actor, variant, assigned_at) VALUES
  ('exp_deleted', 'edge:1', 'control', '2026-01-01'),
  ('exp_39', 'edge:2', 'control', '2026-01-01'),
  ('exp_1', 'edge:3', 'control', '$CUTOFF');
ANALYZE;
SQL
}

for rev in "$before" "$after"; do
  git -C "$root" show "$rev:autumn/src/data_retention.rs" > "$tmp/src.rs"
  eval "$(python3 "$here/extract_sql.py" "$tmp/src.rs")"
  psql -q -X -v ON_ERROR_STOP=1 -v n="$n" -f "$tmp/fixture.sql" >/dev/null
  edge_cases
  for v in JOBS TRACK HOOKS EXP; do
    t=${v}_TABLE; b=${v}_BATCH
    {
      echo "\\set QUIET on"; echo "\\set ON_ERROR_STOP on"
      echo "SET plan_cache_mode = force_custom_plan;"
      echo "PREPARE batch(timestamptz, bigint) AS ${!b};"
      for _ in $(seq 1 1000); do
        echo "EXECUTE batch('$CUTOFF', 500);"
        echo "SELECT :ROW_COUNT = 0 AS done \\gset"
        echo "\\if :done \\else"
      done
      for _ in $(seq 1 1000); do echo "\\endif"; done
    } | psql -X -At >/dev/null
  done
  psql -X -At > "$tmp/$rev.out" <<'SQL'
SELECT 'autumn_jobs', count(*), md5(string_agg(id || '|' || status || '|' || coalesce(finished_at::text, '-'), ',' ORDER BY id)) FROM autumn_jobs;
SELECT 'autumn_job_tracking', count(*), md5(string_agg(key || '|' || updated_at::text, ',' ORDER BY key)) FROM autumn_job_tracking;
SELECT 'autumn_repository_commit_hooks', count(*), md5(string_agg(id || '|' || status || '|' || coalesce(finished_at::text, '-'), ',' ORDER BY id)) FROM autumn_repository_commit_hooks;
SELECT 'autumn_experiment_assignments', count(*), md5(string_agg(experiment || '|' || actor || '|' || assigned_at::text, ',' ORDER BY experiment, actor)) FROM autumn_experiment_assignments;
SELECT 'edge kept', string_agg(id, ' ' ORDER BY id) FROM autumn_jobs WHERE id LIKE 'edge-%';
SELECT 'edge kept hooks', string_agg(id, ' ' ORDER BY id) FROM autumn_repository_commit_hooks WHERE id LIKE 'edge-%';
SELECT 'edge kept assignments', string_agg(actor, ' ' ORDER BY actor) FROM autumn_experiment_assignments WHERE actor LIKE 'edge:%';
SELECT 'at cutoff kept', (SELECT count(*) FROM autumn_jobs WHERE finished_at = '2026-09-01 00:00:00+00'), (SELECT count(*) FROM autumn_job_tracking WHERE updated_at = '2026-09-01 00:00:00+00'), (SELECT count(*) FROM autumn_repository_commit_hooks WHERE finished_at = '2026-09-01 00:00:00+00');
SELECT 'stale left', (SELECT count(*) FROM autumn_jobs WHERE finished_at < '2026-09-01 00:00:00+00' AND status IN ('completed','failed','discarded') AND NOT (unique_key IS NOT NULL AND unique_window = 'ttl')), (SELECT count(*) FROM autumn_job_tracking WHERE updated_at < '2026-09-01 00:00:00+00');
SQL
  echo "== $rev ($(git -C "$root" rev-parse --short "$rev"))"; cat "$tmp/$rev.out"
done
if diff "$tmp/$before.out" "$tmp/$after.out" >/dev/null; then
  echo "IDENTICAL"
else
  echo "DIFFERENT"; diff "$tmp/$before.out" "$tmp/$after.out"; exit 1
fi
