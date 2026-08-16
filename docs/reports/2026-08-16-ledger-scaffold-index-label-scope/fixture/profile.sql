-- Top statements from the just-run workload, ranked by buffers touched.
-- Run after `SELECT pg_stat_statements_reset();` + one of workload_before.sql
-- / workload_after.sql.

SELECT
    calls,
    shared_blks_hit + shared_blks_read AS total_buffers,
    rows,
    left(regexp_replace(query, '\s+', ' ', 'g'), 140) AS query_text
FROM pg_stat_statements
WHERE query ILIKE '%posts%' OR query ILIKE '%comments%'
ORDER BY total_buffers DESC
LIMIT 10;
