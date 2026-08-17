# `PgSyncBackend::apply_push` — push batching perf record

Ledger run profiling `POST /sync/push` (`PgSyncBackend::apply_push`,
`autumn/src/sync/server.rs`) against a production-shaped fixture. See
`autumn/tests/integration/offline_sync_push_batching_perf.rs` for the harness
and fixture generator.

## Reproduce

```sh
cargo test -p autumn-web --features "test-support,offline-sync" \
  --test integration_tests -- --ignored offline_sync_push_batching_profile \
  --nocapture --test-threads=1
```

Requires Docker. Starts a Postgres 16 testcontainer with
`pg_stat_statements` preloaded, seeds 20,000 pre-existing rows (six
collections at a 40/20/15/15/7/3% skew, 5% tombstones, real dead tuples from
a post-seed `UPDATE`, `ANALYZE`'d), then runs push batches of 50/250/1000
changes shaped like a device catching up after being offline.

## Baseline (before batching) — `baseline.txt`

One push batch of `n` changes issued exactly 5 round trips per change,
independent of what the change actually did (update, insert, delete, or a
conflict):

| statement (per change)                          | calls @ n=50 | calls @ n=250 | calls @ n=1000 |
|---------------------------------------------------|-------------:|--------------:|---------------:|
| dedup check (`autumn_sync_applied` SELECT)         | 50           | 250           | 1000           |
| current-row fetch (`autumn_sync_rows` SELECT FOR UPDATE) | 50    | 250           | 1000           |
| version allocation (`nextval`)                     | 50           | 250           | 1000           |
| row upsert (`autumn_sync_rows` INSERT ... ON CONFLICT) | 50       | 250           | 1000           |
| dedup record insert (`autumn_sync_applied` INSERT) | 50           | 250           | 1000           |
| **total (+1 constant horizon read)**               | **251**      | **1251**      | **5001**       |

Statement count is exactly `5n + 1` at every size — a textbook N+1, invisible
in a buffer-cost ranking (each statement is a cheap single-row point lookup:
3–7 buffers) and only visible in the `calls` ranking.

`EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS)` for the two SELECTs
confirms they're individually trivial (`Index Scan`, 3 and 7 buffers,
<0.1ms) — see `baseline.txt`, the two `=== EXPLAIN ... ===` sections.
