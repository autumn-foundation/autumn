\pset footer off
-- Workload statements: everything on the tick table. The pgbench helper
-- SELECT (it computes the keys the Rust caller computes) is listed, not counted.
CREATE TEMP VIEW w AS
SELECT queryid, left(regexp_replace(query, '\s+', ' ', 'g'), 90) AS query, calls, rows,
       shared_blks_hit + shared_blks_read AS buffers, shared_blks_hit AS hit, shared_blks_read AS read,
       shared_blks_dirtied AS dirtied, temp_blks_written AS temp_w, wal_records, wal_fpi, wal_bytes
FROM pg_stat_statements
WHERE dbid = (SELECT oid FROM pg_database WHERE datname = current_database())
  AND query LIKE '%autumn_scheduler_ticks%' AND query NOT LIKE '%pg_stat%' AND query NOT LIKE 'VACUUM%' AND query NOT LIKE 'ANALYZE%';
\echo -- statements on the tick table, by buffers
SELECT query, calls, rows, buffers, hit, read, dirtied, temp_w, wal_records, wal_fpi, wal_bytes,
       round(100.0 * buffers / nullif(sum(buffers) OVER (), 0), 1) AS pct_buf,
       round(100.0 * calls / nullif(sum(calls) OVER (), 0), 1) AS pct_calls,
       round(100.0 * wal_bytes / nullif(sum(wal_bytes) OVER (), 0), 1) AS pct_wal,
       round(buffers::numeric / nullif(calls, 0), 2) AS buf_per_call
FROM w ORDER BY buffers DESC;
\echo -- totals
SELECT sum(calls) AS calls, sum(buffers) AS buffers, sum(wal_bytes) AS wal_bytes, sum(wal_fpi) AS wal_fpi, sum(temp_w) AS temp_w FROM w;
\echo -- top 10 statements in the database by calls (harness helper included)
SELECT left(regexp_replace(query, '\s+', ' ', 'g'), 70) AS query, calls, shared_blks_hit + shared_blks_read AS buffers
FROM pg_stat_statements WHERE dbid = (SELECT oid FROM pg_database WHERE datname = current_database())
ORDER BY calls DESC LIMIT 10;
\echo -- table
SELECT relname, seq_scan, seq_tup_read, idx_scan, idx_tup_fetch, n_tup_ins, n_tup_del, n_tup_hot_upd, n_live_tup, n_dead_tup
FROM pg_stat_user_tables WHERE relname = 'autumn_scheduler_ticks';
SELECT relname, heap_blks_read + heap_blks_hit AS heap_blks, idx_blks_read + idx_blks_hit AS idx_blks
FROM pg_statio_user_tables WHERE relname = 'autumn_scheduler_ticks';
\echo -- indexes
SELECT indexrelname, idx_scan, idx_tup_read, idx_tup_fetch, pg_relation_size(indexrelid) AS bytes
FROM pg_stat_user_indexes WHERE relname = 'autumn_scheduler_ticks' ORDER BY indexrelname;
\echo -- end state
SELECT count(*) AS live_rows, count(*) FILTER (WHERE expires_at < now()) AS expired_left FROM autumn_scheduler_ticks;
