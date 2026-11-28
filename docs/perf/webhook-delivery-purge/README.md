# Webhook delivery logs: an index for the retention purge

Harness: `gen_sql.py` writes the SQL that
`autumn/src/webhook_outbound/sql_store.rs` ships (through
`../webhook-reactivate/extract_sql.py`: the schema and the two pgbench
delivery scripts; plus the `purge_deliveries_before` DELETE). It fails if a
statement it copies is no longer in the source. `fixture.sql`, `run.sh [LOGS]`,
`report.sql`, `explain.sh`, `equivalence.sh`, `migration.sh`.
Artifacts: `baseline-<LOGS>.txt` and `explain-before.txt` (the baseline
commit), `after-<LOGS>.txt`, `explain-after.txt`, `equivalence.txt`,
`migration-3000000.txt`.

Reproduce: Postgres 16 with `pg_stat_statements` preloaded,
`pg_stat_statements.track = all`, autovacuum off, PG* env set. Then
`run.sh 100000`, `run.sh 1000000`, `run.sh 3000000`, `explain.sh`,
`equivalence.sh`, and `migration.sh` on the database the last `run.sh` left.
For the baseline, run with
`SQL_STORE_RS=<(git show <baseline commit>:autumn/src/webhook_outbound/sql_store.rs)`.

## Workload

One hour of steady state for an app that keeps 30 days of delivery logs and
calls `purge_deliveries_before(now - 30 days)` every hour, as
`docs/guide/outbound-webhooks.md` tells it to.

Fixture (`fixture.sql`, the shape of `../webhook-reactivate/fixture.sql`
with the size a variable): 200 subscriptions, and 100,000, 1,000,000 or
3,000,000 delivery logs spread over 30 days. Receivers are zipf-skewed. 97.5%
of logs are delivered (2xx), 1% are retrying, 1% are in the DLQ, 0.5% are
pending. Payloads are 600-900 bytes. `VACUUM ANALYZE`, then `CHECKPOINT`.
At 3,000,000 the heap is 2,944 MB.

The hour: LOGS/720 deliveries (139 / 1,388 / 4,166), so the table stays at
its size. Each runs the store's statements in the order it issues them: 98%
are a first-attempt success, 2% are a 500 and then a success on attempt 2.
Each log write carries a later `logged_at` than the one before, as the store
stamps the clock on each write. Then one purge removes the hour that fell out
of the window (136 / 1,347 / 4,067 rows).

## Profile (baseline, `pg_stat_statements`)

1,000,000 logs:

| statement | calls | % calls | buffers | % buffers | WAL bytes |
| --- | --- | --- | --- | --- | --- |
| purge `DELETE … WHERE logged_at < $1 AND NOT is_dlq AND response_status BETWEEN 200 AND 299` | 1 | 0.0% | 126,849 | 67.1% | 1,461,763 |
| log upsert `INSERT … ON CONFLICT (id) DO UPDATE` | 2,836 | 22.3% | 26,439 | 14.0% | 2,406,170 |
| log by id `SELECT … WHERE id = $1` | 4,254 | 33.4% | 15,695 | 8.3% | 13,116 |
| subscription by id | 1,448 | 11.4% | 7,240 | 3.8% | 1,729 |
| active subscriptions | 1,388 | 10.9% | 6,943 | 3.7% | 0 |
| `REACTIVATE_SQL` | 2,776 | 21.8% | 5,640 | 3.0% | 25,709 |

The purge is 66.9%, 67.1% and 67.2% of the workload's buffers at the three
sizes, in one call. No statement wrote temp blocks.

## Plan (`explain-before.txt`, `explain-after.txt`)

Before, at every size:

```
Delete on public.autumn_webhook_deliveries
  ->  Seq Scan on public.autumn_webhook_deliveries  (rows=1347 loops=1)
        Filter: ((NOT is_dlq) AND (response_status >= 200) AND (response_status <= 299) AND (logged_at < '1760317200000'::bigint))
        Rows Removed by Filter: 1000041
        Buffers: shared hit=30914 read=94412
```

After:

```
Delete on public.autumn_webhook_deliveries
  ->  Index Scan using idx_autumn_webhook_deliveries_delivered on public.autumn_webhook_deliveries  (rows=1347 loops=1)
        Index Cond: (logged_at < '1760317200000'::bigint)
```

## Mechanism

The purge's predicate is `logged_at < $1 AND NOT is_dlq AND response_status
BETWEEN 200 AND 299`. The only index on `logged_at` is
`idx_autumn_webhook_deliveries_dlq`, partial on `is_dlq`, which the purge
excludes. So the plan is a Seq Scan of the whole heap to find the oldest
hour: rows read grow with the table (100,138 / 1,001,388 / 3,004,166), and
rows deleted grow with the traffic (136 / 1,347 / 4,067). A shorter purge
interval does not help. Every call pays for the whole table.

## Change

One partial index whose predicate is the purge's:

```sql
CREATE INDEX IF NOT EXISTS idx_autumn_webhook_deliveries_delivered
    ON autumn_webhook_deliveries (logged_at)
    WHERE NOT is_dlq AND response_status BETWEEN 200 AND 299;
```

It goes into `WEBHOOK_SCHEMA_SQL` for new tables (Postgres and SQLite).
`ensure_schema` sends no DDL when the tables exist. For those tables, it
builds the index in a background task with `CREATE INDEX CONCURRENTLY IF NOT
EXISTS`, the way the outbox adds `idx_autumn_outbox_claim` (#3429). It skips
the build, with a warning, on a pool of one. A failed build is a warning. It
can leave an invalid index, which the warning and the doc comment say to drop
before a restart. The purge SQL is unchanged.

Lock (`migration.sh`, 3,000,000 logs, 2,944 MB heap): the build holds
`ShareUpdateExclusiveLock` on the table. A writer inserting during the build
held `RowExclusiveLock` beside it, had no `Lock` wait events, and inserted all
200 rows. The build took 2.5 s and `DROP INDEX CONCURRENTLY` took 71 ms (wall
clock, for scale only). The index is valid after the build and absent after
the rollback.

## Measurement

`pg_stat_statements` (buffers = `shared_blks_hit + shared_blks_read`), with
`pg_stat_user_tables` for rows read and `pg_current_wal_lsn()` for WAL. Same
fixture, same seed, same session.

| | 100,000 | 1,000,000 | 3,000,000 |
| --- | --- | --- | --- |
| purge buffers, before → after | 12,656 → 182 | 126,849 → 1,731 | 381,421 → 5,197 |
| purge buffers, delta | −98.6% | −98.6% | −98.6% |
| purge rows read (`seq_tup_read`), before → after | 100,138 → 0 | 1,001,388 → 0 | 3,004,166 → 0 |
| rows the index scan read (`idx_tup_read`) | 136 | 1,347 | 4,067 |
| workload buffers, before → after | 18,921 → 6,587 (−65.2%) | 189,024 → 67,434 (−64.3%) | 567,507 → 203,364 (−64.2%) |
| purge WAL bytes, before → after | 173,339 → 171,365 | 1,461,763 → 1,444,051 | 4,346,268 → 4,295,386 |
| workload statements | 1,287 → 1,287 | 12,745 → 12,745 | 38,155 → 38,155 |
| temp blocks written | 0 → 0 | 0 → 0 | 0 → 0 |

Before, purge buffers scale with the table (12.7k, 127k, 381k for 10×, 3×
the rows). After, they scale with the rows deleted (1.34, 1.29, 1.28 buffers
per row). The plan changed from Seq Scan to Index Scan at all three sizes.

The new index is used: `idx_scan` = 1 and `idx_tup_read` = rows deleted in
`pg_stat_user_indexes` at every size.

## Write cost

A delivered log enters the index when its outcome write sets a 2xx status.
That write was already not HOT (it changes `logged_at`, which the DLQ index
reads; `n_tup_hot_upd` = 0 before and after). So the change adds one index
entry per delivered log and does not turn a HOT update into a non-HOT one.

| | 100,000 | 1,000,000 | 3,000,000 |
| --- | --- | --- | --- |
| index size | 3,864 kB | 38 MB | 113 MB |
| `autumn_webhook_deliveries_pkey`, for scale | 3,824 kB | 37 MB | 141 MB |
| log upsert WAL bytes, before → after | 246,330 → 256,051 (+3.9%) | 2,406,170 → 2,509,316 (+4.3%) | 7,360,256 → 7,675,848 (+4.3%) |
| log upsert buffers, before → after | 2,660 → 2,800 (+5.3%) | 26,439 → 29,967 (+13.3%) | 79,195 → 91,276 (+15.3%) |
| log upsert FPIs, before → after | 5 → 6 | 18 → 20 | 54 → 56 |
| workload WAL (LSN delta), before → after | 466,864 → 474,584 (+1.7%) | 4,079,184 → 4,163,736 (+2.1%) | 12,249,360 → 12,511,040 (+2.1%) |

Throughput is not measured on this shared host. WAL is the write cost, and
it is about 4% on the log write and 2% on the whole hour. The purge's own
savings, two thirds of the hour's buffers, are larger than that.

## Equivalence

The purge SQL is unchanged. An index cannot change which rows match. Both
checks below confirm that:

* `equivalence.sh`: a table of every boundary case. `logged_at` at −5, 0,
  cutoff − 2, cutoff − 1, cutoff, cutoff + 1. `response_status` 199, 200,
  204, 299, 300, 500 and NULL. `is_dlq` true and false, which includes a
  2xx DLQ row. The purge runs once with the index dropped (Seq Scan) and once
  with the index and `enable_seqscan = off` (Bitmap Index Scan on the new
  index). Both runs delete the same 12 ids (2xx, not DLQ, strictly before the
  cutoff), and the md5 of the deleted ids matches.
* `run.sh` end state: the row count and the md5 of every row's md5 in id
  order are identical before and after at all three sizes
  (`equivalence.txt`).
* `autumn/tests/integration/outbox_pg.rs`
  `pg_sql_webhook_store_ensure_schema_adds_delivered_index_to_existing_tables`:
  `ensure_schema` builds the index for existing tables, and the purge's plan
  can use it.
