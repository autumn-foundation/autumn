# 🗃️ Ledger: single-queue job claim (buffers/claim ~700→21, 3342→22, 57093→22)

## 🎯 Workload

The Postgres job runtime's claim query (`pg_claim_sql` / `pg_claim_next_job` in
`autumn/src/job.rs`) — the `SELECT … FOR UPDATE SKIP LOCKED` a worker runs
every poll to grab the next ready job. This is the highest-frequency
statement any Autumn app issues against Postgres once it uses background
jobs at all: every worker process runs it on a fixed interval, continuously,
for the life of the process — unlike request-handler queries, which are
bounded by traffic.

**Fixture**: `autumn_jobs`, seeded deterministically (`setseed(0.4231)`) to
match a real background-job corpus's shape:

- Job names Zipfian (`send_email` 40%, `process_webhook` 20%, `sync_contact`
  15%, 11 more names down to 1% each — two of the low-frequency names carry a
  `concurrency_limit = 1`, exercising the claim query's concurrency-check
  subquery for real, not vacuously).
- Queues: `default` 85%, `high` 10%, `low` 5%.
- Status mix: `finished` 90%, `failed` ~7%, `running` ~3% (stale-recovery
  candidates), plus a `ready` (`enqueued`, `run_at <= now`) and `scheduled`
  (`enqueued`, `run_at` in the future) slice sized per run.
- `VACUUM ANALYZE` after load, matching a steady-state production table
  (autovacuum keeps the visibility map current; skipping this step
  understates the concurrency subquery's Index-Only-Scan efficiency and
  produces non-reproducible buffer counts — see `fixture/seed.sql`).

Three sizes, same shape, to demonstrate plan-shape scaling per the process
requirements:

| Size   | ready-in-`default` | total rows |
|--------|---------------------|------------|
| small  | ~4.4k               | 106k       |
| medium | ~44.6k               | 553k       |
| large  | ~444k               | 3.5M       |

**Reproduce**:
```
createdb ledger_bench
psql -d ledger_bench -c "CREATE EXTENSION pg_stat_statements;"
psql -d ledger_bench -f fixture/schema.sql
psql -d ledger_bench -v ready=50000 -v scheduled=2000 -v history=500000 -v running=0 \
     -f fixture/seed.sql
psql -d ledger_bench -f fixture/claim_before.sql   # baseline plan
psql -d ledger_bench -f fixture/claim_after.sql    # fixed plan
```
(`pg_stat_statements.track = 'all'` and `shared_preload_libraries =
'pg_stat_statements'` must be set for the profile step.)

## 📈 Profile

50 claims + 5 admin-dashboard page loads (`pg_enqueued_and_scheduled_pages`)
+ 50 enqueues, on the medium fixture, `pg_stat_statements` reset first
(`fixture/workload.sql`, full output in
`baseline/pg_stat_statements_profile.txt`):

| statement (leaf, `track=all`)         | calls | buffers   | % of workload buffers |
|----------------------------------------|-------|-----------|------------------------|
| **claim `UPDATE … FOR UPDATE SKIP LOCKED`** | **50** | **166,437** | **93.4%** |
| dashboard enqueued-page `SELECT`       | 5     | 9,535     | 5.4%   |
| dashboard counts `SELECT`              | 5     | 1,415     | 0.8%   |
| enqueue `INSERT`                       | 50    | 709       | 0.4%   |
| dashboard scheduled-page `SELECT`      | 5     | 110       | 0.06%  |

Target is 93.4% of buffers and 50/115 (43%) of calls — both far past the 5%
floor for "worth changing."

## 🧭 Plan

`EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS)`, single-queue worker
(`queue_order = ["default"]`, the default/common case — no `[jobs] queues`
priority config). Full output per size in `baseline/*_explain_before.txt` /
`after/*_explain_after.txt`.

**Before** (medium size, 44.6k ready-in-`default` rows):
```
Update on autumn_jobs  (actual time=395.682..395.690 rows=1 loops=1)
  Buffers: shared hit=3342
  InitPlan 2
    -> Limit (actual time=... rows=1 loops=1)
      -> LockRows  (actual rows=1 loops=1)
        -> Sort  (actual rows=1 loops=1)
             Sort Key: (array_position('{default}'::text[], candidate.queue)), candidate.run_at
             Sort Method: quicksort  Memory: 3511kB
          -> Bitmap Heap Scan on autumn_jobs candidate  (actual rows=41909 loops=1)
               Recheck Cond: (queue = ANY('{default}') AND run_at <= now() AND status = 'enqueued')
               -> Bitmap Index Scan on idx_autumn_jobs_queue_ready
```
The whole ready-in-queue backlog (41,909 rows at this size) is fetched,
sorted, and lock-attempted before `LIMIT 1` picks one row.

**After** (same fixture):
```
Update on autumn_jobs  (actual time=0.225..0.227 rows=1 loops=1)
  Buffers: shared hit=22
  InitPlan 2
    -> Limit (actual rows=1 loops=1)
      -> LockRows (actual rows=1 loops=1)
        -> Index Scan using idx_autumn_jobs_queue_ready on autumn_jobs candidate
             Index Cond: (queue = 'default' AND run_at <= now())
```
`Bitmap Heap Scan → Sort → LockRows(all)` becomes `Index Scan → Limit 1`:
the same, already-existing `idx_autumn_jobs_queue_ready (queue, run_at)`
index, used in its natural order instead of being fed into a `Sort`.

## 💡 Hypothesis

`pg_claim_sql`'s `ORDER BY array_position($2::text[], candidate.queue),
candidate.run_at` cannot be served by index order on `(queue, run_at)`:
`array_position` is opaque to the planner at plan time (its result depends
on the bound `$2` array), so even though it evaluates to the same constant
for every row once `queue = ANY($2)` has restricted the candidate set to a
single queue value, the planner has no way to prove that at plan time and
falls back to materializing and sorting the *entire* ready backlog for the
queue before `LIMIT 1` can pick one row. `[jobs] queues` priority config is
opt-in (`QueueSchedule::from_config` defaults to a single `"default"` queue,
`autumn/src/job.rs:193`), so this is the *common*, not edge, case.

## 🔧 Change

`autumn/src/job.rs`: `pg_claim_next_job` now branches on `queue_order.len()
== 1`. The single-queue branch calls a new `pg_claim_sql_single_queue()`
that drops `array_position(...)` from `ORDER BY` (down to `candidate.run_at
ASC` alone) and binds `queue = $2` as a scalar instead of `queue =
ANY($2)`. The multi-queue branch (`queue_order.len() >= 2`, i.e. `[jobs]
queues` configured) is untouched — same SQL text, same bind types, same
`pg_claim_drains_higher_priority_queue_first` test path.

No migration, no new index, no lock beyond what the existing `UPDATE …
WHERE id = (SELECT … FOR UPDATE SKIP LOCKED)` already took.

## 📊 Measurement

Single claim, `EXPLAIN (ANALYZE, BUFFERS)`, `shared_blks_hit +
shared_blks_read`:

| size   | ready-in-queue | before (buffers) | after (buffers) | before temp spill | after temp spill |
|--------|-----------------|-------------------|------------------|--------------------|--------------------|
| small  | 4,451           | 703               | 21               | none                | none               |
| medium | 44,562           | 3,342             | 22               | none                | none               |
| large  | 444,269          | 57,093            | 22               | 486 read / 2045 written (external merge, 16MB disk) | none |

Plan shape: `Bitmap Heap Scan → Sort → LockRows(all)` (before, all 3 sizes)
→ `Index Scan → LockRows(≤1)` (after, all 3 sizes) — buffers scale with
backlog size before, flat after.

Workload level (`pg_stat_statements`, 50 claims on the medium fixture,
`baseline/pg_stat_statements_profile.txt` vs
`after/pg_stat_statements_profile_after.txt`):

| | before | after | delta |
|---|---|---|---|
| claim statement, total buffers (50 calls) | 166,437 | 1,410 | **−99.15%** |
| claim statement, buffers/call | 3,329 | 28.2 | −99.15% |

Clears the impact floor on two independent criteria: ≥20% buffer reduction
(actual: 99.15% at workload level, 96–99.96% per-call across sizes) and a
plan-shape change demonstrated at 3 data sizes; the large-size run also
eliminates an external-merge sort spill to disk.

## ✅ Equivalence

Both query texts share the same `WHERE`/lock structure; only the `ORDER BY`
key and the queue predicate's scalar-vs-array form changed. Verified on the
real medium fixture (`fixture/equivalence.sql`-equivalent, run in this
session, both queries in rolled-back transactions so the fixture state was
never touched mid-comparison):

- **General case**: same fixture, same claim — both variants return the
  identical row (`id = r-50000`). Confirmed the minimum `run_at` among
  ready-in-`default` rows is unique (no ties) before treating "same id" as a
  strong check; `array_position` over a one-element array is constant for
  every row that passes the `queue = ANY($2)` filter, so the `ORDER BY`
  reduces to `run_at ASC` in both forms — this is not a coincidence of the
  fixture, it's the mechanism.
- **Concurrency-limit edge case**: crafted a fixture where the
  lowest-`run_at` ready row belongs to a name already at its
  `concurrency_limit` (a `running` row occupies the slot). Both variants
  correctly skip the blocked row and return the same next-eligible row
  (`edge-next`).
- **Tie semantics**: rows with identical `run_at` are not given a
  deterministic winner by either query (both rely on `FOR UPDATE SKIP
  LOCKED` picking *some* unlocked eligible row) — this is existing queue
  semantics ("any due job is a valid claim"), unchanged by this patch.
- Existing Rust-level correctness tests for the *multi*-queue path
  (`pg_claim_drains_higher_priority_queue_first`,
  `local_strict_priority_runs_critical_before_backlog_of_low`, and the
  `QueueCursor`/`QueueSchedule` unit tests) are untouched — this patch does
  not modify `pg_claim_sql`, `QueueCursor::next_order`, or
  `QueueSchedule::from_config`, only adds a new branch taken exclusively
  when `queue_order.len() == 1`.
- Isolation/locking unchanged: same `FOR UPDATE SKIP LOCKED`, same
  optional `pg_advisory_xact_lock` serialization path when
  `serialize_claims` is set.

Note: the environment's Docker daemon was available for this session
(unusual for this sandbox), so `pg_claim_drains_higher_priority_queue_first`
was attempted directly via `cargo test --ignored`; it fails on **this
branch's unmodified base commit** too (`pg_run_migration`'s naive
`sql.split(';')` migration loader hits a pre-existing "syntax error at or
near \"this\"" — a test-harness issue unrelated to `job.rs`'s query code,
confirmed by reproducing the identical failure with `git stash` applied,
i.e. on the base commit). Equivalence for this change is therefore
established via direct SQL comparison on the real schema/fixture above,
not via that particular ignored test.

## 💸 Write cost

No new index — the existing `idx_autumn_jobs_queue_ready (queue, run_at)`
(added in `20260628000000_add_queue_to_jobs`) is unchanged and is what both
the before and after queries scan; this patch only changes which query text
is sent and how the planner is able to use that index. The `UPDATE`'s `SET`
list and the single row it touches are identical in both variants, so
per-claim WAL bytes are unaffected — the win is entirely in the read/lock
side of the query (rows scanned and locked before `LIMIT 1`), not the
write.

## 🔬 Reproduce

```bash
createdb ledger_bench
psql -d ledger_bench -c "CREATE EXTENSION pg_stat_statements;"
# shared_preload_libraries = 'pg_stat_statements' and
# pg_stat_statements.track = 'all' in postgresql.conf, then restart.

psql -d ledger_bench -f docs/reports/2026-08-14-ledger-job-claim-single-queue/fixture/schema.sql
psql -d ledger_bench -v ready=50000 -v scheduled=2000 -v history=500000 -v running=0 \
     -f docs/reports/2026-08-14-ledger-job-claim-single-queue/fixture/seed.sql

psql -d ledger_bench -f docs/reports/2026-08-14-ledger-job-claim-single-queue/fixture/claim_before.sql
psql -d ledger_bench -f docs/reports/2026-08-14-ledger-job-claim-single-queue/fixture/claim_after.sql

psql -d ledger_bench -c "SELECT pg_stat_statements_reset();"
psql -d ledger_bench -f docs/reports/2026-08-14-ledger-job-claim-single-queue/fixture/workload.sql
psql -d ledger_bench -f docs/reports/2026-08-14-ledger-job-claim-single-queue/fixture/profile.sql
```

Rust-side:
```bash
cargo fmt --all
cargo clippy -p autumn-web --all-targets --features db -- -D warnings
cargo test -p autumn-web --lib --features db
```
