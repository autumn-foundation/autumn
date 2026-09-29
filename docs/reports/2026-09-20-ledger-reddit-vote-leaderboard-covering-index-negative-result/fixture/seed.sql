-- Production-shaped fixture for reddit-clone's front-page vote leaderboard
-- (`VoteRepository::sum_value_grouped_by_post_id`, examples/reddit-clone/src/repositories.rs:89).
--
-- Scale: 20,000 users, 50 subreddits, 30,000 posts, ~15,000 comments (so
-- comment votes with NULL post_id are genuinely present -- the leaderboard's
-- `IS NOT NULL` guard has real rows to exclude, not a vacuous predicate).
--
-- Vote shape: a two-tier distribution, not a smooth power law -- the
-- 29,800 "cold" posts each independently get 0-3 organic votes from
-- distinct random users (one `generate_series(1, 0..3)` per post -- 0 is a
-- real, common outcome, not clamped away), while 200 "hot" posts (ids
-- 1-200) separately absorb heavy, roughly-uniform-among-themselves volume
-- from up to 300,000 distinct-user votes drawn uniformly across just those
-- 200 ids. That's a real cardinality gap between the two tiers (not a
-- Zipfian rank-frequency curve within either tier), which is what this
-- report's conclusion actually depends on: total row count and the
-- post-vote/comment-vote split, not the shape of vote concentration among
-- individual hot posts. ~40,000 comment votes.
--
-- `setseed()` (below) drives which users/posts/comments get drawn into a
-- vote pair; every value derived from a pair afterward (a vote's `value`,
-- and which votes get churned) is computed with `hashtext()` on the pair's
-- own logical key rather than `random()`, so it can't drift with Postgres's
-- row-order or physical-layout choices the way a naive `random()`-per-row
-- or `TABLESAMPLE`-per-block draw could. Together these make every
-- *randomized value* this script produces deterministic -- who voted on
-- what, with which value, and therefore the leaderboard's actual top-5
-- winners, which the title-lookup query depends on being reproducible.
-- They do **not** make the complete rows or this script's raw output
-- byte-for-byte across runs: every table here defaults `created_at` to
-- `NOW()`, which the inserts below don't override.
--
-- ~5% of existing post votes get their value flipped after the bulk load
-- (a changed vote, exactly what `react()` does in production) to produce
-- real dead tuples, then VACUUM + ANALYZE models the steady state
-- autovacuum reaches in a live table -- not a pristine just-loaded one.
--
-- Prerequisite: `pg_stat_statements` must be in the server's
-- `shared_preload_libraries` (a `postgresql.conf` change + restart --
-- `CREATE EXTENSION` alone is not enough) before running this fixture and
-- ../baseline/queries.sql / ../after/queries.sql. See the README's
-- "Reproduce" section for the exact commands. This also requires the
-- framework's `autumn_feature_flags` table (migration
-- `20260530200000_create_feature_flags`) applied first -- `front_page`
-- calls `flags.enabled("new_ui_preview")`
-- (examples/reddit-clone/src/routes/posts.rs:195), and with a real primary
-- database configured that resolves to `PgFlagStore`
-- (autumn/src/feature_flags.rs:688-706), a cold 1-second cache issues
-- `SELECT ... FROM autumn_feature_flags WHERE key = $1` -- a sixth
-- statement this profile has to account for. Same story for the
-- `autumn_runtime_config_values` table (migration
-- `20260530000000_create_runtime_config`): `posts_per_page()`
-- (posts.rs:47-52) reads the `posts_per_page` config key via
-- `config_svc()`, which resolves to `PgConfigStore`
-- (examples/reddit-clone/src/lib.rs:32-49) with the same 1-second-cache
-- shape (autumn/src/runtime_config.rs:1130-1158) -- a seventh statement.

\set ON_ERROR_STOP on

SELECT setseed(0.4152);

-- Matches the app's own bootstrap default (examples/reddit-clone/src/feature_flags.rs:30-32):
-- new_ui_preview at 25% rollout.
INSERT INTO autumn_feature_flags (key, description, enabled, rollout_pct)
VALUES ('new_ui_preview', 'Shows the "New UI" banner to early testers', true, 25);

INSERT INTO users (username, password_hash)
SELECT 'user_' || n, 'x' FROM generate_series(1, 20000) AS n;

INSERT INTO subreddits (name, slug, creator_id)
SELECT 'sub' || n, 'sub' || n, 1 FROM generate_series(1, 50) AS n;

INSERT INTO posts (title, slug, author_id, subreddit_id, hot_rank)
SELECT 'Post ' || n, 'post-' || n,
       (1 + floor(random() * 20000))::bigint,
       (1 + floor(random() * 50))::bigint,
       random() * 1000
FROM generate_series(1, 30000) AS n;

INSERT INTO comments (body, author_id, commentable_type, commentable_id)
SELECT 'comment ' || n, (1 + floor(random() * 20000))::bigint,
       'Post', (1 + floor(random() * 30000))::bigint
FROM generate_series(1, 15000) AS n;

-- Cold posts (ids 201-30000): each gets 0-3 organic votes from distinct
-- random users -- a real long tail, including posts with zero votes.
--
-- The per-post vote count is materialized as a column (`pc.n_votes`) in its
-- own subquery *before* the LATERAL join, not computed inline as
-- `generate_series(1, floor(random() * 4)::int)`. Postgres only forces
-- per-outer-row re-evaluation of a LATERAL right-hand side when it actually
-- references an outer column; `floor(random() * 4)::int` written inline
-- doesn't reference `p.*`, so the planner treats it as uncorrelated and
-- evaluates it exactly *once* for the whole join, applying one random count
-- to every post. Referencing `pc.n_votes` is a real correlation, so it
-- can't be hoisted, and each post gets its own draw.
-- `DISTINCT ON (u, p)` without an `ORDER BY` was dropped: Postgres documents
-- that as picking an unpredictable row among ties, and the hot-post draw
-- below produces plenty of duplicate (u, p) pairs, each with its own
-- independent `random()` call for `value` -- the deduped-away rows and the
-- surviving one could disagree, and which one survives is undefined. The
-- pairs are deduped FIRST with a plain `SELECT DISTINCT u, p` (unambiguous:
-- a duplicate row is identical to its sibling here, there's no extra column
-- to arbitrate between).
--
-- `value` is computed from `hashtext(...)`, not `random()`. A second
-- determinism gap remains even once the pair is deduped first: which
-- `random()` *call* in the session's seeded sequence a given pair receives
-- depends on the row order `SELECT DISTINCT` happens to emit them in, and
-- that order is not part of the SQL standard's or Postgres's contract for
-- `DISTINCT` -- a different `DISTINCT` strategy (hash- vs. sort-based
-- unique), worker count, or `work_mem` could feed the same pair a
-- different draw and flip its vote, even though every pair still gets
-- exactly one draw. `hashtext` makes `value` a pure function of the pair
-- itself (plus a per-block salt, so cold/hot/comment votes don't share a
-- pattern), so no evaluation order can change which value a pair gets.
-- `hashtext` can return `INT_MIN`, which `abs()` on `int` can't represent
-- (overflows); casting to `bigint` first avoids that.
INSERT INTO votes (user_id, post_id, value)
SELECT u, p,
       (CASE WHEN (abs(hashtext(u::text || ':' || p::text || ':cold')::bigint) % 100) < 85
             THEN 1 ELSE -1 END)::smallint
FROM (
    SELECT DISTINCT u, p
    FROM (
        SELECT (1 + floor(random() * 20000))::bigint AS u, pc.id AS p
        FROM (
            SELECT p.id, floor(random() * 4)::int AS n_votes
            FROM posts p WHERE p.id > 200
        ) pc
        CROSS JOIN LATERAL generate_series(1, pc.n_votes) AS vote_n
    ) t(u, p)
) dedup(u, p)
ON CONFLICT (user_id, post_id) DO NOTHING;

-- Hot posts (ids 1-200): heavy additional vote volume (real skew). Same
-- dedupe-then-draw shape and hash-based `value` as the cold-post insert
-- above, for the same reason.
INSERT INTO votes (user_id, post_id, value)
SELECT u, p,
       (CASE WHEN (abs(hashtext(u::text || ':' || p::text || ':hot')::bigint) % 100) < 90
             THEN 1 ELSE -1 END)::smallint
FROM (
    SELECT DISTINCT u, p
    FROM (
        SELECT (1 + floor(random() * 20000))::bigint AS u,
               (1 + floor(random() * 200))::bigint AS p
        FROM generate_series(1, 300000)
    ) t(u, p)
) dedup(u, p)
ON CONFLICT (user_id, post_id) DO NOTHING;

-- Comment votes: NULL post_id, real rows for the leaderboard's guard to
-- exclude. Same dedupe-then-draw shape and hash-based `value` as the two
-- vote inserts above.
INSERT INTO votes (user_id, comment_id, value)
SELECT u, c,
       (CASE WHEN (abs(hashtext(u::text || ':' || c::text || ':comment')::bigint) % 100) < 80
             THEN 1 ELSE -1 END)::smallint
FROM (
    SELECT DISTINCT u, c
    FROM (
        SELECT (1 + floor(random() * 20000))::bigint AS u,
               (1 + floor(random() * 15000))::bigint AS c
        FROM generate_series(1, 45000)
    ) t(u, c)
) dedup(u, c)
ON CONFLICT (user_id, comment_id) DO NOTHING;

-- Realistic churn: ~5% of existing post votes get their value flipped, as
-- `react()` does on a changed vote -- real dead tuples before VACUUM.
--
-- Originally `id IN (SELECT id FROM votes TABLESAMPLE BERNOULLI (5)
-- REPEATABLE (4152))`: reproducible in the sense that a given seed samples
-- the same *physical* tuples every time, but that's the wrong invariant
-- once the vote inserts above only guarantee their logical (user_id,
-- post_id) pairs and values, not the row order they're inserted in (i.e.
-- their physical heap position). A different `work_mem` or `DISTINCT` plan
-- upstream changes which logical vote lands at which physical position, so
-- "the same physical tuples" silently becomes "different votes flipped."
-- Selecting on `hashtext(user_id, post_id, salt)` instead makes churn a
-- pure function of the vote's own logical key, with no dependency on
-- physical layout or insertion order -- the same fix pattern as `value`'s
-- `hashtext()` above. Deliberately *not* `hashtext(id, ...)`: `id` is a
-- surrogate assigned in insertion order by the sequence, so it inherits
-- the exact same order-dependency this fix exists to remove.
UPDATE votes SET value = -value
WHERE post_id IS NOT NULL
  AND (abs(hashtext(user_id::text || ':' || post_id::text || ':churn')::bigint) % 100) < 5;

VACUUM (VERBOSE) votes;
ANALYZE users, subreddits, posts, comments, votes;

SELECT
  (SELECT count(*) FROM posts) AS posts,
  (SELECT count(*) FROM comments) AS comments,
  (SELECT count(*) FROM votes) AS total_votes,
  (SELECT count(*) FROM votes WHERE post_id IS NOT NULL) AS post_votes,
  (SELECT count(*) FROM votes WHERE comment_id IS NOT NULL) AS comment_votes,
  (SELECT count(*) FROM posts p WHERE p.id > 200
     AND NOT EXISTS (SELECT 1 FROM votes v WHERE v.post_id = p.id)) AS cold_posts_with_zero_votes,
  (SELECT round(avg(c), 2) FROM (
     SELECT count(*) AS c FROM votes WHERE post_id > 200 GROUP BY post_id
   ) t) AS avg_votes_per_cold_post_with_at_least_one;
