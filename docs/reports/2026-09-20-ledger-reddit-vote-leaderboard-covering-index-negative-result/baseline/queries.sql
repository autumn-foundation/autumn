-- Requires pg_stat_statements in shared_preload_libraries (see
-- ../README.md's "Reproduce" section) -- without it, pg_stat_statements_reset()
-- below errors and ON_ERROR_STOP stops the script rather than silently
-- continuing past a profile that was never collected.
\set ON_ERROR_STOP on

-- Capture (not profiled -- these run before the reset below, and their
-- array_agg wrapper is a different statement shape than what front_page
-- actually sends, so they must not appear in the profiled window). Needed
-- because the fixture is seeded (not hand-picked ids), so the hot-posts
-- page, the ids `preload()` batches, and the leaderboard's actual top-5
-- winners are only known once the seeded data exists.
SELECT
    array_agg(DISTINCT author_id) AS author_ids,
    array_agg(DISTINCT subreddit_id) AS subreddit_ids
FROM (SELECT author_id, subreddit_id FROM posts ORDER BY hot_rank DESC LIMIT 25) hp \gset hp_

SELECT array_agg(post_id ORDER BY s DESC NULLS LAST, post_id ASC) AS winners
FROM (
    SELECT post_id, SUM(value) AS s FROM votes
    WHERE post_id IS NOT NULL GROUP BY post_id ORDER BY s DESC NULLS LAST, post_id ASC LIMIT 5
) w \gset lb_

SELECT pg_stat_statements_reset();

-- 1. front_page's hot_posts listing
SELECT id, title, slug, body, url, author_id, subreddit_id, score, hot_rank, comment_count, created_at, updated_at
FROM posts ORDER BY hot_rank DESC LIMIT 25;

-- 2 & 3. front_page's `repo.on_primary().preload(hot_posts, Post::preload().author().subreddit())`
-- (posts.rs:176-179) -- the batched belongs_to lookups for the page of posts.
-- Approximated from the schema (author/subreddit are plain belongs_to, no
-- soft-delete/tenant guard on either table) -- not verified byte-identical
-- against the preload macro's codegen the way the leaderboard query below is.
SELECT * FROM users WHERE id = ANY(:'hp_author_ids'::bigint[]);
SELECT * FROM subreddits WHERE id = ANY(:'hp_subreddit_ids'::bigint[]);

-- 4. front_page's top-by-votes leaderboard
-- (byte-identical to the SQL `VoteRepository::sum_value_grouped_by_post_id()
--  .order_by_aggregate_desc().limit(5)` sends -- codegen in
--  autumn-macros-repository/src/repository.rs:14362-14374; verified against
--  the real trait call in examples/reddit-clone/tests/votable_pg_integration.rs:443-449)
SELECT post_id AS agg_key, SUM(value) AS agg_val FROM votes
WHERE post_id IS NOT NULL
  AND (NULL::bigint IS NULL OR post_id = NULL::bigint)
  AND (NULL::bigint IS NULL OR post_id >= NULL::bigint)
  AND (NULL::bigint IS NULL OR post_id <= NULL::bigint)
GROUP BY post_id
ORDER BY agg_val DESC NULLS LAST, post_id ASC
LIMIT 5;

-- 5. front_page's title resolution for the leaderboard's actual top ids
-- (captured above, not hard-coded -- this fixture is seeded but the winners
-- still depend on the full vote distribution, so pinning literal ids here
-- would silently stop matching the data on any fixture change).
SELECT id, title FROM posts WHERE id = ANY(:'lb_winners'::bigint[]);

\echo '--- pg_stat_statements profile (5 front-page statements) ---'
SELECT query, calls, shared_blks_hit, shared_blks_read,
       (shared_blks_hit + shared_blks_read) AS total_buffers,
       round(100.0 * (shared_blks_hit + shared_blks_read) /
             sum(shared_blks_hit + shared_blks_read) OVER (), 2) AS pct_of_page_buffers
FROM pg_stat_statements
WHERE query NOT ILIKE '%pg_stat_statements%'
ORDER BY total_buffers DESC;

\echo '--- EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS) for the leaderboard query ---'
EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS)
SELECT post_id AS agg_key, SUM(value) AS agg_val FROM votes
WHERE post_id IS NOT NULL
GROUP BY post_id
ORDER BY agg_val DESC NULLS LAST, post_id ASC
LIMIT 5;
