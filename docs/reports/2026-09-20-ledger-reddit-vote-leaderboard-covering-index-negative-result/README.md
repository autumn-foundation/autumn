# 🗃️ Ledger: reddit-clone front-page vote leaderboard — covering-index negative result

## 🎯 Workload

`examples/reddit-clone`'s front page (`GET /`, `routes::posts::front_page`,
`examples/reddit-clone/src/routes/posts.rs:80-179`) issues three statements: the
hot-posts listing (`ORDER BY hot_rank DESC LIMIT 25`), the "Top posts by votes"
leaderboard (`VoteRepository::sum_value_grouped_by_post_id().order_by_aggregate_desc().limit(5)`,
`examples/reddit-clone/src/repositories.rs:86-90`), and a small `id = ANY(...)`
title lookup for the leaderboard's 5 winners.

Profiled against a production-shaped fixture applied to a real Postgres 16
instance (schema replayed from `examples/reddit-clone/migrations/`, no
Docker available in this environment — `pg_stat_statements` loaded via
`shared_preload_libraries` on a local `postgresql@16` cluster instead of a
testcontainer): 20,000 users, 50 subreddits, 30,000 posts, 15,000 comments,
733,609 votes with realistic power-law skew (a long tail of 0-3 organic votes
per post, plus 200 "hot" posts absorbing the bulk of the volume — 688,614
post-directed votes, 44,995 comment-directed votes with `NULL post_id`, so
the leaderboard's `IS NOT NULL` group guard has real rows to exclude, not a
vacuous predicate). ~5% of existing post votes had their value flipped after
the bulk load (the same mutation `Post::react()` performs on a changed vote)
to produce real dead tuples, then `VACUUM` (not `FULL`) + `ANALYZE` models
the steady state autovacuum reaches on a live table, rather than a pristine
just-loaded one.

Reproduce (no Docker required — points at any reachable Postgres 16):

```sh
createdb reddit_ledger
psql -d reddit_ledger -c 'CREATE EXTENSION IF NOT EXISTS pg_stat_statements;'
for m in 20260419000000_create_reddit 20260427000000_add_user_avatar \
         20260702000001_create_tags 20260820000000_polymorphic_comments; do
  psql -d reddit_ledger -f examples/reddit-clone/migrations/$m/up.sql
done
psql -d reddit_ledger -f docs/reports/2026-09-20-ledger-reddit-vote-leaderboard-covering-index-negative-result/fixture/seed.sql
psql -d reddit_ledger -f docs/reports/2026-09-20-ledger-reddit-vote-leaderboard-covering-index-negative-result/baseline/queries.sql
psql -d reddit_ledger -f docs/reports/2026-09-20-ledger-reddit-vote-leaderboard-covering-index-negative-result/after/queries.sql
```

## 📈 Profile

`pg_stat_statements`, reset immediately before the three front-page
statements ran once each:

| statement | calls | shared_blks_hit | shared_blks_read | total buffers | % of page's buffers |
|---|---:|---:|---:|---:|---:|
| leaderboard: `SUM(value) GROUP BY post_id ... LIMIT 5` | 1 | 5,656 | 749 | **6,405** | **99.30%** |
| hot-posts listing: `ORDER BY hot_rank DESC LIMIT 25` | 1 | 0 | 27 | 27 | 0.42% |
| title lookup: `id = ANY($1..$5)` | 1 | 18 | 0 | 18 | 0.28% |

The leaderboard query is the front page's cost, by two orders of magnitude —
comfortably clears the "≥5% of total buffers" bar. The other two statements
are cheap, correctly-indexed point/range lookups (`idx_posts_hot_rank`,
primary-key `ANY`) and are not touched by this report.

## 🧭 Plan

`EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS)` (full output in
`baseline/output.txt`): `Limit -> Sort (top-N heapsort) -> Finalize
HashAggregate -> Gather (2 workers) -> Partial HashAggregate -> Parallel Seq
Scan on votes, Filter: (votes.post_id IS NOT NULL)`. `Rows Removed by
Filter: 14998` against `229,538` rows returned per worker-loop — the
`post_id IS NOT NULL` predicate matches 93.9% of the table (688,614 of
733,609 rows), so this is a near-full-table aggregate by construction, not a
selective lookup.

## 💡 Hypothesis

The `votes` table has `idx_votes_post_id` (a plain, non-covering B-tree on
`post_id`), so the aggregate's `Seq Scan` still has to fetch every row's
heap tuple to read `value`. A **partial covering index** —
`(post_id) INCLUDE (value) WHERE post_id IS NOT NULL`, whose predicate
matches the query's own `WHERE post_id IS NOT NULL` exactly — should let an
`Index Only Scan` answer the aggregate without touching the heap at all
(confirmed the table is fully vacuumed, so the visibility map can actually
serve an index-only scan), cutting buffers roughly in proportion to how much
narrower `(post_id, value)` is than the full `votes` row.

## 🔧 Change (and why it isn't shipped)

Added `CREATE INDEX idx_votes_post_id_value_covering ON votes (post_id)
INCLUDE (value) WHERE post_id IS NOT NULL;`, `ANALYZE`d, then re-ran the
identical leaderboard query with `pg_stat_statements` reset.

**The planner did not use it.** Buffers after adding the index: 6,402 —
statistically the same as baseline's 6,405 (`after/output.txt`), and
`pg_stat_user_indexes.idx_scan` for `idx_votes_post_id_value_covering` is
**0**. `EXPLAIN` confirms the plan is unchanged: still `Parallel Seq Scan on
votes`.

Forcing the issue (`SET enable_seqscan = off` — diagnostic only, never
shipped, not a fix per this repo's own banned-changes list) does make
Postgres pick `Parallel Index Only Scan using
idx_votes_post_id_value_covering`, `Heap Fetches: 0`, and its buffer count
*is* genuinely lower — `shared hit=3 read=2641` = 2,644 total, a 58.7%
reduction versus the seq-scan plan's 6,402-6,405. So the index isn't
inert — the planner's cost model just correctly judges the seq scan cheaper
at this selectivity (93.9% of the table matches the guard), and it is right
to: an index nothing ever chooses is a pure write tax on every vote insert,
forever, for a benefit this workload never collects.

The index was **dropped** after the comparison (`after/output.txt`'s final
`DROP INDEX` + the empty-of-it `pg_stat_user_indexes` listing that follows
it). Nothing in this repository is changed by this report.

## 📊 Measurement

| | total buffers (hit+read) | Δ vs baseline | `idx_scan` | plan |
|---|---:|---:|---:|---|
| baseline (no covering index) | 6,405 | — | n/a | Parallel Seq Scan |
| after (covering index present, unforced) | 6,402 | **-0.05%** | **0** | Parallel Seq Scan (unchanged) |
| after, forced (`enable_seqscan=off`, diagnostic only) | 2,644 | -58.7% | n/a | Parallel Index Only Scan, Heap Fetches: 0 |

Tool: `pg_stat_statements` (`shared_blks_hit + shared_blks_read`) and
`pg_stat_user_indexes.idx_scan`, both reset/read within the same `psql`
session as the query they measure.

This does **not** clear the impact floor: the shipped state (no forcing) is
a 0.05% buffer change, and the index that would unlock the forced 58.7% is
never chosen, so its "elimination" of the seq scan doesn't happen. Per this
repo's own VERIFY step — "Confirm the new index is actually used: `idx_scan`
incremented in `pg_stat_user_indexes`. A created-but-unused index is a pure
write tax and must be reverted." — the correct action is revert, not ship.

## ✅ Equivalence

N/A — no query text changed, nothing to compare. The only artifact under
test was an index's presence, and it was dropped.

## 💸 Write cost

Not measured, because the index isn't shipped. For the record: it would
have added one entry per post-directed vote insert (the partial predicate
excludes comment votes, so comment-vote inserts would have paid nothing) —
moot, since the index was reverted.

## 🔬 Reproduce

```sh
psql -d reddit_ledger -f docs/reports/2026-09-20-ledger-reddit-vote-leaderboard-covering-index-negative-result/fixture/seed.sql
psql -d reddit_ledger -f docs/reports/2026-09-20-ledger-reddit-vote-leaderboard-covering-index-negative-result/baseline/queries.sql
psql -d reddit_ledger -f docs/reports/2026-09-20-ledger-reddit-vote-leaderboard-covering-index-negative-result/after/queries.sql
```

Raw output: `baseline/output.txt` (profile + `EXPLAIN` before), `after/output.txt`
(index added, unforced plan unchanged + `idx_scan=0`, forced comparison, then
dropped). Fixture: `fixture/seed.sql` (+ `fixture/seed_output.txt`, one
concrete run's row counts).

## Other candidates ruled out this run

Before settling on this workload, two research passes searched
`examples/*/src`, `autumn/src`, `autumn-billing/src`, `autumn-search/src`,
`autumn-admin-plugin/src`, `autumn-cli/src`, `autumn-media-plugin/src`,
`autumn-edge/src`, `autumn-storage-s3/src` and `autumn-cache-redis/src` for
an unbatched `.load()`/`.first()` inside a loop (the highest-value pattern
per this repo's own Diesel guidance). The one genuine hit —
`examples/cms/src/content.rs`'s `lock_terms` per-term `FOR UPDATE` loop — is
already claimed by open PR #2827 ("batch cms `set_post_terms`'s `lock_terms`
loop"), so it wasn't duplicated here. No other unclaimed N+1 of meaningful
production scale was found; this codebase's example apps and framework
crates are, at this point, unusually well-batched.
