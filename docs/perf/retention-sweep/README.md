# Retention sweep on Postgres: name each batch's rows by `ctid`

This is a Ledger run on the framework retention sweep: `sweep_postgres` in
`autumn/src/data_retention.rs`, which the scheduled `autumn-retention-sweep`
task and `autumn retention run` both reach through `run_retention`.

A pass deletes stale rows in batches of 500, at most 1,000 batches:

```sql
DELETE FROM <table> WHERE <predicate>
   AND <key> IN (SELECT <key> FROM <table> WHERE <predicate> LIMIT 500)
```

The sub-select finds 500 rows. The delete then found each of them again
through the primary key (`Index Scan using <table>_pkey`, `loops=500`), about
four buffers per row, before it deleted the row. Those primary-key probes were
more than half of what the whole pass read.

The batch now passes the sub-select's `ctid`s to the delete:

```sql
DELETE FROM <table> WHERE <predicate>
   AND ctid = ANY(ARRAY(SELECT ctid FROM <table> WHERE <predicate> LIMIT 500))
```

The delete reads each row's heap page directly (`Tid Scan`). It is still one
statement per batch, with the same predicate on both sides.

## Workload and fixture

`autumn/tests/integration/retention_sweep_profile.rs` builds the four
sweep-enforced tables from the shipped migrations, fills them, and calls
`run_retention` once per dataset, in the scheduled task's order. Each pass
starts after a `CHECKPOINT` and a `pg_stat_statements` reset.

`J` is `AUTUMN_LEDGER_RETENTION_JOBS`: 300,000, 1,000,000 and 3,000,000.

| dataset | table | rows | shape | window |
|:--|:--|--:|:--|--:|
| `job_history` | `autumn_jobs` | `J` | 37 days of history. 95.9% completed, 3% failed, 1% discarded, the newest 0.1% enqueued or running. 5% TTL-unique (kept). | 30d |
| `commit_hooks` | `autumn_repository_commit_hooks` | `J/4` | 37 days. 90% completed, 4% failed, 2% `after_hook_failed`, 4% in flight (kept). | 30d |
| `job_tracking` | `autumn_job_tracking` | `J/4` | 9 days of updates | 7d |
| `experiment_assignments` | `autumn_experiment_assignments` | `J` | 400 days over 40 experiments. 45% belong to running, concluded or draft experiments (kept however old). One experiment is deleted. | 90d |

Every table is filled oldest first, as the app writes it. 4% of each table's
rows are updated with no `VACUUM`, so dead tuples are on the pages. No row is
within an hour of a cutoff, so the set a pass deletes does not depend on how
long the fill took. At 3M, `autumn_jobs` is 700 MB of heap and 444 MB of
indexes.

The 3M fixture is larger than one pass can clear, so `job_history` and
`experiment_assignments` stop at the 1,000-batch cap and report
`truncated=true`. That is the first sweep of a table that was never swept.

Postgres 16.15, `shared_buffers = 512MB`, `pg_stat_statements.track = all`,
autovacuum off, one client.

## Profile (baseline)

From `pg_stat_statements`. The full output is in `baseline-*.txt`. A pass
issues one `SELECT NOW() - …` for the cutoff, two `COUNT(*)`s, and the
batches.

| size | dataset | batches | `DELETE` buffers | share of buffers | share of calls |
|:--|:--|--:|--:|--:|--:|
| 3M | `job_history` | 1,000 | 3,024,169 | **94.4%** | 99.5% |
| 3M | `commit_hooks` | 276 | 783,839 | 97.7% | 98.6% |
| 3M | `job_tracking` | 342 | 1,005,198 | 97.0% | 98.8% |
| 3M | `experiment_assignments` | 1,000 | 2,519,333 | 97.7% | 99.6% |
| 1M | `job_history` | 363 | 1,010,436 | 94.4% | 98.6% |

The rest is the two `COUNT(*)`s. No temp blocks anywhere.

## Plan

`explain.sh` writes `explain-3000000.txt`: one batch of each form, inside a
rolled-back transaction, on the 3M database after the baseline pass. For
`autumn_jobs`:

```
--- by key (before)
 Delete on public.autumn_jobs (actual rows=0 loops=1)
   Buffers: shared hit=2458 read=1999 dirtied=16
   ->  Nested Loop (actual rows=500 loops=1)
         ->  HashAggregate … ->  Limit … ->  Seq Scan on autumn_jobs autumn_jobs_1
         ->  Index Scan using autumn_jobs_pkey on public.autumn_jobs (actual rows=1 loops=500)
               Index Cond: (autumn_jobs.id = "ANY_subquery".id)

--- by ctid (after)
 Delete on public.autumn_jobs (actual rows=0 loops=1)
   Buffers: shared hit=1005 read=11 dirtied=15
   InitPlan 1 (returns $0)
     ->  Limit … ->  Seq Scan on autumn_jobs autumn_jobs_1
   ->  Tid Scan on public.autumn_jobs (actual rows=500 loops=1)
         TID Cond: (autumn_jobs.ctid = ANY ($0))
```

That batch reads 4,457 buffers before the change and 1,016 after. The
`experiment_assignments` batch reads 2,512 before and 1,012 after. The
sub-select is the same in both forms. Only the node that finds the row to
delete changes: the primary-key `Index Scan`, run 500 times, becomes one
`Tid Scan`.

## Measurement

Tool: `pg_stat_statements`, `shared_blks_hit + shared_blks_read`, and
`wal_bytes`. Each row is one `run_retention` pass over a fresh fixture. The
full output is in `after-*.txt`.

| size | dataset | batches | `DELETE` buffers before | after | Δ | pass total before | after | Δ |
|:--|:--|--:|--:|--:|--:|--:|--:|--:|
| 300k | `job_history` | 110 | 414,206 | 247,136 | −40.3% | 432,154 | 265,084 | −38.7% |
| 300k | `commit_hooks` | 29 | 48,730 | 31,701 | −34.9% | 50,656 | 33,627 | −33.6% |
| 300k | `job_tracking` | 36 | 98,890 | 41,056 | −58.5% | 102,087 | 44,253 | −56.7% |
| 300k | `experiment_assignments` | 257 | 948,665 | 555,696 | −41.4% | 954,635 | 561,666 | −41.2% |
| 1M | `job_history` | 363 | 1,010,436 | 453,496 | −55.1% | 1,070,228 | 513,288 | −52.0% |
| 1M | `commit_hooks` | 93 | 249,953 | 109,284 | −56.3% | 256,127 | 115,458 | −54.9% |
| 1M | `job_tracking` | 115 | 317,766 | 143,018 | −55.0% | 328,338 | 153,590 | −53.2% |
| 1M | `experiment_assignments` | 854 | 5,453,211 | 4,143,315 | −24.0% | 5,472,925 | 4,163,029 | −23.9% |
| 3M | `job_history` | 1,000 | 3,024,169 | 1,024,169 | **−66.1%** | 3,203,525 | 1,203,525 | **−62.4%** |
| 3M | `commit_hooks` | 276 | 783,839 | 361,925 | −53.8% | 802,307 | 380,393 | −52.6% |
| 3M | `job_tracking` | 342 | 1,005,198 | 481,048 | −52.1% | 1,036,552 | 512,418 | −50.6% |
| 3M | `experiment_assignments` | 1,000 | 2,519,333 | 1,019,333 | −59.5% | 2,577,925 | 1,077,925 | −58.2% |

At 3M the `job_history` delete reads exactly 2,000,000 fewer buffers: 2,000
per batch, four primary-key buffers for each of 500 rows.

Unchanged at every size, before and after: statement count, rows deleted,
`eligible`, `truncated`, and the `COUNT(*)` statements. Statement WAL is
unchanged too: 149,630,874 bytes for the 3M `job_history` delete in both
runs, and within 115 bytes on the other datasets. A delete writes the same
heap tuples either way, and a delete does not touch indexes. No temp blocks
in either run.

## Equivalence

After every pass the harness prints the count and the md5 of every surviving
key in the table, sorted (`string_agg(<key>, ',' ORDER BY <key>)`). All
twelve pairs match: for example `job_history` at 3M leaves 2,500,000 rows,
md5 `818d808643bcd9731ccaad32348a7366`, in both runs. The pass reports
(`eligible`, `removed`, `truncated`) match too.

The fixtures cover the rows the predicates must keep: TTL-unique finished
jobs, enqueued and running jobs whose `finished_at` is NULL, in-flight commit
hooks, assignments of running, concluded and draft experiments however old,
and assignments of a deleted experiment (swept). They also cover a pass that
stops at the batch cap with stale rows left.

Why the result cannot change:

* The predicate is unchanged, and it is still applied to the row the delete
  removes. Every deleted row matches it, whatever `ctid` names.
* The sub-select and the delete are one statement under one snapshot. A row
  the sub-select sees cannot be vacuumed while that snapshot is held, so its
  `ctid` cannot be reused by another row in between.
* Under `READ COMMITTED`, a row that a concurrent transaction updates between
  the sub-select and the delete has a new `ctid`. The batch skips it, where
  before it re-checked the new version and deleted it if it still matched.
  The next batch or run deletes it if it still matches. A row that stops
  matching survives in both forms (a dead letter replayed to `enqueued`).
  Finished rows are not normally updated, so this is the rare case. The
  closing `COUNT(*)` still reports any stale row that is left as
  `truncated`.

The existing `framework_retention_pg` suite (22 tests) passes unchanged.

## Write cost

No index, no migration, no lock change. The delete takes the same row locks
on the same rows.

## Not changed: the batch re-scan on a small table

At 1M, `experiment_assignments` read 6,385 buffers per batch, against 2,519 at
3M. `pg_stat_user_tables` shows why: 851 sequential scans read 152,535,821
tuples. The sub-select is a `Seq Scan … LIMIT 500`. The rows it deleted in
earlier batches are dead but still on the pages at the start of the heap, so
every batch reads the whole deleted prefix again. The cost of a pass grows
with the square of the batch count. At 3M (227 MB) and on `autumn_jobs` at 1M
(233 MB), the table is larger than a quarter of `shared_buffers`. There
Postgres starts each sequential scan where the last one stopped
(`synchronize_seqscans`), which hides the re-scan.

Bounding each batch by the last key it deleted would fix this, but it changes
how the loop walks the table, and it needs a key the batch can order by on
every dataset. It is left for a separate change. This PR's buffer counts
include the re-scan in both columns.

## Reproduce

```sh
# A Postgres 16 server that preloads pg_stat_statements:
#   -c shared_preload_libraries=pg_stat_statements -c pg_stat_statements.track=all
#   -c autovacuum=off -c shared_buffers=512MB
# or leave AUTUMN_LEDGER_PG_URL unset to start a testcontainer.
for n in 300000 1000000 3000000; do
  psql "$ADMIN_URL" -c 'DROP DATABASE IF EXISTS retention' -c 'CREATE DATABASE retention'
  AUTUMN_LEDGER_PG_URL=postgres://postgres@localhost/retention \
  AUTUMN_LEDGER_RETENTION_JOBS=$n \
    cargo test -p autumn-web --test integration_tests -- --ignored \
      retention_sweep_profile --nocapture --test-threads=1
done
docs/perf/retention-sweep/explain.sh postgres://postgres@localhost/retention
```

Check out the baseline commit (the parent of the `ctid` change) for the
before numbers.
