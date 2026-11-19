\echo '--- pg_stat_statements (workload; harness SELECTs excluded) ---'
SELECT calls, shared_blks_hit + shared_blks_read AS buffers, shared_blks_hit AS hit,
       shared_blks_read AS read, rows, temp_blks_written, wal_bytes,
       round(100.0 * (shared_blks_hit + shared_blks_read)
             / nullif(sum(shared_blks_hit + shared_blks_read) OVER (), 0), 1) AS pct_buffers,
       round(100.0 * calls / sum(calls) OVER (), 1) AS pct_calls,
       CASE WHEN query LIKE '%WHERE status IN ($%) AND claimed_at < NOW()%' THEN 'recover stale pending'
            WHEN query LIKE '%attempt < max_attempts%' THEN 'recover stale running'
            WHEN query LIKE '%FOR UPDATE SKIP LOCKED%' THEN 'claim'
            WHEN query LIKE 'INSERT%' THEN 'pending insert'
            WHEN query LIKE '%SET context = $1::JSONB%' THEN 'mark after_hook_succeeded'
            WHEN query LIKE '%run_at = NOW(), enqueued_at%' THEN 'finalize'
            WHEN query LIKE '%finished_at = NOW(), context%' THEN 'ack'
            ELSE left(query, 40) END AS stmt
FROM pg_stat_statements
WHERE query NOT ILIKE '%pg_stat%' AND query NOT LIKE 'SELECT CASE WHEN%'
  AND query NOT LIKE 'SELECT pg_current_wal_lsn%'
ORDER BY buffers DESC;
\echo '--- totals ---'
SELECT sum(calls) AS calls, sum(shared_blks_hit + shared_blks_read) AS buffers,
       sum(temp_blks_written) AS temp_blks_written, sum(wal_bytes) AS wal_bytes,
       pg_wal_lsn_diff(pg_current_wal_lsn(), :'lsn0') AS lsn_wal_bytes
FROM pg_stat_statements WHERE query NOT ILIKE '%pg_stat%' AND query NOT LIKE 'SELECT CASE WHEN%'
  AND query NOT LIKE 'SELECT pg_current_wal_lsn%';
\echo '--- pg_stat_user_tables / indexes ---'
SELECT relname, seq_scan, seq_tup_read, idx_scan, idx_tup_fetch, n_tup_ins, n_tup_upd, n_tup_hot_upd, n_live_tup, n_dead_tup
FROM pg_stat_user_tables;
SELECT indexrelname, idx_scan, idx_tup_read, idx_tup_fetch,
       pg_relation_size(indexrelid) / 8192 AS blocks
FROM pg_stat_user_indexes ORDER BY indexrelname;
