#!/usr/bin/env bash
# usage: run.sh <variant: baseline|btree|brin> <rows>   (PGHOST/PGPORT/PGDATABASE from env)
# Seeds the fixture, then measures one nightly gc_applied sweep
# (DELETE ... WHERE applied_at < horizon, horizon = 30 days before "now"),
# the write cost of the variant's index on a 50k-row push-dedup ingest, and
# the result-equivalence fingerprint. Counters only: buffers, rows, WAL.
set -euo pipefail
variant=${1:-baseline}; n=${2:-1000000}
case $variant in
  baseline) ddl='' ;;
  btree) ddl='CREATE INDEX autumn_sync_applied_applied_at_idx ON autumn_sync_applied (applied_at);' ;;
  brin)  ddl='CREATE INDEX autumn_sync_applied_applied_at_idx ON autumn_sync_applied USING brin (applied_at);' ;;
esac
psql -X -q -v ON_ERROR_STOP=1 -v n="$n" -v "variant_ddl=$ddl" -f "$(dirname "$0")/fixture.sql" >/dev/null
psql -X -q -v ON_ERROR_STOP=1 -v variant="$variant" -v n="$n" -f "$(dirname "$0")/measure.sql"
