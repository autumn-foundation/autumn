#!/bin/sh
# usage: run.sh before|after OUTDIR   (needs PGHOST/PGDATABASE; pg_stat_statements preloaded, track=all)
# For each of three sizes: seed, VACUUM ANALYZE (in seed.sql), EXPLAIN one multi-queue claim.
# Then on the medium fixture: reset pg_stat_statements, run 50 claims, dump the profile.
set -e
V=$1; OUT=$2; F=$(dirname "$0")
Q="'{high,default,low}'"; D="'{}'"
mkdir -p "$OUT"
for spec in "small 100000 5000 500" "medium 500000 50000 2000" "large 3000000 500000 20000"; do
  set -- $spec; SZ=$1; HIST=$2; READY=$3; SCHED=$4
  psql -q -f "$F/schema.sql" >/dev/null 2>&1 || true
  psql -q -v history=$HIST -v ready=$READY -v scheduled=$SCHED -v running=0 -f "$F/seed.sql" > "$OUT/${SZ}_seed_summary.txt" 2>&1
  psql -v worker="'w'" -v queues="$Q" -v deferred="$D" -f "$F/explain_$V.sql" > "$OUT/${SZ}_explain_$V.txt" 2>&1
done
# medium profile
psql -q -v history=500000 -v ready=50000 -v scheduled=2000 -v running=0 -f "$F/seed.sql" > /dev/null 2>&1
psql -q -c "SELECT pg_stat_statements_reset()" >/dev/null
psql -q -v worker="'worker-bench'" -v queues="$Q" -v deferred="$D" -f "$F/workload_$V.sql"
psql -f "$F/profile.sql" > "$OUT/pg_stat_statements_profile_$V.txt"
