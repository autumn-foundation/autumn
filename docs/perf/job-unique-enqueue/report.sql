\echo '--- pg_stat_statements (workload) ---'
SELECT calls, shared_blks_hit + shared_blks_read AS buffers, shared_blks_hit AS hit,
       shared_blks_read AS read, rows, temp_blks_written, wal_bytes,
       round(100.0 * (shared_blks_hit + shared_blks_read)
             / sum(shared_blks_hit + shared_blks_read) OVER (), 1) AS pct_buffers,
       round(100.0 * calls / sum(calls) OVER (), 1) AS pct_calls,
       CASE WHEN query LIKE 'UPDATE%' THEN 'evict (ttl)'
            WHEN query ~ 'CASE WHEN \(\$\d+\)::bigint' THEN 'insert ttl window'
            WHEN query ~ 'WHERE \(\(\$\d+::text\)::TEXT IS NULL' THEN 'insert plain'
            WHEN query LIKE 'INSERT INTO autumn_jobs%' THEN 'insert running/pending window'
            ELSE left(query, 40) END AS stmt
FROM pg_stat_statements
WHERE query NOT ILIKE '%pg_stat%'
ORDER BY buffers DESC;
\echo '--- totals ---'
SELECT sum(calls) AS calls, sum(shared_blks_hit + shared_blks_read) AS buffers,
       sum(temp_blks_written) AS temp_blks_written, sum(wal_bytes) AS wal_bytes,
       pg_wal_lsn_diff(pg_current_wal_lsn(), :'lsn0') AS lsn_wal_bytes
FROM pg_stat_statements WHERE query NOT ILIKE '%pg_stat%';
\echo '--- pg_stat_user_tables / indexes ---'
SELECT relname, seq_scan, seq_tup_read, idx_scan, idx_tup_fetch, n_tup_ins, n_tup_upd
FROM pg_stat_user_tables;
SELECT indexrelname, idx_scan, idx_tup_read, idx_tup_fetch
FROM pg_stat_user_indexes ORDER BY indexrelname;
