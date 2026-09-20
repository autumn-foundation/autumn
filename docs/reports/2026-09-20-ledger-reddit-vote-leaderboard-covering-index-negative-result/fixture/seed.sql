-- Production-shaped fixture for reddit-clone's front-page vote leaderboard
-- (`VoteRepository::sum_value_grouped_by_post_id`, examples/reddit-clone/src/repositories.rs:89).
--
-- Scale: 20,000 users, 50 subreddits, 30,000 posts, ~15,000 comments (so
-- comment votes with NULL post_id are genuinely present -- the leaderboard's
-- `IS NOT NULL` guard has real rows to exclude, not a vacuous predicate).
--
-- Vote shape (real power-law skew, not a uniform draw): the 29,800 "cold"
-- posts each get 0-3 organic votes from distinct random users (one
-- `generate_series(1, 0..3)` per post -- 0 is a real, common outcome, not
-- clamped away), while 200 "hot" posts (ids 1-200) separately absorb heavy
-- volume from up to 300,000 distinct-user votes. ~40,000 comment votes.
-- Seeded with `setseed()` so every run of this script produces the exact
-- same rows -- required for the leaderboard's actual top-5 winners (and
-- therefore the title-lookup query that depends on them) to be reproducible.
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
-- "Reproduce" section for the exact commands.

\set ON_ERROR_STOP on

SELECT setseed(0.4152);

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
INSERT INTO votes (user_id, post_id, value)
SELECT DISTINCT ON (u, p) u, p, (CASE WHEN random() < 0.85 THEN 1 ELSE -1 END)::smallint
FROM (
    SELECT (1 + floor(random() * 20000))::bigint AS u, pc.id AS p
    FROM (
        SELECT p.id, floor(random() * 4)::int AS n_votes
        FROM posts p WHERE p.id > 200
    ) pc
    CROSS JOIN LATERAL generate_series(1, pc.n_votes) AS vote_n
) t(u, p)
ON CONFLICT (user_id, post_id) DO NOTHING;

-- Hot posts (ids 1-200): heavy additional vote volume (real skew).
INSERT INTO votes (user_id, post_id, value)
SELECT DISTINCT ON (u, p) u, p, (CASE WHEN random() < 0.9 THEN 1 ELSE -1 END)::smallint
FROM (
    SELECT (1 + floor(random() * 20000))::bigint AS u,
           (1 + floor(random() * 200))::bigint AS p
    FROM generate_series(1, 300000)
) t(u, p)
ON CONFLICT (user_id, post_id) DO NOTHING;

-- Comment votes: NULL post_id, real rows for the leaderboard's guard to exclude.
INSERT INTO votes (user_id, comment_id, value)
SELECT DISTINCT ON (u, c) u, c, (CASE WHEN random() < 0.8 THEN 1 ELSE -1 END)::smallint
FROM (
    SELECT (1 + floor(random() * 20000))::bigint AS u,
           (1 + floor(random() * 15000))::bigint AS c
    FROM generate_series(1, 45000)
) t(u, c)
ON CONFLICT (user_id, comment_id) DO NOTHING;

-- Realistic churn: ~5% of existing post votes get their value flipped, as
-- `react()` does on a changed vote -- real dead tuples before VACUUM.
-- `setseed()` only drives `random()`; `TABLESAMPLE` has its own RNG and is
-- only reproducible with an explicit `REPEATABLE` seed, so it needs one too.
UPDATE votes SET value = -value
WHERE post_id IS NOT NULL
  AND id IN (SELECT id FROM votes TABLESAMPLE BERNOULLI (5) REPEATABLE (4152));

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
