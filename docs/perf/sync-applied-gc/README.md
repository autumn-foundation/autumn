# `PgSyncBackend::gc_applied` — negative result (no index)

Ledger run on `DELETE FROM autumn_sync_applied WHERE applied_at < $1`
(`autumn/src/sync/server.rs`, `gc_applied`). The Ledger catalog flags it as a
full scan: the table has only its `(scope, device_id, change_id)` primary key.
The scan is real. **Adding an index does not pay for itself, so none was added.**

## Workload and fixture

One nightly `gc_applied` sweep (30-day dedup retention) against the push-dedup
table, plus the ingest it competes with (a day of `apply_push` dedup inserts).
`fixture.sql` seeds 200k / 1M / 3M rows over 31 days (so a sweep removes
~3.2%), 200 Zipf-skewed scopes, 5,000 devices, ~3% rows with a `resolved_row`,
`applied_at` in insert order with ±10 min jitter, autovacuum off, `VACUUM
ANALYZE`d. Postgres 16.15, `pg_stat_statements.track = all`.

## Reproduce

```sh
export PGHOST=<socket dir> PGPORT=<port> PGDATABASE=postgres   # pg_stat_statements preloaded
for v in baseline btree brin; do for n in 200000 1000000 3000000; do
  docs/perf/sync-applied-gc/run.sh $v $n > $v-$n.txt; done; done
```

## Profile

The sweep is the only statement that touches the whole table. In one day of
steady state at 3M rows (≈97k ingested rows ≈ 395k buffers) the sweep's 127,630
buffers are ~24% of the day's buffers, so it clears the 5% bar. Inside the
sweep, the Seq Scan is `heap_blocks` buffers (30,041 at 3M, 24%); the other
~97.6k are the per-row delete itself and no index removes them.

## Measurement (pg_stat_statements / EXPLAIN BUFFERS / pg_wal_lsn_diff)

| rows | variant | sweep total buffers | heap blocks read from disk | rows read at scan node | ingest 50k: WAL bytes | ingest 50k: buffers |
|---:|:--|---:|---:|---:|---:|---:|
| 200k | none (Seq Scan) | 8,508 | 0 | 200,000 | ~10.6M¹ | 203,626 |
| 200k | btree | 8,495 (-0.2%) | 0 | 6,449 | 14,486,464 | 254,444 |
| 200k | BRIN | 6,579 (-22.7%) | 0 | ~16.4k (128 blocks) | 12,057,088 | 316,355 |
| 1M | none | 42,537 | 1,886 | 1,000,000 | 10,662,096 | 203,597 |
| 1M | btree | 58,185 (**+36.8%**) | 0 | 32,245 | 13,974,144 | 254,415 |
| 1M | BRIN | 32,631 (-23.3%) | 1 | ~49k (384 blocks) | 11,230,504 | 311,907 |
| 3M | none | 127,630 | 29,006 | 3,000,000 | 10,228,608 | 203,226 |
| 3M | btree | 186,879 (**+46.4%**) | 0 | 96,755 | 13,539,632 | 254,400 |
| 3M | BRIN | 97,781 (-23.4%) | 0 | ~131k (1,024 blocks) | 10,985,328 | 314,191 |

¹ The first 200k run had one-off WAL noise (20.1M); 1M/3M are representative.

Statement count is 1 in every case (no N+1). No temp blocks. Sweep WAL is the
same ±noise for every variant (it is dominated by the deletes).

## Why it is a negative result

* **btree**: the index turns a 3M-row scan into a 96,755-row scan and removes
  all physical reads, but every matched row is then visited twice (index scan
  heap fetch, then the delete), so *touched* buffers rise 37–46%. It also adds
  ~+33% WAL and +25% buffer touches to every push-dedup insert, forever.
* **BRIN**: the best candidate. Sweep buffers -23% at all three sizes, disk
  reads 29,006 → 0, rows read -96%, index is 3 blocks. But `brininsert` touches
  the range summary for every inserted row: ingest buffers +53–55%, WAL +7–9%.
  Per day at 3M rows (≈97k ingested + one sweep) the net is
  ~523k → ~703k buffers, i.e. worse overall. The sweep win (-30k buffers) is
  smaller than the ingest tax (+~210k).
* The floor applies to the workload, not the sweep in isolation, and neither
  index clears it there. Wall-clock (btree sweep 67 ms vs 265 ms at 3M) was
  not used as evidence.

## Equivalence

Every variant deletes the same rows: survivors count and `md5` of
`change_id ORDER BY scope, device_id, change_id` are identical to baseline at
all three sizes (`to_delete`/`survivors`/`at_boundary` rows in each `.txt`,
boundary = exact horizon, 0 rows on it because of jitter).

## When to revisit

An index could win if sweeps ran far more often than ingest (they do not), the
table were far larger than RAM (the baseline's disk reads would then dominate
and BRIN's 29k → 0 reads would matter), or ingest were batched so BRIN summary
updates amortised. Re-run `run.sh` at that point. `autumn_sync_applied`'s
`gc_applied` catalog note can stay as "full scan, accepted".
