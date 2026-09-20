\echo '=== add the candidate covering partial index ==='
CREATE INDEX idx_votes_post_id_value_covering ON votes (post_id) INCLUDE (value) WHERE post_id IS NOT NULL;
ANALYZE votes;

SELECT pg_stat_statements_reset();

\echo '=== unforced: planner choice with the index available ==='
SELECT post_id AS agg_key, SUM(value) AS agg_val FROM votes
WHERE post_id IS NOT NULL
  AND (NULL::bigint IS NULL OR post_id = NULL::bigint)
  AND (NULL::bigint IS NULL OR post_id >= NULL::bigint)
  AND (NULL::bigint IS NULL OR post_id <= NULL::bigint)
GROUP BY post_id
ORDER BY agg_val DESC NULLS LAST, post_id ASC
LIMIT 5;

\echo '--- pg_stat_statements after the index exists (unforced) ---'
SELECT query, calls, shared_blks_hit, shared_blks_read,
       (shared_blks_hit + shared_blks_read) AS total_buffers
FROM pg_stat_statements
WHERE query ILIKE '%agg_key%'
ORDER BY total_buffers DESC;

\echo '--- is the new index ever used? (idx_scan must be > 0 to keep it) ---'
SELECT indexrelname, idx_scan, idx_tup_read
FROM pg_stat_user_indexes
WHERE relname = 'votes'
ORDER BY indexrelname;

\echo '--- EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS), index present, unforced ---'
EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS)
SELECT post_id AS agg_key, SUM(value) AS agg_val FROM votes
WHERE post_id IS NOT NULL
GROUP BY post_id
ORDER BY agg_val DESC NULLS LAST, post_id ASC
LIMIT 5;

\echo '=== diagnostic only: force the index path to see its true cost (enable_seqscan=off is never shipped) ==='
SET enable_seqscan = off;
EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS)
SELECT post_id AS agg_key, SUM(value) AS agg_val FROM votes
WHERE post_id IS NOT NULL
GROUP BY post_id
ORDER BY agg_val DESC NULLS LAST, post_id ASC
LIMIT 5;
RESET enable_seqscan;

\echo '=== revert: this is a negative result, the index is not shipped ==='
DROP INDEX idx_votes_post_id_value_covering;

\echo '--- confirm no leftover index ---'
SELECT indexrelname FROM pg_stat_user_indexes WHERE relname = 'votes' ORDER BY indexrelname;
