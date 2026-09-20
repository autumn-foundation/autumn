SELECT pg_stat_statements_reset();

-- 1. front_page's hot_posts listing
SELECT id, title, slug, body, url, author_id, subreddit_id, score, hot_rank, comment_count, created_at, updated_at
FROM posts ORDER BY hot_rank DESC LIMIT 25;

-- 2. front_page's top-by-votes leaderboard
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

-- 3. front_page's title resolution for the leaderboard's top ids
SELECT id, title FROM posts WHERE id = ANY(ARRAY[144,193,186,194,126]);

\echo '--- pg_stat_statements profile ---'
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
