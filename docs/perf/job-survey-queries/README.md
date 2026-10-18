# Job-queue survey queries — baseline (no change yet)

Ledger baseline for the two periodic survey queries in `autumn/src/job.rs`:
`pg_update_concurrency_blocked_gauges` (maintenance loop, `survey_blocked`) and
`pg_update_queue_depth_gauges` (survey loop, every replica). SQL is verbatim in `surveys.sql`.

Reproduce (needs a server with `shared_preload_libraries=pg_stat_statements`, PG16):

```sh
PGHOST=... PGPORT=... PGUSER=... ./run.sh 5000   > baseline_5k.txt
./run.sh 50000 > baseline_50k.txt;  ./run.sh 500000 > baseline_500k.txt
```

Fixture (`fixture.sql`, seeded): 10x`ready` rows; 10 skewed job names, 4 queues; ~30% of due
rows carry `concurrency_limit=5` keyed by one of 200 tenants (5% NULL key); 0.5% running;
2% future-scheduled; rest succeeded/dead; dead tuples from a post-seed UPDATE; ANALYZE, no VACUUM.

## Baseline (shared hit+read per single execution, `EXPLAIN (ANALYZE, BUFFERS)`)

| ready jobs | blocked-concurrency survey | queue-depth survey |
|-----------:|---------------------------:|-------------------:|
| 5,000      | 3,502                      | 103                |
| 50,000     | 164,940                      | 1,024            |
| 500,000    | 8,468,269 (hit+read)       | 91,018             |

pg_stat_statements over 10 ticks of both: the blocked survey is **97.1% / 99.4% / 99.1% of
buffers** (5k/50k/500k) at 50% of calls. Nothing spilled (temp blocks 0).

Mechanism: the correlated subplan runs once per limited ready row (loops=150,159 at 500k) and
probes `idx_autumn_jobs_concurrency_running (name, concurrency_key)` with `name` only, because
`concurrency_key IS NOT DISTINCT FROM ...` is not indexable; each probe heap-reads every running
row of that name (529 index hits/probe at 500k) and filters the key afterwards. Cost scales as
limited-ready x running-per-name. The survey returned 0-160 blocked rows, i.e. almost all of it is
wasted work. Queue-depth survey is an inherent GROUP BY over the ready backlog.

Not yet done (hard gate): the fix, after-measurement, result-equivalence proof. Wall-clock
figures in the artifacts are not evidence and are not used.
