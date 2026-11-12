#!/usr/bin/env bash
# The delivery workload with CLIENTS concurrent workers (default 8), as a
# job runner with that many delivery workers would run it, while a sampler
# reads pg_stat_activity every 10 ms. Prints the wait events of the workers
# and the run's profile. Wait samples depend on scheduling: they corroborate
# the lock probe, they are not a gate.
# Usage: contention.sh [N] (default 40000).
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
N="${1:-40000}"
export CLIENTS="${CLIENTS:-8}" DB="${DB:-ledger_webhook}"
psql -X -q -d postgres -c "CREATE UNLOGGED TABLE IF NOT EXISTS ledger_wait_samples (wait_event_type text, wait_event text)" \
    -c "TRUNCATE ledger_wait_samples"
(
  sleep 25  # past the fixture build
  psql -X -q -d postgres -c "DO \$\$ BEGIN FOR i IN 1..1500 LOOP PERFORM pg_stat_clear_snapshot(); INSERT INTO ledger_wait_samples SELECT coalesce(wait_event_type, 'CPU'), coalesce(wait_event, '-') FROM pg_stat_activity WHERE datname = '$DB' AND state = 'active' AND application_name = 'pgbench'; PERFORM pg_sleep(0.01); END LOOP; END \$\$"
) &
"$HERE/run.sh" "$N" | sed -n '/== totals/,/== top statements by calls/p'
wait
echo "== wait-event samples of the pgbench workers"
psql -X -d postgres -c "SELECT wait_event_type, wait_event, count(*) AS samples, round(100.0 * count(*) / sum(count(*)) OVER (), 1) AS pct FROM ledger_wait_samples GROUP BY 1, 2 ORDER BY samples DESC, 1, 2"
