# Retention sweep: read each batch in index order

Harness: `extract_sql.py` prints the Postgres SQL that `sweep_postgres` in
`autumn/src/data_retention.rs` issues, read from the source (or from a file
given as its argument). `fixture.sql`, `run.sh <rows> [window_days]`,
`explain.sh <rows> [skip]`, `equivalence.sh <before-rev> <after-rev> [rows]`,
`catalog_record.py <dir>`.
Artifacts: `baseline-<rows>.txt` and `explain-before-1000000.txt` (the
baseline commit); `after-<rows>.txt`, `explain-after-1000000.txt`,
`equivalence.txt`; `trial-assignments-ordered-*.txt` (the negative result
below).

Reproduce: Postgres 16 with `pg_stat_statements` preloaded,
`shared_buffers = 2GB`, autovacuum off, the `autumn/migrations` set applied to
the database, PG* env set. `run.sh 1000000` on the baseline commit and on the
change, then `explain.sh 1000000` on each, then
`equivalence.sh <baseline> <change> 1000000`.

`shared_buffers` matters. Postgres starts a sequential scan where the last one
on the table stopped (`synchronize_seqscans`) only when the table is larger
than a quarter of `shared_buffers`. With the 128 MB default, a 1M-row fixture
crosses that line and the problem below hides. A production server sized at
25% of RAM keeps a 1 GB table under the line from 16 GB of RAM up.

## Workload

`autumn db retention` and the framework retention task: one pass per
sweepable Postgres dataset. A pass resolves the cutoff on the database,
counts the stale rows, deletes them in batches of 500 (each batch its own
autocommit statement) until a batch deletes nothing or 1,000 batches ran, and
counts again. `run.sh` issues those statements in that order, as custom plans
(diesel-async prepares a raw `sql_query` as an unnamed statement on every
call).

Fixture, per table, at 100,000, 400,000 and 1,000,000 rows: 90 days of
history written in time order, so the oldest rows are at the front of the
heap, denser toward now (traffic grew 3x over the window). Ids are random
UUIDs. A 30-day window makes about 44% of each table stale. 5% of rows take
an `UPDATE` and `ANALYZE` runs without `VACUUM`, so there are dead tuples.

* `autumn_jobs`: 90% completed, 7% failed with a `last_error`, 3% discarded.
  5% carry a `unique_key`, one in five of those with the `ttl` window that the
  sweep keeps. 0.5% in flight. 48,742 heap blocks at 1M.
* `autumn_job_tracking`: a ~300 byte JSON record each. 52,517 blocks at 1M.
* `autumn_repository_commit_hooks`: 94% completed, 3% failed, 3%
  `after_hook_failed`, handler keys skewed 70/15/10/5. 24,160 blocks at 1M.
* `autumn_experiment_assignments`: 40 experiments, 30 archived (swept) and 10
  running, concluded or draft (kept), assignments skewed toward the
  low-numbered ones. 9,814 blocks at 1M.

## Profile

At 1M rows the batch `DELETE` is 99% of each pass's buffers (`autumn_jobs`:
13,055,441 of 13,152,931; the two counts are the rest). Buffers per batch grow
with the stale set at every size, which is the problem:

| rows | `autumn_jobs` | `autumn_job_tracking` | `autumn_repository_commit_hooks` |
|--:|--:|--:|--:|
| 100,000 | 3,800 | 3,901 | 2,685 |
| 400,000 | 7,457 | 7,887 | 5,002 |
| 1,000,000 | 14,802 | 16,253 | 8,627 |

## Plan

Before, batch 401 of the 1M `autumn_jobs` pass (`explain-before-1000000.txt`):

```
Delete on autumn_jobs                               Buffers: shared hit=12382
  ->  Nested Loop
        ->  HashAggregate
              ->  Subquery Scan on "ANY_subquery"
                    ->  Limit
                          ->  Seq Scan on autumn_jobs autumn_jobs_1   (actual rows=500)
                                Rows Removed by Filter: 2013          Buffers: shared hit=9882
        ->  Index Scan using autumn_jobs_pkey on autumn_jobs  (loops=500)  Buffers: shared hit=2000
```

After (`explain-after-1000000.txt`):

```
Delete on autumn_jobs                               Buffers: shared hit=7560
  ->  ...
                          ->  Index Scan Backward using idx_autumn_jobs_status_finished
                                on autumn_jobs autumn_jobs_1          (actual rows=500)
                                                                      Buffers: shared hit=5016
```

`autumn_job_tracking` batch 401: 13,559 buffers, of them 10,559 in a
`Seq Scan`, before; 3,734, of them 686 in an `Index Scan using
idx_autumn_job_tracking_updated_at`, after.

## Mechanism

The batch is
`DELETE … WHERE <predicate> AND id IN (SELECT id … WHERE <predicate> LIMIT 500)`.
The sub-select has no `ORDER BY`, so the planner reads it as a sequential
scan from block 0: it expects to find 500 matches in the first few pages, and
on a fresh table it would. But the stale rows are the old rows, at the front
of the heap, and the rows the earlier batches of the pass deleted are still
there as dead tuples: nothing vacuums between batches. Batch `k` reads every
page that batches `1..k-1` emptied before it finds a live stale row, so a pass
over `S` stale rows reads about `S²/1000/rows_per_page` pages. At 1M rows the
sub-selects of the `autumn_jobs` pass read 29,055,229 tuples
(`pg_stat_user_tables.seq_tup_read`) to delete 440,134.

Each of the three tables already has an index that serves the predicate,
added for this sweep in `20260831191622_retention_sweep_indexes` (and
`idx_autumn_jobs_status_finished` for jobs). Ordering the sub-select by that
index makes the planner walk it. A batch then steps over index entries of the
rows it deleted, not heap pages, and the first scan that passes a dead entry
marks it, so the next skips it without a heap fetch.

## Change

`sweep_order(dataset)` in `autumn/src/data_retention.rs`, appended to the
sub-select as ` ORDER BY <order>`:

* `autumn_jobs`: `status DESC, finished_at`, `idx_autumn_jobs_status_finished
  (status, finished_at DESC)` read backward.
* `autumn_repository_commit_hooks`: `finished_at`, the partial
  `idx_autumn_commit_hooks_finished`.
* `autumn_job_tracking`: `updated_at`, `idx_autumn_job_tracking_updated_at`.
* `autumn_experiment_assignments`: none. See the negative result.

Oldest first in each, so a pass that stops at the 1,000-batch cap leaves the
youngest stale rows for the next tick. Before, it left whatever the heap order
left. No migration, no new index, no schema change.

## Measurement

Batch `DELETE`, one full pass, `pg_stat_statements` (`shared_blks_hit +
shared_blks_read`). `temp_blks_written` is 0 in every run, before and after.
Statement count is unchanged: the same number of batches at every size.

| rows | table | buffers before | buffers after | delta |
|--:|:--|--:|--:|--:|
| 100,000 | `autumn_jobs` | 342,020 | 294,011 | -14.0% |
| 100,000 | `autumn_job_tracking` | 351,092 | 245,288 | -30.1% |
| 100,000 | `autumn_repository_commit_hooks` | 244,293 | 244,293 | 0 (already an index scan) |
| 400,000 | `autumn_jobs` | 2,632,272 | 1,731,273 | -34.2% |
| 400,000 | `autumn_job_tracking` | 2,823,443 | 1,050,454 | -62.8% |
| 400,000 | `autumn_repository_commit_hooks` | 1,790,602 | 1,041,606 | -41.8% |
| 1,000,000 | `autumn_jobs` | 13,055,441 | 7,129,421 | -45.4% |
| 1,000,000 | `autumn_job_tracking` | 14,481,722 | 3,388,344 | -76.6% |
| 1,000,000 | `autumn_repository_commit_hooks` | 7,695,544 | 2,927,345 | -62.0% |

Tuples read by sequential scans over the pass, `pg_stat_user_tables` (the two
counts are sequential scans in both columns):

| 1,000,000 rows | `seq_tup_read` before | after |
|:--|--:|--:|
| `autumn_jobs` | 29,055,229 | 1,569,866 |
| `autumn_job_tracking` | 26,838,308 | 1,555,312 |
| `autumn_repository_commit_hooks` | 26,838,674 | 1,554,818 |

The plan shape changes at every size where the planner chose the sequential
scan, and buffers per batch stop growing with the stale set on
`autumn_job_tracking` (2,725 → 2,934 → 3,803) and
`autumn_repository_commit_hooks` (2,685 → 2,910 → 3,282). On `autumn_jobs`
they still grow (3,267 → 4,905 → 8,083), more slowly than before (3,800 →
7,457 → 14,802): the index returns the `ttl` unique jobs, the filter keeps
them, and each later batch fetches them from the heap again.

The buffer totals are stable between runs (`autumn_jobs` at 1M: 7,129,431 and
7,129,421 in two runs). WAL bytes are not: they are mostly full-page images,
whose count depends on where the checkpoints fall. The rows a pass deletes are
the same, so no WAL claim is made either way.

### Negative result: experiment assignments

The first version ordered `autumn_experiment_assignments` by `assigned_at`
too (`trial-assignments-ordered-*.txt`). It is worse at every size: 134,439 →
166,123 at 100k, 1,093,079 → 1,478,571 at 400k, 3,856,916 → 6,203,842 at 1M
(+61%). The `NOT EXISTS` keeps every assignment of a live experiment. In
`assigned_at` order those rows come back from the index on every batch, and
each one is a heap fetch, where the sequential scan reads them a page at a
time. That dataset keeps the planner's order, and its statement text (and
Ledger fingerprint) is unchanged.

## Equivalence

`equivalence.sh 307f0404 74470c05 1000000` (`equivalence.txt`). For each
revision it loads the fixture, adds the edge cases, runs one full pass of
every table with that revision's statements at one fixed cutoff, and hashes
the surviving rows of each table sorted by primary key. The two revisions
leave identical tables. Edge cases: 50 rows of each table exactly at the
cutoff (kept: the predicate is `<`) and 50 one microsecond before it (swept),
a terminal job and hook with a NULL `finished_at` (kept), a `ttl` unique job
(kept) beside a `running` one (swept) and one with a NULL window (kept, see
below), an old enqueued job and an old `pending_after_hook` hook (kept), and
old assignments of a deleted experiment (swept) and of a draft one (kept).
The fixture's clock is pinned for the check, so both loads are the same rows;
assignments hash by `(experiment, actor)`, because `TRUNCATE` does not restart
their id sequence.

A finding on the way, unchanged by this PR: a job with a `unique_key` and a
NULL `unique_window` is never swept. `NOT (unique_key IS NOT NULL AND
unique_window = 'ttl')` is `NOT (true AND NULL)`, which is NULL, so the
predicate never matches it. Both revisions keep that row. `job.rs` writes
both columns from one uniqueness setting, so only a row written another way
(by hand, or by an app's own SQL) is affected.

The set of rows a full pass deletes cannot change: the predicate is the same,
and the loop runs until a batch deletes nothing. Only which rows go in which
batch changes. The outer `DELETE` still re-checks the predicate, so a row
that stops qualifying after the sub-select survives as before. Isolation,
transactions and locks are unchanged: each batch is still one autocommit
statement that locks the rows it deletes.

## Write cost

None. No index is added, and no migration runs.

## Ledger catalog

The three changed statements have new fingerprints. Their profiles
(`docs/perf/ledger/queries/4708b378a4d4a0b1.toml`, `e781da630199fafb.toml`,
`7c6b620cb30dff6f.toml`) come from `catalog_record.py` and
`scripts/perf/ledger_catalog.py update`. The profiles of the replaced
statements are removed.
