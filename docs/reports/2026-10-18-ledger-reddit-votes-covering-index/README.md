# Ledger: covering index for the reddit-clone front-page leaderboard

Findings and decision context: #3325.

## Workload
reddit-clone `/`, `/r/{sub}`, `/r/{sub}/posts/{slug}` over HTTP, **41 requests**: 5 front page, 9 subreddit pages
(3 subs x 3), 27 post pages (9 posts - 3 hot, 3 mid, 3 cold - x 3). The comment-thread statement's 27 `calls` are the post pages,
the leaderboard's 5 `calls` are the front pages. Postgres 16 + `pg_stat_statements`. Fixture: `examples/reddit-clone/perf/seed.sql` (100k users, 1k subs, 500k posts,
3M comments, ~3M votes, power-law skew). The seed ends with `VACUUM (ANALYZE)` on every table, so the visibility map is set
(index-only scans need it) — both runs were taken in that state.

Prerequisite: Postgres must preload `pg_stat_statements`; the stock compose service does not. e.g.
`docker compose -f examples/reddit-clone/docker-compose.yml run -d -p 5433:5432 db -c shared_preload_libraries=pg_stat_statements -c track_io_timing=on`
(or add that `command:` to your local compose file; the compose invocation is untested here — my sandbox has no Docker, I used a local Postgres 16 with the same settings). `run.sh` checks this and runs `CREATE EXTENSION IF NOT EXISTS pg_stat_statements`.

Reproduce (one database for every command; the default matches `docker-compose.yml`/`autumn-dev.toml`):
```
export DATABASE_URL=postgres://autumn:autumn@127.0.0.1:5433/reddit
# boot the app once so it applies migrations (the new index is built then), then:
psql "$DATABASE_URL" -f examples/reddit-clone/perf/seed.sql       # ~4 min
examples/reddit-clone/perf/compare.sh out/                         # drops the index -> out/before, rebuilds it -> out/after
psql "$DATABASE_URL" -f docs/reports/2026-10-18-ledger-reddit-votes-covering-index/after/equivalence.sql
```
`compare.sh` runs `perf/run.sh` once per state; the app must be serving on `:3000`. Buffer totals vary ~3% run to run
(re-run on the same fixture: before 124,725 vs 128,890; after 57,325 both times); temp blocks and plan shape do not.

## Profile (baseline/, same session, before the index)
| statement | calls | buffers | % buffers | temp blks written |
|---|---|---|---|---|
| comment thread (unbounded, inherent; see #3325) | 27 | 230,946 | 57.5% | 2,871 |
| **votes leaderboard `SUM(value) GROUP BY post_id LIMIT 5`** | 5 | 128,890 | **32.1%** | 69,922 |
| subreddit post list | 9 | 39,900 | 9.9% | 0 |
No N+1 (each request-path statement ran once per request).

## Hypothesis
The leaderboard is a Parallel Seq Scan of all 2.99M `votes` heap rows (24.9k buffers/call) feeding a HashAggregate that
exceeds `work_mem` (5 batches, ~19 MB/worker) and a spilling sort. `idx_votes_post_id` holds only `post_id`, so it cannot
supply `value` without a heap visit. A covering index `(post_id) INCLUDE (value)` makes it an index-only scan that is already
ordered by the group key, so no hash spill and no heap reads.

## Change
`examples/reddit-clone/migrations/20261018055123_add_votes_post_id_value_covering_index`:
`CREATE INDEX CONCURRENTLY idx_votes_post_id_value ON votes (post_id) INCLUDE (value) WHERE post_id IS NOT NULL`.
Lock: `SHARE UPDATE EXCLUSIVE` only (sampled `pg_locks` during apply: only that mode on `votes`); does not block reads/writes.
`run_in_transaction = false` as in the cms precedent. No `IF NOT EXISTS`: a cancelled build leaves an INVALID index; detect with
`SELECT indexrelid::regclass FROM pg_index WHERE NOT indisvalid`, clean with `DROP INDEX CONCURRENTLY idx_votes_post_id_value`.
`idx_votes_post_id` is intentionally **not** dropped (drop = separate, reviewed decision).

## Measurement
| counter | tool | before | after |
|---|---|---|---|
| leaderboard buffers (5 calls) | pg_stat_statements | 128,890 | 57,325 (**-55.5%**) |
| leaderboard temp blocks written | pg_stat_statements | 69,922 | **0** |
| leaderboard share of workload buffers | pg_stat_statements | 32.1% | 17.4% |
| statements per request | pg_stat_statements calls | 1 | 1 |
| index used | pg_stat_user_indexes.idx_scan | n/a | 5 (one per front-page request) |
| heap fetches | EXPLAIN | n/a (seq scan) | 0 |
Plan: Parallel Seq Scan + Partial HashAggregate (spill) + Sort + Gather Merge -> Index Only Scan (`after/explain_leaderboard.txt`).
Not claimed: any wall-clock improvement.

## Equivalence
`after/equivalence.sql`, run in a rolled-back transaction with edge data injected (comment votes with NULL `post_id`,
a post with net-negative votes, a post with exactly-zero net, ties): md5 over **all 474,912 groups** ordered `(sum DESC, post_id)`
is identical between the index path and a forced heap path (`enable_*scan=off`, diagnostic session only):
`00d2057071beb007ec002b525bf659c2`; 30,641 negative and 40,889 zero-sum groups included. Top-20 identical pre/post migration.
App tests are unchanged (no Rust code changed).

## Write cost (100k vote inserts, checkpoint before each, 2 runs each)
| | WAL bytes | insert secs |
|---|---|---|
| with index | 348,528,016 / 345,501,568 | 3.36 / 3.32 |
| without | 332,112,208 / 332,104,352 | 3.39 / 3.06 |
WAL **+4.0% to +4.9%** (~+4.5%); insert time ~+3% (noisy; WAL is the gated number). Index size 90 MB on a 201 MB table.

## Migration
Apply via app boot: valid index, 90 MB; `up.sql` re-applied by hand in 2.9 s on the 3M-vote fixture; `down.sql` 0.12 s. Both clean.

## Caveats
- The scan is still O(votes) per front-page view: this removes the heap visit and the spill, not the shape. A cache/rollup is the real fix (needs an invariant) — see #3325.
- Index-only scans need an up-to-date visibility map; on a write-heavy `votes` table autovacuum must keep up or heap fetches return.
- Follow-up candidate: `idx_votes_post_id` is now redundant for `post_id` lookups (needs idx_scan over a real window before any drop).
