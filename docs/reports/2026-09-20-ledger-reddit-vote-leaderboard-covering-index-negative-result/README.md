# 🗃️ Ledger: reddit-clone front-page vote leaderboard — covering-index negative result

## 🎯 Workload

`examples/reddit-clone`'s front page (`GET /`, `routes::posts::front_page`,
`examples/reddit-clone/src/routes/posts.rs:47-195`) issues seven statements:
the hot-posts listing (`ORDER BY hot_rank DESC LIMIT 25`), the batched
`preload(author().subreddit())` for that page of posts (two `id = ANY(...)`
belongs-to lookups, `posts.rs:176-179`), the "Top posts by votes" leaderboard
(`VoteRepository::sum_value_grouped_by_post_id().order_by_aggregate_desc().limit(5)`,
`examples/reddit-clone/src/repositories.rs:86-90`), a small `id = ANY(...)`
title lookup for the leaderboard's 5 winners, and — with a real primary
database configured, so both `PgFlagStore` and `PgConfigStore` are live
instead of their in-memory fallbacks, and a cold 1-second cache — two
config-backed lookups: `flags.enabled("new_ui_preview")` (`posts.rs:195`)
and `posts_per_page()`'s runtime-config read (`posts.rs:47-52`, which feeds
statement 1's `LIMIT`).

Profiled against a production-shaped fixture applied to a real Postgres 16
instance (schema replayed from `examples/reddit-clone/migrations/`, no
Docker available in this environment — `pg_stat_statements` loaded via
`shared_preload_libraries` on a local `postgresql@16` cluster instead of a
testcontainer): 20,000 users, 50 subreddits, 30,000 posts, 15,000 comments,
378,300 votes with a real two-tier cardinality gap (not a smooth power-law
curve — 200 "hot" posts, ids 1-200, absorb heavy, roughly-uniform-among-
themselves volume from up to 300,000 distinct-user votes; the other 29,800
"cold" posts each independently get 0-3 organic votes from distinct random
users, including plenty of posts with zero — 333,302 post-directed votes, 44,998
comment-directed votes with `NULL post_id`, so the leaderboard's
`IS NOT NULL` group guard has real rows to exclude, not a vacuous
predicate). `setseed()` (which drives which users/posts/comments get drawn
into a pair) plus `hashtext()` on each pair's own logical key (which drives
a vote's `value` and whether it gets churned, so it can't drift with
Postgres's row-order or physical-layout choices) makes every *randomized*
value — which user voted on what, with which value, who authored which
post — deterministic, and therefore makes the leaderboard's winners
deterministic too; the harness reads those
winners back out of the data rather than hard-coding ids, so it can't drift
regardless. That does **not** make the raw committed output byte-identical
across runs: `created_at` columns default to `NOW()`, and `EXPLAIN`/`VACUUM`
output carries wall-clock time, XIDs and I/O counts that vary run to run by
nature — only the row *values* that matter to this report (vote targets,
values, and the winners they produce) are pinned. ~5% of existing post votes had their value flipped
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
# One-time server setup (skip if pg_stat_statements is already preloaded).
# `postgresql.conf` only honors the LAST `shared_preload_libraries` line it
# finds, so blindly appending a new one -- rather than editing the existing
# line -- silently drops whatever was already preloaded (pgaudit,
# auto_explain, ...) on the next restart. On a server that doesn't already
# set this (a throwaway/local cluster, as used for this report), appending
# is fine:
echo "shared_preload_libraries = 'pg_stat_statements'" >> /etc/postgresql/16/main/postgresql.conf
# On a server that already sets shared_preload_libraries, edit that
# existing line instead, e.g.:
#   shared_preload_libraries = 'pgaudit,pg_stat_statements'
echo "pg_stat_statements.track = all" >> /etc/postgresql/16/main/postgresql.conf
service postgresql restart   # or: pg_ctl restart / your platform's equivalent

createdb reddit_ledger
psql -d reddit_ledger -c 'CREATE EXTENSION IF NOT EXISTS pg_stat_statements;'
for m in 20260419000000_create_reddit 20260427000000_add_user_avatar \
         20260702000001_create_tags 20260820000000_polymorphic_comments; do
  psql -d reddit_ledger -f examples/reddit-clone/migrations/$m/up.sql
done
# Framework migrations (autumn/migrations/, not the app's own) -- front_page's
# flags.enabled("new_ui_preview") and posts_per_page() calls need these
# tables when a real primary database is configured (see Profile section,
# statements 6 and 7).
psql -d reddit_ledger -f autumn/migrations/20260530200000_create_feature_flags/up.sql
psql -d reddit_ledger -f autumn/migrations/20260530000000_create_runtime_config/up.sql
psql -d reddit_ledger -f docs/reports/2026-09-20-ledger-reddit-vote-leaderboard-covering-index-negative-result/fixture/seed.sql
psql -d reddit_ledger -f docs/reports/2026-09-20-ledger-reddit-vote-leaderboard-covering-index-negative-result/baseline/queries.sql
psql -d reddit_ledger -f docs/reports/2026-09-20-ledger-reddit-vote-leaderboard-covering-index-negative-result/after/queries.sql
```

## 📈 Profile

`pg_stat_statements`, reset immediately before the seven front-page
statements ran once each (the two `array_agg`-wrapped queries that recover
the seeded fixture's actual hot-post ids and leaderboard winners run
*before* the reset, so they never appear in this profile — they are not
statements `front_page` itself issues):

| statement | calls | total buffers | % of page's buffers |
|---|---:|---:|---:|
| leaderboard: `SUM(value) GROUP BY post_id ... LIMIT 5` | 1 | **3,292** | **96.43%** |
| preload: `SELECT * FROM users WHERE id = ANY(...)` | 1 | 77 | 2.26% |
| hot-posts listing: `ORDER BY hot_rank DESC LIMIT 25` | 1 | 27 | 0.79% |
| title lookup: `id = ANY(...)` (leaderboard winners) | 1 | 13 | 0.38% |
| flag lookup: `autumn_feature_flags WHERE key = $1` | 1 | 2 | 0.06% |
| runtime-config lookup: `autumn_runtime_config_values WHERE key = $1` | 1 | 2 | 0.06% |
| preload: `SELECT * FROM subreddits WHERE id = ANY(...)` | 1 | 1 | 0.03% |

The leaderboard query is the front page's cost by nearly two orders of
magnitude — comfortably clears the "≥5% of total buffers" bar. The other
six statements are cheap, correctly-indexed point/batched lookups
(`idx_posts_hot_rank`, primary-key `ANY`, the flag/config tables' unique
`key` indexes) and are not touched by this report. (The two `preload()`
statements are approximated from the schema — plain `belongs_to`, no
soft-delete/tenant guard on either `users` or `subreddits` — not verified
byte-identical against the preload macro's codegen the way the leaderboard
query is, below. The flag and runtime-config lookups also assume a cold
1-second cache — a request landing within a second of a prior one for
either key skips that lookup entirely.)

## 🧭 Plan

`EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS)` (full output in
`baseline/output.txt`): `Limit -> Sort (top-N heapsort) -> Finalize
HashAggregate -> Gather (2 workers) -> Partial HashAggregate -> Parallel Seq
Scan on votes, Filter: (votes.post_id IS NOT NULL)`. `Rows Removed by
Filter: 14999` against `111,101` rows returned per worker-loop — the
`post_id IS NOT NULL` predicate matches 88.1% of the table (333,302 of
378,300 rows), so in this fixture's vote mix this is a near-full-table
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

**The planner did not use it.** Buffers after adding the index: 3,292 —
identical to baseline's 3,292 (`after/output.txt`), and
`pg_stat_user_indexes.idx_scan` for `idx_votes_post_id_value_covering` is
**0**. `EXPLAIN` confirms the plan is unchanged: still `Parallel Seq Scan on
votes`.

Forcing the issue (`SET enable_seqscan = off` — diagnostic only, never
shipped, not a fix per this repo's own banned-changes list) does make
Postgres pick `Parallel Index Only Scan using
idx_votes_post_id_value_covering`, `Heap Fetches: 0`, and its buffer count
*is* genuinely lower — `shared hit=3 read=1280` = 1,283 total, a 61.0%
reduction versus the seq-scan plan's 3,292. So the index isn't inert.

To be precise about what that does and doesn't establish: `after/output.txt`
also records `Execution Time:` for both plans (lines 90 and 134) — in the
exact output committed here, 42.673 ms unforced (seq scan) versus 32.617 ms
forced (index-only). Re-running this identical script (or an equivalent
version of it) several times during this report's review produced 43.9/30.6
ms, 42.5/41.8 ms (essentially a tie), 44.6/30.7 ms, 44.1/29.6 ms, 50.4/35.2
ms, 42.7/29.7 ms, 37.0/29.8 ms, 39.4/29.8 ms, 45.4/31.9 ms, and 42.7/32.6 ms — the gap between the two plans swings
from "roughly tied" to "index ~30% faster" across otherwise-identical runs,
which is
itself the reason this project gates `EXPLAIN ANALYZE` timing on a `>2×`
delta before treating it as evidence at all: none of these three runs clear
it. So wall-clock isn't used as a claim here either way — whatever number
`after/output.txt` happens to show on any given run, re-running this script
can and does produce a different one, and that instability is the point,
not a data point to explain away. The fair description of what happened is
**"the planner doesn't select this index," not "the planner is right not
to"** or "the seq scan is faster." Postgres's cost model weighs
`random_page_cost` against
`seq_page_cost` and judged the seq scan cheaper at 88.1% selectivity;
buffers say the index path touches less. Either way, the planner does not
choose it, so shipping it collects none of that buffer win in practice: an
index nothing ever chooses is a pure write tax on every post-directed vote
insert (the partial predicate excludes comment votes, so those pay
nothing), forever, for a benefit this workload never actually gets in
return.

The index was **dropped** after the comparison (`after/output.txt`'s final
`DROP INDEX` + the empty-of-it `pg_stat_user_indexes` listing that follows
it). Nothing in this repository's runtime behavior is changed by this
report — only the report itself and a doc-comment cross-reference in
`examples/reddit-clone/src/repositories.rs` are added.

## 📊 Measurement

| | total buffers (hit+read) | Δ vs baseline | `idx_scan` | plan |
|---|---:|---:|---:|---|
| baseline (no covering index) | 3,292 | — | n/a | Parallel Seq Scan |
| after (covering index present, unforced) | 3,292 | **0%** | **0** | Parallel Seq Scan (unchanged) |
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
concrete run's row counts and ratios — the *randomized values* `setseed()`
and `hashtext()` pin are reproducible; the raw file
itself is not byte-for-byte across runs (`created_at` timestamps, `VACUUM`'s
wall-clock/XID/I/O counters), same caveat as above).

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
   overreached — a run's `after/output.txt` showed the *forced* index-only
   plan finishing faster, a direction that agrees with the buffer evidence
   but doesn't clear this project's own `>2×` bar for treating
   `EXPLAIN ANALYZE` timing as evidence at all (and, as later re-runs
   showed, isn't even a stable direction — see the "🔧 Change" section's own
   discussion of run-to-run variance). Fixed:
   the section now says only what the measurements support — the planner
   doesn't select the index, so shipping it collects none of its buffer win
   in practice — without characterizing that choice as correct or the
   seq-scan plan as faster.

A third review round caught three more:

7. `pg_stat_statements` is cluster-wide: the bare `pg_stat_statements_reset()`
   in `baseline/queries.sql` and `after/queries.sql` clears statistics for
   every database and role on the server, not just this fixture's, which
   would disrupt unrelated monitoring on a shared/reused instance even
   though the reads were already scoped to this database/role. Fixed: both
   scripts now pass `pg_stat_statements_reset(userid, dbid)` scoped to
   `current_user`/`current_database()`.
8. The wall-clock narrative and the fixture-reproducibility claim had gone
   stale against the freshly re-run `after/output.txt` and
   `fixture/seed_output.txt` (this report was re-run several times over the
   course of review, and each re-run changes the exact ms figures and raw
   timestamps those files contain). Fixed: the "🔧 Change" section now cites
   the actual numbers in the currently-committed `after/output.txt` and
   describes the range observed across re-runs instead of a single pair of
   numbers that the next re-run would immediately invalidate; the
   "Reproduce" section's fixture note now says explicitly that the
   *randomized values* are reproducible, not the raw output file
   byte-for-byte.

A fourth review round caught two more, both terminology/scope, no data
change:

9. "Power-law skew" overclaimed the vote generator's actual shape: the 200
   hot posts are drawn *uniformly* across just those 200 ids (so they land
   at roughly equal counts, not a Zipfian rank-frequency decay), and the
   29,800 cold posts each draw independently. That's a real two-tier
   cardinality gap, which is what this report's conclusion depends on
   (total row count and the post-vote/comment-vote split) — but it isn't a
   power law, and calling it one overclaimed the fixture's realism. Fixed:
   `fixture/seed.sql` and this README now say "two-tier," not "power-law."
10. `fixture/seed.sql`'s own header comment still said `setseed()` makes
    "every run of this script produce the exact same rows," which is the
    same overclaim item 8 already fixed in the README but missed in the
    fixture file itself — every table here defaults `created_at` to
    `NOW()`, so complete rows are never identical across runs. Fixed: the
    header now states the same scoped claim (randomized values and
    winners, not complete rows) as the README.

A fifth review round caught two more:

11. `examples/reddit-clone/src/repositories.rs`'s doc-comment had the
    selectivity direction backwards: it said `post_id IS NOT NULL` "was
    selective enough" that the index never got chosen, when a *higher*
    match rate (88.1%) is *lower* selectivity, and lower selectivity is
    exactly why the seq scan wins. Fixed: now says "was NOT selective
    enough."
12. The leaderboard query's `eq`/`low`/`high` guard was written as three
    independent `NULL::bigint` literals, but the real codegen binds them
    as three *reused* parameters (`$1` appears twice in the SQL text for
    the eq guard, `$2` twice for low, `$3` twice for high) — three literal
    constants normalize to 6 distinct `pg_stat_statements` placeholders,
    not matching the shape a real trace of this app would show. Fixed:
    both `queries.sql` scripts now issue the leaderboard query via
    `PREPARE ... EXECUTE ... DEALLOCATE`, which reproduces the real reuse
    structure. Verified this changes only the recorded query *text*, not
    the plan or buffer counts (a freshly prepared statement's first
    execution costs itself with the actual bound values, same as a
    literal query) — re-ran the full pipeline; buffers are unchanged
    (3,293, 96.51%–96.57% depending on which round's total buffer
    denominator this is measured against — see item 13). One residual,
    and irreducible via `psql`, fidelity gap remains: `pg_stat_statements`
    records this statement with a `PREPARE leaderboard_lookup (...) AS`
    prefix that a real driver-level bind (extended query protocol, no
    textual `PREPARE`) wouldn't have; see the comment on the statement in
    `baseline/queries.sql` for why.

A sixth review round caught one more, the largest gap found:

13. The profile was still missing a real statement `front_page` issues:
    `flags.enabled("new_ui_preview")` (`posts.rs:195`), which — with a real
    primary database configured, so `flags` resolves to `PgFlagStore`
    rather than the in-memory fallback — queries `autumn_feature_flags`
    on a cold 1-second cache (`autumn/src/feature_flags.rs:688-706`).
    Fixed: added the framework's `autumn_feature_flags` migration and a
    seeded `new_ui_preview` row (matching the app's own bootstrap default,
    25% rollout) to the fixture, added the query as statement 6, and
    updated the "Reproduce" section with the extra migration. Re-ran the
    full pipeline: the flag lookup is 2 buffers (0.06% of page buffers),
    the leaderboard is now 96.51% of a 6-statement, 3,412-buffer total
    (was 96.57% of a 5-statement, 3,410-buffer total) — the conclusion is
    unaffected, the new statement barely moves the denominator.

A seventh review round caught one more, wording only:

14. The "🔧 Change" section's closing line said the covering index would
    have been "a pure write tax on every vote insert," which overstates it
    — the partial predicate (`WHERE post_id IS NOT NULL`) excludes
    comment-directed votes, matching the "💸 Write cost" section a few
    paragraphs down, which already scoped it correctly. Fixed: both now
    say "every post-directed vote insert."

An eighth review round caught the deepest fidelity gap yet, still no data
change:

15. Even after the parameter-reuse fix (item 12), the leaderboard query
    still wasn't what the codegen actually emits: real Diesel-generated SQL
    double-quotes every identifier (`table_q`/`group_col_q`,
    `autumn-macros-repository/src/repository.rs:14133,14214`), wraps the
    aggregate in `CAST(SUM("value") AS bigint)`
    (`repository.rs:4193-4222` — needed in general so the result
    deserializes into the declared value type, though a no-op here since
    Postgres's `sum(smallint)` already returns `bigint`), and orders by the
    `agg_key` alias, not the raw column (`repository.rs:14371`). The
    harness used unquoted identifiers, no `CAST`, and `post_id ASC`.
    Fixed: both `queries.sql` scripts (the profiled `PREPARE` statement and
    the supplementary `EXPLAIN` illustrations) now match the codegen
    exactly — `"post_id"`, `"votes"`, `"value"`, `CAST(...)`, `agg_key ASC`.
    Re-ran the full pipeline: buffers are unchanged (still 3,293, 96.51%)
    and `idx_scan` is still 0, confirming quoting/casting/tiebreaker-alias
    are cosmetic to the plan (Postgres case-folds unquoted lowercase
    identifiers identically, and `CAST(bigint AS bigint)` is eliminated at
    parse time) — this was a query-text fidelity fix, not a measurement
    fix.

A ninth review round caught one more missing statement:

16. Same class of gap as item 13's flag lookup: `posts_per_page()`
    (`posts.rs:47-52`) reads the `posts_per_page` runtime-config key via
    `config_svc()`, which — with a real primary database configured —
    resolves to `PgConfigStore` (`examples/reddit-clone/src/lib.rs:32-49`)
    with the same 1-second cold-cache shape
    (`autumn/src/runtime_config.rs:1130-1158`). This feeds statement 1's
    `LIMIT` and was missing from the profile. Fixed: added the framework's
    `autumn_runtime_config_values` migration (no seed row — an operator who
    never overrode `posts_per_page` is the common case, and the query still
    executes and is still profiled either way) to the fixture, added the
    lookup as statement 7, and updated the "Reproduce" section's migration
    list. Re-ran the full pipeline: the runtime-config lookup is 2 buffers
    (0.06% of page buffers, same as the flag lookup); the leaderboard is
    now 96.46% of a 7-statement, 3,414-buffer total (was 96.51% of 6) —
    conclusion unaffected.

A tenth review round caught a determinism bug in the fixture, no fixture
scale or ratio change:

17. All three vote-insert statements used `SELECT DISTINCT ON (u, p) u, p,
    (CASE WHEN random() < X THEN 1 ELSE -1 END)::smallint` with no
    `ORDER BY` — Postgres documents `DISTINCT ON` without an `ORDER BY` as
    picking an unpredictable row among ties, and the hot-post draw in
    particular produces many duplicate `(u, p)` pairs, each with its own
    independent `random()` call for `value`. Which duplicate's value
    survives was therefore undefined, meaning the exact vote totals — and
    potentially the leaderboard's top-5 winners — were not actually pinned
    by `setseed()` the way the rest of this report claims, despite every
    other value being deterministic. Fixed: restructured all three inserts
    (cold-post votes, hot-post votes, comment votes) to deduplicate the
    `(u, p)`/`(u, c)` pair first via a plain `SELECT DISTINCT` (unambiguous,
    since a duplicate row is identical to its sibling before `value` is
    computed — there's no extra column to arbitrate between), then draw
    `value` once per already-unique pair in the outer `SELECT`, so there is
    never a second candidate value for the same key to lose track of. Re-ran
    the full pipeline: total votes shifted slightly (378,338 vs. the prior
    round's 378,446 — the old, non-deterministic dedup could keep or drop a
    handful of colliding pairs differently run to run; the ~88.1%
    post-vote/comment-vote split this report's conclusion depends on is
    unchanged) and the leaderboard's winners changed (ids 41/82/102/154/200,
    not the prior round's 82/85/119/146/161 — expected, since the earlier
    winners were never actually pinned). Buffers: leaderboard 3,292 (was
    3,293, a one-page difference from the slightly smaller table), still
    96.40% of page buffers; `idx_scan` for the covering index still 0 both
    before and after adding it; forced index-only plan still 1,283 buffers,
    a 61.0% reduction. Conclusion unaffected — this was a fixture-determinism
    fix, not a measurement fix.

An eleventh review round caught a second, subtler determinism gap in the
same fix:

18. Item 17's dedup-then-draw fix still had a gap: deduplicating the pair
    first makes each `(u, p)`/`(u, c)` pair get exactly one `random()` call,
    but *which* call in the session's seeded sequence a given pair receives
    depends on the row order `SELECT DISTINCT` happens to emit them in —
    and Postgres doesn't guarantee that order. A different `DISTINCT`
    strategy (hash- vs. sort-based unique), a different worker count, or a
    different `work_mem` could feed the same pair a different position in
    the random sequence and flip its vote, even with every pair getting
    exactly one draw. Fixed: `value` is no longer drawn from `random()` at
    all — it's computed as `hashtext(u, p, salt) % 100 < threshold`, a pure
    function of the pair (plus a per-block salt so cold/hot/comment votes
    don't share a pattern), so no evaluation order can change which value a
    pair gets. Verified directly, not just argued: reseeded and re-ran the
    fixture from scratch twice in a row and diffed the leaderboard
    aggregate — both runs produced the exact same 5 `(post_id, sum)` pairs
    (82/1173, 100/1129, 13/1122, 43/1114, 41/1108) and the same total vote
    count (378,300), where before this fix two runs were never guaranteed
    to agree. Re-ran the full pipeline: total votes shifted again (378,300
    vs. item 17's 378,338, same reason — a different value-assignment rule
    redistributes which pairs land on which side of each threshold) but the
    ~88.1% post-vote share is unchanged; leaderboard is still 3,292 buffers
    (96.45% of page buffers), `idx_scan` for the covering index still 0
    before and after adding it, and the forced index-only plan is still
    1,283 buffers, a 61.0% reduction. Conclusion unaffected.

A twelfth review round caught the same class of gap one step further down
the fixture, in the churn step:

19. The churn step selected candidates with `id IN (SELECT id FROM votes
    TABLESAMPLE BERNOULLI (5) REPEATABLE (4152))` — reproducible in the
    sense that a given seed samples the same *physical* tuples every time,
    but that was the wrong invariant once item 18 stopped guaranteeing the
    vote inserts' row order: `TABLESAMPLE` samples by block/tuple position,
    and which logical `(user_id, post_id)` vote lands at a given physical
    position depends on insertion order, which — per item 18 — Postgres
    doesn't guarantee. A different upstream plan could still sample "the
    same physical tuples" while silently churning different votes. Fixed:
    replaced the `TABLESAMPLE`/subquery with
    `(abs(hashtext(user_id::text || ':' || post_id::text || ':churn')::bigint) % 100) < 5`,
    selecting churn candidates by the vote's own logical key instead of its
    physical position — no `TABLESAMPLE`, no `REPEATABLE` seed, no
    dependency on insertion order. (First draft of this fix hashed the
    surrogate `id` column instead — wrong, since `id` is itself assigned in
    insertion order by the table's sequence and so inherits the exact
    dependency being removed; caught before committing and hashed
    `user_id`/`post_id` instead.) Verified directly: reseeded and re-ran
    the fixture from scratch twice more and diffed the leaderboard
    aggregate — both runs again produced the exact same 5 `(post_id, sum)`
    pairs (82/1163, 41/1150, 119/1119, 196/1117, 100/1115) and the same
    total vote count (378,300). Re-ran the full pipeline: leaderboard
    still 3,292 buffers (96.43% of page buffers — the title lookup shifted
    by one buffer, 13 vs. item 18's 12, from the different winners), `idx_scan`
    for the covering index still 0 before and after adding it, forced
    index-only plan still 1,283 buffers, a 61.0% reduction. Conclusion
    unaffected — no more `random()`- or physical-order-dependent value in
    this fixture.
