# Job enqueue: index the TTL-window dedup check

Harness: `extract_sql.py` writes the SQL that `autumn/src/job.rs` ships
(`pg_insert_job` without `telemetry-otlp`, and `pg_evict_expired_unique_key`).
`gen_pgbench.py` turns it into one pgbench script per job kind. `schema.sql`
(the shipped `autumn_jobs` migrations), `schema_before.sql` (the same without
`20261112170000`), `fixture.sql`, `report.sql`, `run.sh [N] [hist]`,
`explain.sh`, `equivalence.sh` (+ `equivalence_cases.sql`), `write_cost.sh`.
Artifacts: `baseline-<hist>.txt` and `explain-before.txt` (commit a5e852d);
`after-<hist>.txt`, `explain-after.txt`, `equivalence.txt`, `write_cost.txt`.

Reproduce: Postgres 16 with `pg_stat_statements` preloaded and autovacuum
off, PG* env set. `SCHEMA=schema_before.sql run.sh 2000 1000000` is the
baseline, `run.sh 2000 1000000` the after. Then `explain.sh` (on the database
`run.sh` left), `equivalence.sh`, `write_cost.sh`. The scripts turn
`synchronize_seqscans` off: a seq scan of a table past `shared_buffers / 4`
starts where the last one stopped, so an early-exit scan read a different
number of blocks on each run.

Fixture: 1,000,000 finished jobs over 7 days (97% completed, 3% failed). 25%
are a running-window unique job (`digest.send`, key `user:<zipf 20,000>`, kept
on the finished row), 15% a pending-window unique job (`sync.pull`, key
cleared at claim), 10% a 1-hour TTL unique job (`report.build`, key
`acct:<zipf 2,000>`, kept), 50% eight names with no key. In flight: 3,000 +
500 + 300 unique holds, 40 running, 20,000 plain ready rows. Heap 288 MB
(36,906 blocks). The 100,000 and 3,000,000 sizes use the same shape.
Workload: 2,000 enqueues, seeded: 50% plain, 25% running window, 15% pending
window, 10% TTL window (the eviction `UPDATE`, then the `INSERT`).

Each call is a custom plan. diesel-async prepares a raw `sql_query` anew on
every call. pgbench `-M extended` sends an unnamed statement, so it plans the
same way. A custom plan folds `CASE WHEN $8 IS NOT NULL` to one branch.

## Mechanism

The dedup guard is `NOT EXISTS (SELECT 1 FROM autumn_jobs dup WHERE dup.name
= $2 AND dup.unique_key = $6 AND CASE WHEN $8 IS NOT NULL THEN
dup.enqueued_at > NOW() - ttl ELSE dup.status IN ('enqueued', 'running')
END)`. For the running and pending windows the folded predicate has the
status filter, so `idx_autumn_jobs_unique_inflight` serves it (2 buffers).
The TTL window has no status filter, because a finished job holds its key
until the window passes. That partial index holds only in-flight rows, and
no other index has `(name, unique_key)`. So every TTL enqueue read the whole
table, finished history included. A key that is held only stops the scan
when it reaches that row, and the newest rows sit at the end of the heap. The
retention sweep keeps TTL-keyed rows (`NOT (unique_key IS NOT NULL AND
unique_window = 'ttl')`), so this history only grows.

The change is the migration `20261112170000_add_jobs_unique_key_enqueued_index`:
`(name, unique_key, enqueued_at) WHERE unique_key IS NOT NULL`. The SQL does
not change. `dup.unique_key = $6` is strict, so the planner proves the
partial predicate.

## Profile (pg_stat_statements, 2,000 enqueues)

| statement | calls | share of calls | buffers (1M) | share of buffers |
| --- | --- | --- | --- | --- |
| TTL insert | 200 | 9.1% | 7,293,662 | 99.7% |
| plain insert | 1,006 | 45.7% | 19,188 | 0.3% |
| running/pending insert | 794 | 36.1% | 4,654 | 0.06% |
| TTL eviction `UPDATE` | 200 | 9.1% | 561 | 0.01% |

No statement spilled (`temp_blks_written` 0). The plain insert is 87% of
workload WAL and is inherent.

## Measurement

| TTL insert, 200 calls | before | after | delta |
| --- | --- | --- | --- |
| buffers, 100,000 rows | 743,874 | 1,661 | -99.8% |
| buffers, 1,000,000 rows | 7,293,662 | 1,528 | -99.98% |
| buffers, 3,000,000 rows | 21,905,882 | 1,425 | -99.99% |
| `seq_tup_read`, 1M | 200,376,872 | 0 | -100% |
| `seq_tup_read`, 3M | 598,174,414 | 0 | -100% |
| rows inserted (1M) | 29 | 29 | |
| statement WAL bytes (1M) | 277,528 | 446,403 | +60.8% (index entries) |

| workload, 2,000 enqueues | before | after |
| --- | --- | --- |
| buffers, 100k / 1M / 3M | 764,995 / 7,318,065 / 21,931,398 | 23,094 / 26,243 / 27,253 |
| WAL bytes (pg_stat_statements), 1M | 10,184,655 | 10,807,919 (+6.1%) |
| WAL, LSN delta, 1M | 10,267,472 | 10,892,512 (+6.1%) |

The cost scaled with the table before: 3,719 → 36,468 → 109,529 buffers per
TTL enqueue at 100k, 1M and 3M rows. After, it is 8.3 → 7.6 → 7.1.
`idx_autumn_jobs_unique_key_enqueued` `idx_scan` = 200 (the TTL inserts).
`autumn_jobs` `seq_scan` = 0.

Plan (`explain-*.txt`, 3M rows): before, `Seq Scan on autumn_jobs dup`, Rows
Removed by Filter 3,024,973, 110,340 buffers, for a key with no row in the
window. A key held in the window reads 109,309 buffers before it finds the
row. After, `Index Only Scan using idx_autumn_jobs_unique_key_enqueued`,
Heap Fetches 0, 3 and 4 buffers. Execution time went from 460 ms to 0.29 ms,
and the buffer change is in the same direction. The running-window plan does
not change.

## Equivalence (`equivalence.txt`)

* The workload end state at 100k, 1M and 3M rows: the md5 of every row's
  `(id, name, queue, status, unique_key, unique_window, pending_unique_key)`
  ordered by id, and the inserted-row counts per statement, are identical
  without the index and with it.
* 16 edge cases, before and after (the after run sets
  `enable_seqscan = off`, so the small table really uses the index; the
  output names the scan). TTL: a finished row in the window and one before
  it; `enqueued_at` exactly `now() - ttl` (strict `>`, inserts) and 1 µs
  inside; the same key under another name; a key only in
  `pending_unique_key`; an in-flight row from before the window (left to
  `ON CONFLICT`); a failed row and a running-window row in the window; an
  in-flight row in the window; a key that differs in case; an empty key; ttl
  0. Running window: a finished row only, and an in-flight row. A plain job.
  Each case has the same outcome before and after.

## Write cost (`write_cost.txt`, 1M rows, two fresh databases)

Index: 17 MB (2,162 blocks) for 355,574 keyed rows of 1,027,840. Build: 0.48
s in the migration transaction, holding `ShareLock` on `autumn_jobs`. That
blocks `INSERT`, `UPDATE` and `DELETE` until it commits, and reads go on.
`down.sql`: 30 ms.

| 2,000 statements | WAL records | WAL bytes before | after | delta |
| --- | --- | --- | --- | --- |
| plain insert | 14,275 → 14,275 | 15,423,909 | 15,423,909 | 0 |
| unique-key insert | 16,262 → 18,283 | 17,144,681 | 17,419,988 | +1.6% |
| claim update | 14,165 → 16,182 | 17,312,488 | 17,494,776 | +1.1% |
| ack update | 10,911 → 13,034 | 17,028,411 | 17,227,377 | +1.2% |

A status change was not HOT before either (`status` is in other indexes), so
each update of a keyed row adds one index entry, about 100 bytes of WAL.
Rows with no key pay nothing.

## Not done

* A narrower index (`WHERE unique_window = 'ttl'`) holds only the TTL rows,
  but the query would need `dup.unique_window = 'ttl'` for the planner to
  use it. A row of the same name and key from another window would then no
  longer block a TTL enqueue. That is a change in meaning, so it was left
  out.
* `CREATE INDEX CONCURRENTLY` cannot run in the migration transaction. The
  previous `autumn_jobs` index migration (`20261026170000`) made the same
  choice.
