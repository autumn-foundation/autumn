# Job claim: count running jobs once per claim

Harness: `schema.sql` (the shipped `autumn_jobs` migrations), `fixture.sql`,
`claim.sql` (shipped claim before), `claim_after.sql` (after, generated from
`autumn/src/job.rs` by `extract_claim.py`), `workload.sql`, `report.sql`,
`run.sh <N> [hist]`.
Artifacts: `baseline-<N>.txt` (commit c1bd781), `after-<N>.txt`,
`experiments-50000.txt`, `equivalence.txt`, `running_set.txt`.

Reproduce: Postgres 16 with `pg_stat_statements` preloaded, PG* env set, then
`run.sh 50000` and `CLAIM=claim_after.sql run.sh 50000`; `equivalence.sh`;
the `running_set.sql` loop in the PR.

Fixture: 1,000,000 finished rows (97% completed, 3% failed, over 7 days),
20,000 unlimited ready rows, 20,000 future rows, 30 running unlimited jobs, and
N ready rows of `mail.deliver` whose group (`concurrency_key = 'smtp'`, limit 2)
already has 2 running. Those N rows are the oldest ready rows, because a
blocked group's backlog stops moving while newer work drains past it.
Workload: 100 claim + ack cycles of the single-queue claim, after `CHECKPOINT`.

| N (saturated rows) | claim buffers, 100 calls, before | after | rows removed per claim |
| --- | --- | --- | --- |
| 0 | 2,353 | 2,353 | 0 / 0 |
| 5,000 | 1,077,971 | 78,376 (-92.7%) | 5,000 / 5,000 |
| 50,000 | 10,182,163 | 182,486 (-98.2%) | 50,000 / 50,000 |
| 200,000 | 40,540,055 | 540,432 (-98.7%) | 200,000 / 200,000 |

Before, the scan of `idx_autumn_jobs_queue_ready` ran the correlated
`COUNT(*)` of running jobs for every row it passed (SubPlan, loops = N). After,
the counts are one InitPlan (2 buffers) that the filter reads with `->>`. The
walk still passes the N blocked rows; skipping them needs an index keyed by the
group, which this change does not add.

Equivalence: the ordered claimed-id md5 is identical before and after at every
N. `equivalence.sh` drains an edge-case fixture (NULL, `''` and `'null'` keys
as three groups, mixed limits in one group, limit 0, a group held by a running
row with no limit, a running row in another queue, names and keys that need
JSON escaping, `run_at` ties, the `now()` boundary, a future row, deferred
names) for 4 deferred sets x 3 ack cadences: 12 identical sequences, on
Postgres 16.15 and on 11.22 (the testcontainers default).

Trade-off (`running_set.txt`): the InitPlan reads the whole running set, the old
probe only the candidate's group. With an unsaturated limited job at the head
and R running rows just written and spread one per heap page, a claim reads
56 / 354 / 1,361 buffers before and 57 / 366 / 1,399 after (R = 50 / 500 / 2,000).
Claims that reach no concurrency-limited row do not run the InitPlan.

Write cost: no index, no migration. Claim WAL, 100 calls: 1,321,138 vs
1,321,138 (N = 0), 1,230,859 vs 1,234,108 (N = 50,000).
