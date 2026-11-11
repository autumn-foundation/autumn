SELECT pg_sleep(0.6);  -- let stats flush to pg_stat_user_tables
\echo '--- pg_stat_statements (workload) ---'
SELECT calls, shared_blks_hit + shared_blks_read AS buffers, shared_blks_hit AS hit,
       shared_blks_read AS read, rows, temp_blks_written, wal_bytes,
       left(regexp_replace(query, '\s+', ' ', 'g'), 70) AS query
FROM pg_stat_statements
WHERE query NOT ILIKE '%pg_stat%' AND query NOT ILIKE '%AS claimed%'
ORDER BY buffers DESC;
\echo '--- pg_stat_user_tables / indexes ---'
SELECT relname, seq_scan, seq_tup_read, idx_scan, idx_tup_fetch, n_live_tup, n_dead_tup
FROM pg_stat_user_tables;
SELECT indexrelname, idx_scan, idx_tup_read, idx_tup_fetch FROM pg_stat_user_indexes ORDER BY indexrelname;
