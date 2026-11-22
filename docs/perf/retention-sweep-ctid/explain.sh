#!/usr/bin/env bash
# EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS) of one retention sweep batch
# per dataset, in the id-keyed form (before) and the ctid-keyed form (after),
# each inside a rolled-back transaction. Run it on the database the harness
# filled (autumn/tests/integration/retention_sweep_profile.rs).
# Usage: explain.sh <database-url> > explain.txt
set -euo pipefail
URL=${1:?database url}
JOBS="status IN ('completed', 'failed', 'discarded') AND finished_at IS NOT NULL AND finished_at < now() - interval '30 days' AND NOT (unique_key IS NOT NULL AND unique_window = 'ttl')"
HOOKS="status IN ('completed', 'failed', 'after_hook_failed') AND finished_at IS NOT NULL AND finished_at < now() - interval '30 days'"
TRACK="updated_at < now() - interval '7 days'"
EXP="assigned_at < now() - interval '90 days' AND NOT EXISTS (SELECT 1 FROM autumn_experiments e WHERE e.name = autumn_experiment_assignments.experiment AND e.state IN ('draft', 'running', 'concluded'))"
plan() {
  echo "--- $1"
  psql "$URL" -X -q -P pager=off -c BEGIN \
    -c "EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS) $2" -c ROLLBACK
}
for spec in "autumn_jobs|id|$JOBS" "autumn_repository_commit_hooks|id|$HOOKS" \
            "autumn_job_tracking|key|$TRACK" "autumn_experiment_assignments|id|$EXP"; do
  IFS='|' read -r table key pred <<<"$spec"
  plan "$table: by key (before)" \
    "DELETE FROM $table WHERE $pred AND $key IN (SELECT $key FROM $table WHERE $pred LIMIT 500)"
  plan "$table: by ctid (after)" \
    "DELETE FROM $table WHERE $pred AND ctid = ANY(ARRAY(SELECT ctid FROM $table WHERE $pred LIMIT 500))"
done
