# Ledger: Postgres job claim under cost deferral

Harness, fixture and raw artifacts: `benchmarks/ledger-jobs-claim-deferred/`
(`baseline/` committed before the change, `after/` from the same harness and fixtures).

## Workload

A worker poll loop under cost deferral (issue #1720): per poll, one enqueue, one claim with
`deferred = ['report.slow']`, one ack, driven by `pgbench -M prepared` (300 polls) against
`autumn_jobs` with 1,000,000 finished rows, 100,000 ready rows over 9 names, 200 running
rows, and a parked backlog of the deferred name of 50k / 200k / 800k rows (oldest `run_at`,
so it sits at the head of `idx_autumn_jobs_queue_ready`). Seeded, deterministic (`fixture.sql`).
Postgres 16.15, `pg_stat_statements.track=all`, single client (no contention, so no wait-event samples).

## Profile (pg_stat_statements, 300 polls)

| Parked backlog | Claim buffers | Share of workload buffers | Share of calls |
| --- | ---: | ---: | ---: |
| 50k | 277,957 | 97.1% | 33% |
| 200k | 1,093,270 | 99.3% | 33% |
| 800k | 4,354,264 | 99.7% | 33% |

The ack (4.6k) and enqueue (3.8k) are flat. `temp_blks_written` is 0 throughout.

## Plan (single claim, `EXPLAIN (ANALYZE, BUFFERS)`)

Before: `Index Scan using idx_autumn_jobs_queue_ready` with `Rows Removed by Filter` = the
parked backlog (50,000 / 200,000 / 800,000); buffers 932 / 3,629 / 14,497 (scales linearly).
After: recursive skip scan (`Index Only Scan using idx_autumn_jobs_queue_name_ready`, ~10 loops,
one per distinct ready name), then an early-stopping walk over the runnable names; ~108 buffers per claim at every size.

## Hypothesis

The deferral predicate `NOT (name = ANY($3))` is not in `idx_autumn_jobs_queue_ready (queue, run_at)`,
so each parked row costs a heap fetch before it is discarded, and the parked backlog only
grows because nothing claims it.

## Change

`pg_claim_sql_single_queue_deferred` (used only when `deferred` is non-empty): skip-scan the
distinct ready names on a new partial index `(queue, name, run_at) WHERE status = 'enqueued'`,
order the runnable names by the `run_at` of their earliest due row, and walk them: each probe
returns the name's earliest concurrency-eligible row strictly earlier than the best so far, and
the walk stops at the first name whose head cannot beat the best. The first version of this
query probed every name to exhaustion; Codex review found (and `saturated.sh` reproduces) that
a non-deferred name with a large concurrency-saturated backlog made that O(backlog); see below. Migration
`20261026170000_add_jobs_queue_name_ready_index`: plain `CREATE INDEX IF NOT EXISTS`,
`SHARE` lock for the build (2.09 s on 1.9M rows / 900k ready; 1.78 s on re-apply),
`down.sql` `DROP INDEX` 37 ms. The migration system is transactional, so `CONCURRENTLY` was not an option.
The index is also in `SHARD_JOB_SCHEMA`.

## Measurement (300 polls; claim statement, pg_stat_statements)

| Parked backlog | Buffers before | Buffers after | Delta | Rows filtered out per claim, before → after |
| --- | ---: | ---: | ---: | --- |
| 50k | 277,957 | 32,423 | -88.3% | 50,000 → 0 |
| 200k | 1,093,270 | 32,421 | -97.0% | 200,000 → 0 |
| 800k | 4,354,264 | 32,422 | -99.3% | 800,000 → 0 |

Statements per poll are unchanged (3). `temp_blks_written` 0 before and after.
`idx_autumn_jobs_queue_name_ready.idx_scan` = 6300 after each 300-poll run (`after/indexes.*.txt`);
`idx_autumn_jobs_queue_ready` = 0 in the deferred loop, so the new index is the one in use.

## Saturated-backlog check (regression found in review, fixed)

One claim, a non-deferred name `sat` with N due rows, `concurrency_limit = 1` and one running,
an earlier eligible `ok` job, and a 1,000-row parked name (`saturated.sh`, `saturated_result.txt`):

| `sat` backlog | shipped claim | first rewrite | final claim |
| --- | ---: | ---: | ---: |
| 20k | 23 | 40,430 | 51 |
| 100k | 25 | 201,976 | 95 |
| 200k | 26 | 403,898 | 61 |

The first rewrite scanned the whole saturated backlog on every claim; the final claim is flat.

## Equivalence

`equivalence.sql` drains an edge-case fixture (NULL / non-NULL concurrency keys with limits,
equal-`run_at` ties, a future row, a row 1 us in the past, a second queue, names sorting
before/after/around the parked ones, a prefix of a parked name, a running row holding a slot)
with the shipped and the final query for five deferred sets, including all names deferred and a nonexistent
name. Result (`equivalence_result.txt`): identical claim counts, identical `run_at`
sequence, identical claimed ids per `run_at` tie group. Order inside a `run_at` tie was
unspecified before and remains so. The text extracted from the Rust source was also run
against the 200k fixture and claims the same row as the oracle.
No bi-temporal or validity semantics are touched. Isolation is unchanged.

## Write cost

`write_cost_result.txt` (CHECKPOINT first, identical work, alternated, two runs each):
200k inserts: 101.0 MB -> 123.0 MB WAL (+21.7%, ~110 B/row). 50k claim updates and 50k acks:
byte-identical WAL, because the partial predicate excludes those rows. Index size 42 MB vs the
27 MB of `idx_autumn_jobs_queue_ready` at 900k ready rows. Wall-clock throughput is not claimed.
Per-statement WAL in the pgbench loop varies with full-page-write timing and is not used as evidence.

## Limits

- Helps only while deferral is active; the common claim path is untouched and gains nothing.
- The multi-queue (`[jobs] queues`) deferred claim is not changed.
- Docker tests (`cost_deferral_durable`) could not be run in the authoring sandbox; CI's sweep runs them.

## Reproduce

```
export PGHOST=... PGUSER=... PGDATABASE=...   # PG with pg_stat_statements preloaded + autumn_jobs migrations
cd benchmarks/ledger-jobs-claim-deferred
./run.sh baseline baseline && ./run.sh after after   # after = new index + rewritten claim
psql -f equivalence.sql
```
