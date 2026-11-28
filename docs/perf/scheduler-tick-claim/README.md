# Scheduler tick claim: prune once per tick, not once per replica

The Postgres tick coordinator (`PostgresTickSchedulerCoordinator` in
`autumn/src/scheduler.rs`, #3052) claims a fleet tick with one row in
`autumn_scheduler_ticks`. Every replica tries every tick. Before this change,
every attempt ran two statements:

1. the table-wide prune, `DELETE FROM autumn_scheduler_ticks WHERE expires_at < now()`;
2. the claim, `INSERT … SELECT … WHERE pg_try_advisory_xact_lock($6) ON CONFLICT DO NOTHING RETURNING generation`.

## Harness

| File | What it does |
| --- | --- |
| `extract_sql.py` | Reads the DDL, prune, prune-of-one-key, claim and free SQL from `scheduler.rs`. |
| `fixture.sql` | Steady-state tick table, described below. |
| `claim.bench`, `claim-split.bench` | One replica's attempt as pgbench runs it: the old order, and the claim-first order. `run.sh` picks the one that matches the source. |
| `run.sh [SECONDS] [REPLICAS]` | Loads the fixture, resets the stats and drives the attempts. Prints the `pg_stat_statements` profile, table and index counters, WAL, and wait events (`report.sql`). |
| `explain.sh` | `EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS)` of every statement, on the table a run leaves. |
| `equivalence.sh BASE` | Runs one attempt per case, as `BASE` issues it and as the working tree issues it. |

Fixture: 30 tasks with the default 300 s retention. Ten run every 1 s, ten
every 10 s and ten every 60 s. Each tick inside its hold (retention plus
period) holds a row: 3,380 live rows. The fixture also leaves the dead
versions of ~700 pruned rows. That is the autovacuum threshold for a table of
this size, so the run starts as dirty as the table gets between two vacuums.

Workload: R replicas (pgbench clients) for 300 s, at
R × (10/1 + 10/10 + 10/60) attempts per second. That is one attempt per
replica per tick on average. The task is drawn with weight 1/period, and the
tick key and advisory key are computed as `fixed_delay_tick_key` and
`advisory_lock_key` compute them. Server autovacuum is off so the counters
do not depend on when it fires. `run.sh` runs its own autovacuum with the
server defaults instead: every 60 s it checks the thresholds
(50 + 0.2 × live rows to vacuum, 50 + 0.1 × live rows to analyze) and runs
whatever is due. Postgres 16.15, `pg_stat_statements` preloaded.

## Profile (baseline, `baseline-r*.txt`)

| R | statements | prune share of buffers | prune share of calls | claim share of buffers |
| --- | --- | --- | --- | --- |
| 2 | 13,454 | 61.9% | 50.0% | 38.1% |
| 6 | 39,882 | 65.8% | 50.0% | 34.2% |
| 12 | 81,094 | 61.1% | 50.0% | 38.9% |

The two statements are the whole workload. No temp blocks. The prune is the
top statement by buffers at every R. The claim is 68–73% of WAL. That WAL
comes from the winning inserts and is inherent.

## Mechanism

The prune runs on every attempt, so R times per tick. Every tick inserts one
row, and the fleet prunes it R times. The planner runs the prune as a
**Bitmap Heap Scan** on `idx_autumn_scheduler_ticks_expires_at`
(`explain-before.txt`): it estimates 411–760 rows from a histogram that
`now()` keeps moving past, and the actual count is 0–1. A bitmap scan never
marks dead index entries (`kill_prior_tuple` works only for a plain index
scan). So each prune walks every entry pruned since the last vacuum and
visits its heap page. At R = 6, 19,972 prune scans read 1,715,849 index
entries (`idx_tup_read`) and deleted 3,380 rows.

## Change

The claim comes first:

* **The claim wins**: the winner runs the table-wide prune. That is once per
  tick. The claim stands even if the prune fails. The failure is logged, and
  the next won tick prunes again.
* **The claim loses**: the loser deletes this key's row only if it has
  expired (`PG_TICK_PRUNE_KEY_SQL`, a primary-key lookup). It claims again
  only if that delete removed a row. That is the case of a late replica past
  the retention, or a constant key such as the ACME mutex.

Each attempt still runs two statements. No migration, no index, no new lock.
The claim SQL is the same text as before.

## Measurement (`after-r*.txt`, same harness, same fixture, same session)

pg_stat_statements, buffers = `shared_blks_hit + shared_blks_read` (all hits
in every run; reads were 0). pgbench paces attempts by time, so the attempt
count differs a little between runs. Per attempt, workload buffers went from
16.27 to 12.77 (R = 2, −21.5%), 14.21 to 10.00 (R = 6, −29.6%) and 11.36 to
8.39 (R = 12, −26.1%).

| R | | before | after | delta |
| --- | --- | --- | --- | --- |
| 2 | workload buffers | 109,433 | 85,250 | **−22.1%** |
| | table-wide prune: calls / buffers | 6,727 / 67,709 | 2,870 / 25,994 | −57% / −61.6% |
| | statements per attempt | 2.00 | 2.00 | 0 |
| 6 | workload buffers | 283,373 | 200,169 | **−29.4%** |
| | table-wide prune: calls / buffers | 19,941 / 186,547 | 3,369 / 31,483 | −83% / −83.1% |
| | prune-of-key: calls / buffers | — | 16,645 / 70,705 | 4.25 per call |
| | claim buffers | 96,826 | 97,981 | +1.2% |
| | `idx_autumn_scheduler_ticks_expires_at` idx_tup_read | 1,715,849 | 243,988 | −85.8% |
| | statements per attempt | 2.00 | 2.00 | 0 |
| | WAL bytes (pg_stat_statements) | 2,021,820 | 2,022,197 | 0.0% |
| 12 | workload buffers | 460,736 | 336,457 | **−27.0%** |
| | table-wide prune: calls / buffers | 40,547 / 281,284 | 3,366 / 32,383 | −91.7% / −88.5% |
| | prune-of-key: calls / buffers | — | 36,713 / 125,076 | 3.41 per call |
| | statements per attempt | 2.00 | 2.00 | 0 |

Temp blocks: 0 in every run. WAL does not move. The pruned rows are the same
rows, deleted by fewer statements, and the prune-of-key almost never deletes
anything.

The prune-of-key costs about 4 buffers. That is a primary-key descent plus
the heap page of the live row it then rejects. The old order did not pay it.
It is still far below the ~9 buffers of each table-wide prune it replaces.

Wait events (`pg_stat_activity`, 20 ms samples) are corroboration only: they
depend on scheduling. Before, two concurrent prunes of the same expired rows
waited on each other: `Lock:tuple` + `Lock:transactionid` 8 samples at R = 6
and 11 at R = 12. After, 0 at both.

Runs are timed in real time, so the number of ticks won depends on when each
pgbench client draws each task. The fleet won 3,365 ticks before and 3,369 after
at R = 6, and 3,364 and 3,366 at R = 12. At R = 2 a tick gets no attempt
about 13% of the time (Poisson, mean 2), so its count varies run to run
(2,901 and 2,870). An after run at R = 12 made while a build starved the
machine (pgbench schedule lag 2.3 s) was thrown away and run again.

The `explain-*.txt` files show plan shapes only. Each is a single rollback
run on the table a run leaves, so its buffer counts depend on how long ago
the last vacuum ran. The counters above come from the whole run.

## Equivalence (`equivalence.txt`)

`equivalence.sh 1ac3175c` seeds one table per case: this app's live and
expired rows, another key prefix's expired row, and generations 1–5. It runs
one attempt as the base commit issues it and one as the change issues it. It
then compares the attempt's result (lost, or won with a generation above
every earlier one) and the full set of unexpired rows. All eight cases match:

* free key: won
* key holds a live row: lost
* key holds an expired row (late replica): won
* constant key with an expired row (ACME mutex): won
* free key with the advisory key held (pre-#3052 replica): lost
* expired row with the advisory key held: lost
* the same tick key in another key prefix, live here: won
* a key only another prefix has used: won

The one difference is intended: after a *losing* attempt, expired rows of
other keys stay until the next won tick prunes them. No statement reads an
expired row except the claim of the same key, and the prune-of-key handles
that. The Docker suite `scheduled_coordination_pg` covers retention, fencing,
races, ACME-style `release_and_free` and the pre-#3052 advisory lock. It
passes unchanged.

## Reproduce

```sh
# Postgres 16 with shared_preload_libraries = 'pg_stat_statements', autovacuum = off; PG* env set
git show 1ac3175c:autumn/src/scheduler.rs > /tmp/base.rs
SCHEDULER_RS=/tmp/base.rs docs/perf/scheduler-tick-claim/run.sh 300 6   # baseline
SCHEDULER_RS=/tmp/base.rs docs/perf/scheduler-tick-claim/explain.sh
docs/perf/scheduler-tick-claim/run.sh 300 6                             # after
docs/perf/scheduler-tick-claim/explain.sh
PGDATABASE=scratch docs/perf/scheduler-tick-claim/equivalence.sh 1ac3175c
```
