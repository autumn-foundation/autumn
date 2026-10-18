# Ledger: covering index for the reddit-clone front-page leaderboard

Findings and decision context: #3325.

## Workload
reddit-clone `/`, `/r/{sub}`, `/r/{sub}/posts/{slug}` over HTTP, **41 requests**: 5 front page, 9 subreddit pages
(3 subs x 3), 27 post pages (9 posts - 3 hot, 3 mid, 3 cold - x 3). The comment-thread statement's 27 `calls` are the post pages,
the leaderboard's 5 `calls` are the front pages. Postgres 16 + `pg_stat_statements`. Fixture: `examples/reddit-clone/perf/seed.sql` (100k users, 1k subs, 500k posts,
3M comments, ~3M votes, power-law skew). The seed ends with `VACUUM (ANALYZE)` on every table, so the visibility map is set
(index-only scans need it) — both runs were taken in that state.
The vote rows are deduplicated by `(user, post)` first and their direction derived from a hash of the pair, so the fixture does not depend
on plan or `work_mem`: 2,991,307 votes, checksum `a8faa6de1f369fec4fb993d2b90fbabb`, identical across two seeds (default settings, and
`work_mem=1MB` with parallelism off).

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
`compare.sh` runs `perf/run.sh` once per state and restores the index on any exit (abort-tested: with the app down it died in the baseline
phase and left a valid index). Each state's directory holds the statement rankings, `index_usage.txt` (`pg_stat_user_indexes`) and
`explain_leaderboard.txt` (`EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS)`); the app must be serving on `:3000`.

## Profile (baseline/, same session, before the index)
| statement | calls | buffers | % buffers | temp blks written |
|---|---|---|---|---|
| comment thread (unbounded, inherent; see #3325) | 27 | 230,889 | 58.1% | 2,871 |
| **votes leaderboard `SUM(value) GROUP BY post_id LIMIT 5`** | 5 | 124,720 | **31.4%** | 69,998 |
| subreddit post list | 9 | 39,903 | 10.0% | 0 |
No N+1 (each request-path statement ran once per request).

## Hypothesis
The leaderboard is a Parallel Seq Scan of all 2.99M `votes` heap rows (24,928 buffers/call) feeding a HashAggregate that exceeds
`work_mem` (spilling) and a spilling sort. `idx_votes_post_id` holds only `post_id`, so it cannot supply `value` without a heap
visit. A covering index `(post_id) INCLUDE (value)` makes it an index-only scan already ordered by the group key.

## Change
`examples/reddit-clone/migrations/20261018055123_add_votes_post_id_value_covering_index`:
`CREATE INDEX CONCURRENTLY idx_votes_post_id_value ON votes (post_id) INCLUDE (value) WHERE post_id IS NOT NULL`.
Lock: `SHARE UPDATE EXCLUSIVE` only (sampled `pg_locks` during apply on the earlier fixture: only that mode on `votes`); does not block
reads/writes. `run_in_transaction = false` as in the cms precedent. No `IF NOT EXISTS`: a cancelled build leaves an INVALID index; detect with
`SELECT indexrelid::regclass FROM pg_index WHERE NOT indisvalid`, clean with `DROP INDEX CONCURRENTLY idx_votes_post_id_value`.
`idx_votes_post_id` is intentionally **not** dropped (drop = separate, reviewed decision).

## Measurement (baseline/ vs after/, produced by `compare.sh`)
| counter | tool | before | after |
|---|---|---|---|
| leaderboard buffers (5 calls) | pg_stat_statements | 124,720 | 57,320 (**-54.0%**) |
| leaderboard buffers per call | EXPLAIN (ANALYZE, BUFFERS) | 24,944 | 11,467 |
| temp blocks written (5 calls) | pg_stat_statements | 69,998 | **0** |
| share of workload buffers | pg_stat_statements | 31.4% | 17.4% |
| statements per request | pg_stat_statements calls | 1 | 1 |
| scans of `idx_votes_post_id_value` | pg_stat_user_indexes.idx_scan (`after/index_usage.txt`) | n/a | 5 (one per front-page request) |
| heap fetches | EXPLAIN (`after/explain_leaderboard.txt`) | n/a (seq scan) | 0 |
Plan: Parallel Seq Scan + Partial HashAggregate (spill) + Sort + Gather Merge -> Index Only Scan. Not claimed: any wall-clock improvement.
Buffer totals move a few percent between runs of the same state; temp blocks, plan shape and `idx_scan` do not.

## Equivalence
`after/equivalence.sql` (output in `after/equivalence_result.txt`), in a rolled-back transaction with injected edge data (comment votes with
NULL `post_id`, a net-negative post, a tie/zero-net post): md5 over **all 474,971 groups** ordered `(sum DESC, post_id)` is identical between
the index path and a forced heap path (`enable_*scan=off`, diagnostic session only): `649360b2e95a8eb60e6e361ba29ca049`; 29,810 negative and
39,599 zero-sum groups included. App tests are unchanged (no Rust code changed).

## Write cost (`after/write_cost.txt`; 100k vote inserts, checkpoint before each, 2 runs each)
| | WAL bytes | insert secs |
|---|---|---|
| with index | 253,063,352 / 248,905,576 | 2.62 / 2.83 |
| without | 235,537,040 / 235,529,248 | 2.50 / 2.51 |
WAL **+6.6%**; insert time ~+9% (noisy; WAL is the gated number). Index size 90 MB on a ~200 MB table.

## Migration
`up.sql` 2.1 s, `down.sql` 0.11 s on the 3M-vote fixture; both clean, index valid afterwards.

## Caveats
- The scan is still O(votes) per front-page view: this removes the heap visit and the spill, not the shape. A cache/rollup is the real fix (needs an invariant) — see #3325.
- Index-only scans need an up-to-date visibility map; on a write-heavy `votes` table autovacuum must keep up or heap fetches return.
- Follow-up candidate: `idx_votes_post_id` is now redundant for `post_id` lookups (needs idx_scan over a real window before any drop).
- Earlier exploratory numbers in #3325 and `examples/reddit-clone/perf/baseline/` came from a previous seed version (non-deterministic vote directions); they are superseded by this report.
