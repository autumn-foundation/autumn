-- Requires pg_stat_statements in shared_preload_libraries (see
-- ../README.md's "Reproduce" section).
\set ON_ERROR_STOP on

\echo '=== add the candidate covering partial index ==='
CREATE INDEX idx_votes_post_id_value_covering ON votes (post_id) INCLUDE (value) WHERE post_id IS NOT NULL;
ANALYZE votes;

-- Scoped reset -- see baseline/queries.sql for why the bare, no-argument
-- form isn't used here.
SELECT pg_stat_statements_reset(
    (SELECT oid FROM pg_roles WHERE rolname = current_user),
    (SELECT oid FROM pg_database WHERE datname = current_database())
);

\echo '=== unforced: planner choice with the index available ==='
-- PREPARE/EXECUTE reproduces the real codegen's parameter-reuse structure
-- (3 bound params, each referenced twice), quoted identifiers, the
-- CAST(SUM(...) AS bigint) aggregate expression, and the agg_key-alias
-- ORDER BY tiebreaker -- see baseline/queries.sql's comment on the same
-- statement for exactly why and where each piece comes from in the codegen.
PREPARE leaderboard_lookup (bigint, bigint, bigint) AS
SELECT "post_id" AS agg_key, CAST(SUM("value") AS bigint) AS agg_val FROM "votes"
WHERE "post_id" IS NOT NULL
  AND ($1 IS NULL OR "post_id" = $1)
  AND ($2 IS NULL OR "post_id" >= $2)
  AND ($3 IS NULL OR "post_id" <= $3)
GROUP BY "post_id"
ORDER BY agg_val DESC NULLS LAST, agg_key ASC
LIMIT 5;
EXECUTE leaderboard_lookup(NULL, NULL, NULL);
DEALLOCATE leaderboard_lookup;

\echo '--- pg_stat_statements after the index exists (unforced) ---'
-- Same dbid/userid scoping as baseline/queries.sql -- pg_stat_statements is
-- cluster-wide, so an unfiltered scan on a shared instance could pick up
-- another database's coincidentally-similar query text.
SELECT query, calls, shared_blks_hit, shared_blks_read,
       (shared_blks_hit + shared_blks_read) AS total_buffers
FROM pg_stat_statements
WHERE query ILIKE '%agg_key%'
  AND dbid = (SELECT oid FROM pg_database WHERE datname = current_database())
  AND userid = (SELECT oid FROM pg_roles WHERE rolname = current_user)
ORDER BY total_buffers DESC;

\echo '--- is the new index ever used? (idx_scan must be > 0 to keep it) ---'
SELECT indexrelname, idx_scan, idx_tup_read
FROM pg_stat_user_indexes
WHERE relname = 'votes'
ORDER BY indexrelname;

\echo '--- EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS), index present, unforced ---'
EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS)
SELECT "post_id" AS agg_key, CAST(SUM("value") AS bigint) AS agg_val FROM "votes"
WHERE "post_id" IS NOT NULL
GROUP BY "post_id"
ORDER BY agg_val DESC NULLS LAST, agg_key ASC
LIMIT 5;

\echo '=== diagnostic only: force the index path to see its true cost (enable_seqscan=off is never shipped) ==='
SET enable_seqscan = off;
EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS)
SELECT "post_id" AS agg_key, CAST(SUM("value") AS bigint) AS agg_val FROM "votes"
WHERE "post_id" IS NOT NULL
GROUP BY "post_id"
ORDER BY agg_val DESC NULLS LAST, agg_key ASC
LIMIT 5;
RESET enable_seqscan;

\echo '=== revert: this is a negative result, the index is not shipped ==='
DROP INDEX idx_votes_post_id_value_covering;

\echo '--- confirm no leftover index ---'
SELECT indexrelname FROM pg_stat_user_indexes WHERE relname = 'votes' ORDER BY indexrelname;
