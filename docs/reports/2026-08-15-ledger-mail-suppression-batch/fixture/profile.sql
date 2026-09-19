-- Top statements from the just-run workload, ranked by buffers touched.
-- Run after `SELECT pg_stat_statements_reset();` + one of workload_before.sql
-- / workload_after.sql.

SELECT
    calls,
    shared_blks_hit + shared_blks_read AS total_buffers,
    round((shared_blks_hit + shared_blks_read)::numeric / NULLIF(calls, 0), 2) AS buffers_per_call,
    temp_blks_read,
    temp_blks_written,
    left(regexp_replace(query, '\s+', ' ', 'g'), 140) AS query_text
FROM pg_stat_statements
WHERE query ILIKE '%mail_unsubscribes%'
ORDER BY total_buffers DESC
LIMIT 10;
