# Commit-hook worker: index the stale after-hook recovery

Harness: `extract_sql.py` writes the Postgres SQL that
`autumn/src/repository_commit_hooks.rs` ships as pgbench scripts, and the
queue schema from the shipped migrations (`MIGRATIONS_DIR`, default the full
framework set `autumn/migrations`; `SKIP_MIGRATIONS` leaves migrations out).
`fixture.sql`, `report.sql`, `run.sh [T] [hist]`, `explain.sh`,
`equivalence.sh` (+ `equivalence_cases.sql`), `write_cost.sh`, `migration.sh`.
Artifacts: `baseline-<hist>.txt` and `explain-before-<hist>.txt` (the
baseline commit); `after-<hist>.txt`, `explain-after-<hist>.txt`,
`standalone-set-1000000.txt`, `equivalence.txt`, `write_cost.txt`,
`migration.txt`.

Reproduce: Postgres 16 with `pg_stat_statements` preloaded and autovacuum
off, PG* env set.
`SKIP_MIGRATIONS=20261115143207_add_commit_hooks_after_hook_recovery_index run.sh 2000 1000000`
is the baseline, `run.sh 2000 1000000` the after. Then `explain.sh` (on the
database `run.sh` left), `equivalence.sh`, `write_cost.sh`, `migration.sh`.

## Workload

The repository commit-hook worker (`start_repository_commit_hook_worker`).
Every 250 ms, on every replica, it runs `recover_stale_repository_commit_hooks`
(two `UPDATE`s) and then `drain_ready_repository_commit_hooks` (a claim
attempt, and per claimed hook an ack). Each `#[after_commit]` hook also writes
the row three times before the worker sees it: the pending insert in the
mutation's transaction, mark succeeded, finalize.

2,000 pgbench transactions, one client, seeded: 80% an idle tick (both
recovery `UPDATE`s, then the claim), 20% one hook end to end (pending insert,
mark succeeded, finalize, claim, ack). That is 1,594 ticks and 406 hooks, about
400 seconds of one replica's ticks with a hook a second.

Fixture: 30 days of finished history in insert order. Retention is "forever"
for commit hooks by default (`data_retention`), so history is the bulk of
the table. 94% completed (the ack empties `context`/`record`), 3% failed and
3% after_hook_failed, both with a `last_error`. Handler keys skew 70/15/10/5.
In flight: 8 running (2 claimed past the 60 s stale window), 6
pending_after_hook (2 stale), 4 after_hook_succeeded (1 stale), 20 enqueued
retries backed off an hour. At 1,000,000 rows the heap is 186 MB (23,789
blocks). The 10,000 and 100,000 sizes use the same shape.

## Mechanism

The stale after-hook recovery filters
`status IN ('pending_after_hook', 'after_hook_succeeded') AND claimed_at < NOW() - 60 s`.
There are two Postgres migration sets for the queue table, with the same
version `20260515000000`:

* `autumn/repository_commit_hook_migrations` (the standalone set) makes
  `idx_autumn_repository_commit_hooks_pending_recovery` with
  `WHERE status IN ('pending_after_hook', 'after_hook_succeeded')`.
* `autumn/migrations` (`FRAMEWORK_MIGRATIONS`) makes the same name with
  `WHERE status = 'pending_after_hook'`.

An app that registers `FRAMEWORK_MIGRATIONS` runs it first. Diesel then
records `20260515000000`, and the standalone set's copy never runs on that
database. The narrow index does not imply the two-status predicate, and no
other index has `claimed_at` for those rows. So every tick read the whole
table: 1,000,444 rows removed by the filter, 23,799 buffers at 1M rows. The
history only grows.

The change is the migration
`20261115143207_add_commit_hooks_after_hook_recovery_index`:
`(claimed_at) WHERE status IN ('pending_after_hook', 'after_hook_succeeded')`,
the predicate the standalone set already ships. It is made only when no
`claimed_at` index on the table covers `after_hook_succeeded`, so a database
that ran the standalone set first gets no duplicate. The SQL does not change.

## Profile (pg_stat_statements, 2,000 transactions, 1,000,000 history rows)

| statement | calls | share of calls | buffers before | share of buffers |
| --- | --- | --- | --- | --- |
| recover stale pending | 1,594 | 23.4% | 37,928,061 | 99.9% |
| claim | 2,000 | 29.4% | 9,112 | 0.02% |
| ack | 406 | 6.0% | 5,943 | 0.02% |
| finalize | 406 | 6.0% | 5,942 | 0.02% |
| mark after_hook_succeeded | 406 | 6.0% | 3,845 | 0.01% |
| pending insert | 406 | 6.0% | 3,676 | 0.01% |
| recover stale running | 1,594 | 23.4% | 1,827 | 0.005% |

No temp blocks in any run. The running recovery is already served by
`idx_autumn_repository_commit_hooks_stale_recovery` (1 buffer).

## Plan (1,000,000 rows, `explain-before-1000000.txt` / `explain-after-1000000.txt`)

Before:

```
Update on public.autumn_repository_commit_hooks (actual rows=0 loops=1)
  Buffers: shared hit=23799
  ->  Seq Scan on public.autumn_repository_commit_hooks (actual rows=0 loops=1)
        Filter: ((status = ANY ('{pending_after_hook,after_hook_succeeded}'::text[])) AND (claimed_at < (now() - '00:01:00'::interval)))
        Rows Removed by Filter: 1000444
        Buffers: shared hit=23799
```

After:

```
Update on public.autumn_repository_commit_hooks (actual rows=0 loops=1)
  Buffers: shared hit=1
  ->  Index Scan using idx_autumn_repository_commit_hooks_after_hook_recovery (actual rows=0 loops=1)
        Index Cond: (claimed_at < (now() - '00:01:00'::interval))
        Filter: (status = ANY ('{pending_after_hook,after_hook_succeeded}'::text[]))
        Buffers: shared hit=1
```

## Measurement

Same harness, fixture and session. Tool: `pg_stat_statements` (reset before
the workload), `pg_stat_user_tables`, `pg_stat_user_indexes`.

| history rows | recover stale pending, buffers (1,594 calls) | per tick | workload total buffers | `seq_scan` on the table |
| --- | --- | --- | --- | --- |
| 10,000 | 391,240 → 2,294 (−99.4%) | 245 → 1.4 | 417,038 → 29,245 (−93.0%) | 1,594 → 0 |
| 100,000 | 3,800,732 → 2,297 (−99.94%) | 2,384 → 1.4 | 3,830,528 → 33,637 (−99.1%) | 1,594 → 0 |
| 1,000,000 | 37,928,061 → 1,617 (−99.996%) | 23,794 → 1.0 | 37,958,406 → 31,811 (−99.92%) | 1,594 → 0 |

The before cost grows linearly with history (one heap per tick); the after
cost does not grow. `idx_scan` on the new index: 1,594 at 10k and 100k. At 1M
it is 2,406: the planner also takes this small partial index for mark
succeeded's `WHERE id = $3 AND ... AND status = 'pending_after_hook'`
(3,845 → 4,502 buffers over 406 calls).
`idx_autumn_repository_commit_hooks_pending_recovery` has `idx_scan` 0 before
and after.

`standalone-set-1000000.txt` runs the after workload on the standalone set
alone, whose `pending_recovery` index already has the two-status predicate:
the recovery reads 1,617 buffers there too.

The 1M baseline updated 10 rows in the recovery and the after run 3. A
baseline tick took about 0.1 s, so the run lasted long enough for the
fixture's fresh claims to pass the 60 s window. That is the wall clock, not
the plan. `equivalence.sh` freezes the clock to compare results.

## Equivalence (`equivalence.txt`)

The 100,000-row fixture plus `equivalence_cases.sql`: every status (enqueued,
running, pending_after_hook, after_hook_succeeded, after_hook_failed,
completed, failed) crossed with `claimed_at` NULL, 3 days old, 1 µs before the
cutoff, exactly at the cutoff, 1 µs after, and fresh; with and without a
`last_error`; with a real and an empty payload. It is cloned into a database
without the migration and one with it. `now()` is frozen at the same instant
in both (a `public.now()` ahead of `pg_catalog` on the `search_path`). Each
runs the shipped tick (running recovery, then pending recovery) once. Before:
the pending recovery is a seq scan and updates 19 rows. After: an index scan,
19 rows. The full table, every column, sorted by the primary key `id`, is
identical: 100,206 rows, md5 `c19138590247a6d785057d3911191dbb` on both sides.
The boundary is exact on both: a row claimed 1 µs before the cutoff is
recovered, and rows at and after it are not. The SQL and the transaction
boundaries do not change. `cargo test -p autumn-web --lib -- repository_commit_hooks::tests`
passes unchanged, plus the new test.

## Write cost (`write_cost.txt`, `migration.txt`)

5,000 hooks end to end, no idle ticks, 1M fixture:

| | before | after | delta |
| --- | --- | --- | --- |
| WAL bytes (LSN delta) | 19,947,520 | 20,604,656 | +657,136 (+3.3%), +131 B per hook |
| `pg_stat_statements.wal_bytes` | 18,759,002 | 19,419,884 | +660,882 (+3.5%) |
| pending insert WAL | 12,231,975 | 12,553,050 | +64 B per insert |
| mark succeeded WAL | 1,275,110 | 1,614,935 | +68 B per call |
| buffers, all hook statements | 325,972 | 345,599 | +3.9 per hook (+6.0%) |

Only the pending insert and mark succeeded add an entry: those are the two
writes that put a row into one of the two statuses. Claim, finalize and ack
are unchanged to the byte. The index holds rows that are mid-hook, a handful
at a time: 2 blocks on the 1M fixture. With autovacuum off it grows to 30
blocks of dead entries after 5,000 hooks; vacuum reclaims them.

Against that, one replica's idle ticks read 23,794 buffers each at 1M rows,
four times a second. One hook's index cost (3.9 buffers, 131 WAL bytes) is
less than one after-tick reads.

Migration on the 1M fixture: the build holds `ShareLock` on
`autumn_repository_commit_hooks` for 77 ms (one heap scan), so hook writes
wait for it. A second run is a no-op. On a database made from the
standalone set the guard makes nothing. `down.sql` drops the index (2.7 ms).

## Not changed

`idx_autumn_repository_commit_hooks_pending_recovery` (the narrow one, in the
full set) serves no statement in this workload: `idx_scan` 0, over 2,000
transactions here and 5,000 hooks in `write_cost.txt`. The new index covers
its rows too, so it is a candidate to drop. Dropping an index is left to a
person: this run saw a few minutes of synthetic load, not a production
observation window.
