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

## After (batched) — `after.txt`

`PgSyncBackend::apply_push` (`autumn/src/sync/server.rs`) now issues exactly
6 statements per push, independent of `n`: one batched dedup lookup
(`change_id = ANY($3)`), one batched `FOR UPDATE` fetch over the batch's
distinct `(collection, pk)` pairs, one batched version draw
(`generate_series`), one batched multi-row upsert (`UNNEST` + `ON CONFLICT`),
one batched multi-row dedup-record insert, plus the one constant horizon
read.

| metric                         | n=50        | n=250        | n=1000        |
|---------------------------------|------------:|-------------:|--------------:|
| statement calls, before         | 251         | 1251         | 5001          |
| statement calls, after          | **6**       | **6**        | **6**         |
| buffers, before                 | 1085        | 5823         | 23372         |
| buffers, after                  | 981         | 4962         | 20340         |
| buffers reduction               | 9.6%        | 14.8%        | 13.0%         |

Statement count drops from `5n + 1` to a **constant 6** — an N+1 elimination
(the impact-floor category this change clears), independent of the buffers
delta. Buffers also drop 10–15%, a side effect of fewer planner/executor
round trips per row, not the primary claim.

Idempotent replay (pushing the same 250-change batch again — every change
either already applied or already resolved, nothing new to write) drops from
251 statements / 750 buffers to **2 statements / 8 buffers**: the batched
dedup check alone, no writes at all.

## Equivalence

The harness dumps every row touched by each batch (sorted deterministically
by `collection, pk` — the table's key), plus the replay's outcomes, and
prints them to stdout. Every `PushRequest` the harness builds uses fixed
constant timestamps (not wall-clock), so two runs of the harness against
identical code produce byte-identical input and — if the change preserves
semantics — byte-identical output.

`diff` between the row dumps in `baseline.txt` and `after.txt` (1233 lines
each) is empty: every touched row, across all three batch sizes and every
scenario (clean update, clean insert, clean delete, two edits of the same
existing pk in one batch, create-then-edit of the same new pk in one batch,
and idempotent replay) landed in the identical final state, byte for byte.
The full existing `offline_sync_conformance` suite (39 tests, covers
`MemorySyncBackend` and shared push/pull/dedup/conflict/GC/scope-isolation
semantics) and the Docker-gated `offline_sync_pg` conformance suite both
still pass unchanged.
