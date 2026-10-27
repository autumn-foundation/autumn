#!/usr/bin/env bash
# Saturated-backlog check (Codex review on #3466): a NON-deferred name with a large due backlog
# whose concurrency group is full, plus an earlier eligible job of another name and a small
# parked backlog. Buffers of ONE claim for: the shipped single-queue claim (baseline), the first
# rewrite (claim_after_v1_regressed.sql, probes every name to exhaustion) and the final claim.
# Usage: saturated.sh  (PG env set; writes saturated_result.txt)
set -euo pipefail
cd "$(dirname "$0")"
for sat in 20000 100000 200000; do
  psql -q -f after_index.sql >/dev/null 2>&1 || true
  psql -q <<SQL >/dev/null
TRUNCATE autumn_jobs;
INSERT INTO autumn_jobs (id,name,status,run_at,queue,payload) SELECT 'ok'||g,'ok','enqueued',TIMESTAMPTZ '2026-01-01'+g*interval '1 second','default','{}' FROM generate_series(1,10) g;
INSERT INTO autumn_jobs (id,name,status,run_at,queue,payload,concurrency_limit) SELECT 'sat'||g,'sat','enqueued',TIMESTAMPTZ '2026-03-01'+g*interval '1 second','default','{}',1 FROM generate_series(1,$sat) g;
INSERT INTO autumn_jobs (id,name,status,run_at,queue,payload,claimed_at) VALUES ('r','sat','running',now(),'default','{}',now());
INSERT INTO autumn_jobs (id,name,status,run_at,queue,payload) SELECT 'pk'||g,'parked','enqueued',TIMESTAMPTZ '2025-12-01'+g*interval '1 second','default','{}' FROM generate_series(1,1000) g;
VACUUM ANALYZE autumn_jobs;
SQL
  for v in baseline after_v1_regressed after; do
    f=claim_$v.sql
    out=$( { echo "BEGIN;"; echo "EXPLAIN (ANALYZE, BUFFERS)"; sed '$ s/RETURNING id AS jid;/RETURNING id;/' $f; echo "ROLLBACK;"; } | psql -At)
    b=$(grep -m1 -E "^ +Buffers: shared" <<<"$out" | sed 's/^ *//')
    echo "sat_backlog=$sat variant=$v  $b"
  done
done | tee saturated_result.txt
