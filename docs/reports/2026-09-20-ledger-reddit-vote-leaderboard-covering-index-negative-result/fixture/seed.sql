-- Production-shaped fixture for reddit-clone's front-page vote leaderboard
-- (`VoteRepository::sum_value_grouped_by_post_id`, examples/reddit-clone/src/repositories.rs:89).
--
-- Scale: 20,000 users, 50 subreddits, 30,000 posts, ~15,000 comments (so
-- comment votes with NULL post_id are genuinely present -- the leaderboard's
-- `IS NOT NULL` guard has real rows to exclude, not a vacuous predicate),
-- ~690,000 post votes with a power-law skew (200 "hot" posts absorb most of
-- the volume, the rest get a handful each -- real vote-count skew), ~40,000
-- comment votes. ~5% of existing post votes get their value flipped after
-- the bulk load (a changed vote, exactly what `react()` does in production)
-- to produce real dead tuples, then VACUUM + ANALYZE models the steady state
-- autovacuum reaches in a live table -- not a pristine just-loaded one.

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

-- Long tail: every post gets a few "organic" votes from random distinct users.
INSERT INTO votes (user_id, post_id, value)
SELECT DISTINCT ON (u, p) u, p, (CASE WHEN random() < 0.85 THEN 1 ELSE -1 END)::smallint
FROM (
    SELECT (1 + floor(random() * 20000))::bigint AS u,
           (1 + floor(random() * 30000))::bigint AS p
    FROM generate_series(1, 400000)
) t(u, p)
ON CONFLICT (user_id, post_id) DO NOTHING;

-- Hot posts: 200 posts absorb heavy additional vote volume (real skew).
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
UPDATE votes SET value = -value
WHERE post_id IS NOT NULL AND id IN (SELECT id FROM votes TABLESAMPLE BERNOULLI (5));

VACUUM (VERBOSE) votes;
ANALYZE users, subreddits, posts, comments, votes;

SELECT
  (SELECT count(*) FROM posts) AS posts,
  (SELECT count(*) FROM comments) AS comments,
  (SELECT count(*) FROM votes) AS total_votes,
  (SELECT count(*) FROM votes WHERE post_id IS NOT NULL) AS post_votes,
  (SELECT count(*) FROM votes WHERE comment_id IS NOT NULL) AS comment_votes;
