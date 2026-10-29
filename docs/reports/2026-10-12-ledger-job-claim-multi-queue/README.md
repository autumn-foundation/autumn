# 🗃️ Ledger: multi-queue job claim (buffers/claim 805→41, 5,333→44, 87,148→44; workload −99.2%)

## 🎯 Workload
`pg_claim_sql()` in `autumn/src/job.rs`: the claim a Postgres job worker runs every poll when `[jobs] queues` lists two or more queues. The single-queue path was fixed in `2026-08-14-ledger-job-claim-single-queue`; this branch was left untouched there.
Fixture: the same seeded `autumn_jobs` generator (`fixture/seed.sql`, copied from that report): Zipfian job names, queues `default` 85% / `high` 10% / `low` 5%, ~3% running rows, two names with `concurrency_limit`. Three sizes: ready 5k / 50k / 500k (history 100k / 500k / 3M), `VACUUM ANALYZE` after load. Driven as the exact claim text through `psql` (no Docker in this environment, so the Rust ignored tests did not run).

## 📈 Profile
`pg_stat_statements` (track=all), 50 claims, medium fixture, queue order `high,default,low`: the claim is 185,979 of 185,982 buffers (99.99%) and 50 of 54 calls. Files: `baseline/pg_stat_statements_profile_before.txt`.

## 🧭 Plan
Before (medium): `Bitmap Heap Scan` of all 49,000 ready rows across the three queues → `Sort (array_position(..), run_at)` → `LockRows` → `Limit 1`. Large size sorts 19 MB on disk (`external merge`). The 1.08M estimated cost also exceeds `jit_above_cost`, so every claim JITs (plan shows `JIT: Functions: 23`).
After: `Function Scan unnest` → `Nested Loop` → per queue `Index Scan idx_autumn_jobs_queue_ready` → `LockRows` → `Limit 1`; `Incremental Sort` of ≤3 rows; no JIT. Full plans in `baseline/` and `after/`.

## 💡 Hypothesis
`ORDER BY array_position($2, queue), run_at` cannot use `(queue, run_at)` index order because `array_position` is opaque to the planner, so the whole ready backlog of every polled queue is fetched, sorted and lock-attempted before `LIMIT 1`. Same mechanism as the single-queue fix. Probing each queue with its own `ORDER BY run_at LIMIT 1` lets the index serve each head; the outer `ORDER BY ord, run_at` restores the original order.

## 🔧 Change
`pg_claim_sql()` only: `unnest($2) WITH ORDINALITY` cross-joined to a `LATERAL` per-queue `ORDER BY run_at LIMIT 1 FOR UPDATE SKIP LOCKED`, outer `ORDER BY ord, run_at LIMIT 1`. Binds unchanged. No migration, no new index, no new lock level.
Trade-off: each probe locks its queue head, so the claim can briefly lock the head of a lower-priority queue it then discards (released at statement end). A concurrent claim just skips that row for that instant.
Follow-up (#3182): the outer sort is now `ORDER BY q.ord` only. Each probe returns one row at most, so the result is the same. `WITH ORDINALITY` output is sorted on `ord`, so Postgres does not sort and stops at the first queue that has a row. The claim now locks only the row it claims. `pg_multi_queue_claim_does_not_lock_lower_queue_heads` and `pg_multi_queue_claim_cost_stays_flat_as_the_backlog_grows` in `autumn/src/job.rs` guard the lock and the plan.

## 📊 Measurement
Single claim, `EXPLAIN (ANALYZE, BUFFERS)`, shared hit+read:

| size (ready) | before | after | temp before → after |
|---|---|---|---|
| small (5k) | 805 | 41 | 0 → 0 |
| medium (50k) | 5,333 | 44 | 0 → 0 |
| large (500k) | 87,148 | 44 | 2,409 written → 0 |

Workload (50 claims, medium, `pg_stat_statements`): 185,979 → 1,565 buffers (−99.2%). Statement count unchanged (1 per claim). WAL: the UPDATE and its SET list are unchanged. Wall-clock is not used as evidence.

## ✅ Equivalence
`fixture/equivalence.sql` compares old and new picks as `(queue, run_at)` (ties on `run_at` are existing "any due job" semantics) over 24 cases: 8 queue orders (3-queue permutations, 2-queue, duplicated queue, unknown queue, only-unknown queues, empty list) × 3 deferred sets (none, hottest name, 12 names), on a fixture with 1,000 ready concurrency-limited rows: 24/24 identical (6 return no row in both). Concurrency: pgbench, 8 clients × 250 claims: 2,000 claims, 2,000 distinct ids, 0 failures. Not run here: the Docker-gated Rust tests (`pg_claim_drains_higher_priority_queue_first` etc.); CI's Docker sweep runs them unchanged.

## 💸 Write cost
No index added. Per-claim WAL unchanged.

## 🔬 Reproduce
```
createdb ledger_bench && psql -d ledger_bench -c "CREATE EXTENSION pg_stat_statements"   # preload + track=all
export PGDATABASE=ledger_bench   # run.sh and psql both read it
sh docs/reports/2026-10-12-ledger-job-claim-multi-queue/fixture/run.sh before baseline   # at the baseline commit's query
sh docs/reports/2026-10-12-ledger-job-claim-multi-queue/fixture/run.sh after after
psql -f docs/reports/2026-10-12-ledger-job-claim-multi-queue/fixture/equivalence.sql
```
