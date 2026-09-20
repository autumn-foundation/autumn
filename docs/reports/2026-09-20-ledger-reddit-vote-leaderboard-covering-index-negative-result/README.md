# 🗃️ Ledger: reddit-clone front-page vote leaderboard — covering-index negative result

## 🎯 Workload

`examples/reddit-clone`'s front page (`GET /`, `routes::posts::front_page`,
`examples/reddit-clone/src/routes/posts.rs:80-179`) issues five statements:
the hot-posts listing (`ORDER BY hot_rank DESC LIMIT 25`), the batched
`preload(author().subreddit())` for that page of posts (two `id = ANY(...)`
belongs-to lookups, `posts.rs:176-179`), the "Top posts by votes" leaderboard
(`VoteRepository::sum_value_grouped_by_post_id().order_by_aggregate_desc().limit(5)`,
`examples/reddit-clone/src/repositories.rs:86-90`), and a small `id = ANY(...)`
title lookup for the leaderboard's 5 winners.

Profiled against a production-shaped fixture applied to a real Postgres 16
instance (schema replayed from `examples/reddit-clone/migrations/`, no
Docker available in this environment — `pg_stat_statements` loaded via
`shared_preload_libraries` on a local `postgresql@16` cluster instead of a
testcontainer): 20,000 users, 50 subreddits, 30,000 posts, 15,000 comments,
378,446 votes with real power-law skew (200 "hot" posts, ids 1-200, absorb
heavy volume from up to 300,000 distinct-user votes; the other 29,800 "cold"
posts each get 0-3 organic votes from distinct random users, including
plenty of posts with zero — 333,448 post-directed votes, 44,998
comment-directed votes with `NULL post_id`, so the leaderboard's
`IS NOT NULL` group guard has real rows to exclude, not a vacuous
predicate). `setseed()` makes the fixture fully deterministic — the same
seed produces the same rows, and therefore the same leaderboard winners,
on every run; the harness reads those winners back out of the data rather
than hard-coding ids. ~5% of existing post votes had their value flipped
after the bulk load (the same mutation `Post::react()` performs on a
changed vote) to produce real dead tuples, then `VACUUM` (not `FULL`) +
`ANALYZE` models the steady state autovacuum reaches on a live table,
rather than a pristine just-loaded one.

Reproduce (no Docker required — points at any reachable Postgres 16):

`CREATE EXTENSION pg_stat_statements` alone is **not** sufficient: the
extension's stats-collection hooks only run when the module is loaded via
`shared_preload_libraries`, which requires a `postgresql.conf` edit and a
server restart *before* creating the extension. Without it, the fixture and
query scripts below fail at `pg_stat_statements_reset()` /
`SELECT ... FROM pg_stat_statements` — which is why both now start with
`\set ON_ERROR_STOP on`, so a missing preload stops the script loudly
instead of finishing silently with no measurements collected.

```sh
# One-time server setup (skip if pg_stat_statements is already preloaded):
echo "shared_preload_libraries = 'pg_stat_statements'" >> /etc/postgresql/16/main/postgresql.conf
echo "pg_stat_statements.track = all" >> /etc/postgresql/16/main/postgresql.conf
service postgresql restart   # or: pg_ctl restart / your platform's equivalent

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

`pg_stat_statements`, reset immediately before the five front-page
statements ran once each (the two `array_agg`-wrapped queries that recover
the seeded fixture's actual hot-post ids and leaderboard winners run
*before* the reset, so they never appear in this profile — they are not
statements `front_page` itself issues):

| statement | calls | total buffers | % of page's buffers |
|---|---:|---:|---:|
| leaderboard: `SUM(value) GROUP BY post_id ... LIMIT 5` | 1 | **3,293** | **96.57%** |
| preload: `SELECT * FROM users WHERE id = ANY(...)` | 1 | 77 | 2.26% |
| hot-posts listing: `ORDER BY hot_rank DESC LIMIT 25` | 1 | 27 | 0.79% |
| title lookup: `id = ANY(...)` (leaderboard winners) | 1 | 12 | 0.35% |
| preload: `SELECT * FROM subreddits WHERE id = ANY(...)` | 1 | 1 | 0.03% |

The leaderboard query is the front page's cost by nearly two orders of
magnitude — comfortably clears the "≥5% of total buffers" bar. The other
four statements are cheap, correctly-indexed point/batched lookups
(`idx_posts_hot_rank`, primary-key `ANY`) and are not touched by this
report. (The two `preload()` statements are approximated from the schema —
plain `belongs_to`, no soft-delete/tenant guard on either `users` or
`subreddits` — not verified byte-identical against the preload macro's
codegen the way the leaderboard query is, below.)

## 🧭 Plan

`EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS)` (full output in
`baseline/output.txt`): `Limit -> Sort (top-N heapsort) -> Finalize
HashAggregate -> Gather (2 workers) -> Partial HashAggregate -> Parallel Seq
Scan on votes, Filter: (votes.post_id IS NOT NULL)`. `Rows Removed by
Filter: 14999` against `111,149` rows returned per worker-loop — the
`post_id IS NOT NULL` predicate matches 88.1% of the table (333,448 of
378,446 rows), so in this fixture's vote mix this is a near-full-table
aggregate, not a selective lookup. That 88.1% is a property of this
fixture's post-vote-to-comment-vote ratio, not a schema guarantee — `votes`
permits either target, and nothing enforces this proportion in production.
A deployment where comment votes are a much larger share of the table would
see a more selective guard, and the conclusion below should be re-measured
against real data before assuming it still holds.

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

**The planner did not use it.** Buffers after adding the index: 3,293 —
identical to baseline's 3,293 (`after/output.txt`), and
`pg_stat_user_indexes.idx_scan` for `idx_votes_post_id_value_covering` is
**0**. `EXPLAIN` confirms the plan is unchanged: still `Parallel Seq Scan on
votes`.

Forcing the issue (`SET enable_seqscan = off` — diagnostic only, never
shipped, not a fix per this repo's own banned-changes list) does make
Postgres pick `Parallel Index Only Scan using
idx_votes_post_id_value_covering`, `Heap Fetches: 0`, and its buffer count
*is* genuinely lower — `shared hit=3 read=1280` = 1,283 total, a 61.0%
reduction versus the seq-scan plan's 3,293. So the index isn't inert.

To be precise about what that does and doesn't establish: `after/output.txt`
also records `Execution Time:` for both plans (lines 87 and 131) —
42.511 ms unforced (seq scan) versus 41.774 ms forced (index-only) in the
version of this run committed here, essentially a tie; an earlier run of
the same script (before an unrelated fixture fix that doesn't touch this
query) recorded a starker 43.905 ms vs. 30.560 ms. Neither delta clears
this project's own admissibility bar for `EXPLAIN ANALYZE` timings (`>2×`,
this repo's rule for when `actual time=`/`Execution Time:` counts as
evidence at all) — and the fact that the gap swings between "roughly tied"
and "index 30% faster" across two otherwise-identical runs is itself the
reason that bar exists. So wall-clock isn't used as a claim here either
way, and the fair description of what happened is **"the planner doesn't
select this index," not "the planner is right not to"** or "the seq scan is
faster." Postgres's cost model weighs `random_page_cost` against
`seq_page_cost` and judged the seq scan cheaper at 88.1% selectivity;
buffers say the index path touches less. Either way, the planner does not
choose it, so shipping it collects none of that buffer win in practice: an
index nothing ever chooses is a pure write tax on every vote insert,
forever, for a benefit this workload never actually gets in return.

The index was **dropped** after the comparison (`after/output.txt`'s final
`DROP INDEX` + the empty-of-it `pg_stat_user_indexes` listing that follows
it). Nothing in this repository's runtime behavior is changed by this
report — only the report itself and a doc-comment cross-reference in
`examples/reddit-clone/src/repositories.rs` are added.

## 📊 Measurement

| | total buffers (hit+read) | Δ vs baseline | `idx_scan` | plan |
|---|---:|---:|---:|---|
| baseline (no covering index) | 3,293 | — | n/a | Parallel Seq Scan |
| after (covering index present, unforced) | 3,293 | **0%** | **0** | Parallel Seq Scan (unchanged) |
| after, forced (`enable_seqscan=off`, diagnostic only) | 1,283 | -61.0% | n/a | Parallel Index Only Scan, Heap Fetches: 0 |

Tool: `pg_stat_statements` (`shared_blks_hit + shared_blks_read`) and
`pg_stat_user_indexes.idx_scan`, both read within the same `psql` session as
the query they measure.

This does **not** clear the impact floor: the shipped state (no forcing) is
a 0% buffer change, and the index that would unlock the forced 61.0% is
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
concrete run's row counts — reproducible byte-for-byte thanks to
`setseed()`).

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

## Revision note

The first version of this report and fixture had three defects, caught in
review:

1. The profile omitted `front_page`'s `preload()` statements, so the
   reported percentage was of an incomplete subset of the page's buffers,
   not the whole page. Fixed — `preload()`'s two statements are now in the
   profile (they're a combined 2.29% of page buffers, not enough to change
   the conclusion).
2. The title-lookup query's `id = ANY(...)` literal was hand-copied from
   one fixture run and went stale the moment the (then-unseeded) fixture
   regenerated with different random data, so it no longer exercised the
   real leaderboard-winner ids. Fixed two ways: the fixture is now
   `setseed()`-deterministic, and the harness reads the actual winners back
   out of the data (`\gset`) instead of hard-coding them, so this can't
   drift again regardless of future fixture changes.
3. The "long tail" vote generator drew 400,000 uniform `(user, post)` pairs
   over 30,000 posts (~13.3 votes/post on average, not the documented 0-3),
   because `generate_series(1, floor(random() * 4)::int)` inside a
   `LATERAL` join doesn't force per-outer-row evaluation when its argument
   doesn't reference an outer column — Postgres decorrelates it and
   evaluates the random bound *once* for the whole join, applying that
   single draw to every post. Fixed by materializing the per-post count in
   its own subquery column first, so the `LATERAL` genuinely references
   `pc.n_votes` per row and can't be hoisted; the fixture now produces the
   documented 0-3-per-cold-post long tail (see `fixture/seed.sql`'s
   comment for the mechanism). The corrected, smaller `votes` table
   (378,446 rows vs. the first version's 733,609) changes the absolute
   buffer counts above but not the conclusion — the leaderboard is still
   ~97% of page buffers, still near-full-table by selectivity, and the
   covering index is still never chosen by the planner.

A second review round on the fix caught three more:

4. `setseed()` doesn't make the churn `UPDATE`'s `TABLESAMPLE BERNOULLI (5)`
   reproducible — `TABLESAMPLE` has its own RNG, only pinned by an explicit
   `REPEATABLE (...)` seed. Fixed: `fixture/seed.sql`'s churn step now uses
   `TABLESAMPLE BERNOULLI (5) REPEATABLE (4152)`.
5. The reproduce commands didn't mention that `pg_stat_statements` has to be
   in `shared_preload_libraries` (a `postgresql.conf` edit + restart) before
   `CREATE EXTENSION` does anything useful — without it the profile queries
   fail, and `psql` would previously keep going past the failure and finish
   looking successful with no measurements collected. Fixed: the "Reproduce"
   section above now states the prerequisite and the setup commands, and all
   three `.sql` scripts start with `\set ON_ERROR_STOP on`.
6. The "🔧 Change" section asserted Postgres's plan choice was "right," which
   overreached — this run's own `after/output.txt` shows the *forced*
   index-only plan finishing faster (30.560 ms vs. 43.905 ms), a direction
   that agrees with the buffer evidence but doesn't clear this project's own
   `>2×` bar for treating `EXPLAIN ANALYZE` timing as evidence at all. Fixed:
   the section now says only what the measurements support — the planner
   doesn't select the index, so shipping it collects none of its buffer win
   in practice — without characterizing that choice as correct or the
   seq-scan plan as faster.
