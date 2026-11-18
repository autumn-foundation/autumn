#!/usr/bin/env bash
# usage: explain.sh <database url>
# EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS) of the statements one
# ExperimentService::assign issues through PgExperimentStore, on a fixture the
# harness (autumn/tests/integration/experiments_assign_connection_profile.rs)
# has seeded. Each statement is explained twice: first as the opening
# statement of a new session, the way the store runs it (each store call
# opens its own connection), then again on the same session. The difference
# is the "Planning: Buffers" a cold catalog cache costs every call.
set -euo pipefail
url=${1:?database url}
pair=$(psql "$url" -XAtc "SELECT a.experiment || '|' || a.actor FROM autumn_experiment_assignments a JOIN autumn_experiments e ON e.name = a.experiment WHERE e.state = 'running' AND a.actor NOT LIKE 'qa:%' ORDER BY md5(a.experiment || a.actor) LIMIT 1")
exp=${pair%%|*}; actor=${pair#*|}
opts='ANALYZE, BUFFERS, VERBOSE, SETTINGS'
run() {
  local label=$1 sql=$2
  echo "=== $label: first statement of a new session"
  psql "$url" -X -q -c "SET statement_timeout = 5000" -c "EXPLAIN ($opts) $sql"
  echo "=== $label: same statement, second run on that session"
  psql "$url" -X -q -c "SET statement_timeout = 5000" -c "EXPLAIN ($opts) $sql" -c "EXPLAIN ($opts) $sql" | awk 'f; /^-+$/ && ++n == 2 {f = 1}' | sed '/^$/d'
  echo
}
echo "-- experiment=$exp actor=$actor"
run get_override "SELECT variant FROM autumn_experiment_overrides WHERE experiment = '$exp' AND actor = '$actor'"
run get_assignment "SELECT experiment, actor, variant, is_override, EXTRACT(EPOCH FROM assigned_at)::bigint AS assigned_at_secs FROM autumn_experiment_assignments WHERE experiment = '$exp' AND actor = '$actor'"
run has_assignment_in_group "SELECT EXISTS (SELECT 1 FROM autumn_experiment_assignments a JOIN autumn_experiments e ON e.name = a.experiment WHERE a.actor = 'user:new-0' AND e.exclusion_group = 'checkout' AND a.experiment <> 'exp_07') AS result"
