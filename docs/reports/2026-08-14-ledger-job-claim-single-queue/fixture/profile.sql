\pset format aligned
\pset border 2

SELECT 'TOP BY BUFFERS (hit+read)' AS section;
SELECT
  calls,
  round((shared_blks_hit + shared_blks_read)::numeric, 0) AS total_buffers,
  round(temp_blks_written::numeric, 0) AS temp_written,
  left(query, 90) AS query
FROM pg_stat_statements
ORDER BY (shared_blks_hit + shared_blks_read) DESC
LIMIT 10;

SELECT 'TOTAL BUFFERS ACROSS ALL STATEMENTS' AS section;
SELECT sum(shared_blks_hit + shared_blks_read) AS total_buffers, sum(calls) AS total_calls
FROM pg_stat_statements;

SELECT 'TOP BY CALLS' AS section;
SELECT calls, round((shared_blks_hit + shared_blks_read)::numeric, 0) AS total_buffers, left(query, 90) AS query
FROM pg_stat_statements
ORDER BY calls DESC
LIMIT 10;
