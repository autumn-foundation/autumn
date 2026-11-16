-- The post harness profile: pg_stat_statements without the harness's own
-- input-picking statements (/*harness*/), transaction control, or the
-- profile queries. Run with -v lsn0=<pg_current_wal_lsn() before>.
\pset footer off
CREATE TEMP VIEW s AS
SELECT queryid, toplevel, calls, rows,
       shared_blks_hit + shared_blks_read AS buffers,
       shared_blks_hit AS hit, shared_blks_read AS read,
       temp_blks_written AS temp_w, wal_bytes, wal_records, wal_fpi,
       regexp_replace(left(query, 150), '\s+', ' ', 'g') AS query
  FROM pg_stat_statements
 WHERE dbid = (SELECT oid FROM pg_database WHERE datname = current_database())
   AND query NOT LIKE '/*harness*/%'
   AND query !~* '^\s*(BEGIN|END|COMMIT|ROLLBACK)'
   AND query NOT LIKE '%pg_stat%' AND query NOT LIKE '%pg_current_wal_lsn%'
   AND query NOT LIKE '%uuid_send(gen_random_uuid())%';  -- uuid_v7(), the harness's id
\echo === top statements by buffers (shared_blks_hit + shared_blks_read)
SELECT toplevel AS top, calls, rows, buffers, hit, read, temp_w,
       round(100.0 * buffers / NULLIF(sum(buffers) OVER (PARTITION BY toplevel), 0), 1) AS pct_buf,
       round(100.0 * calls / NULLIF(sum(calls) OVER (PARTITION BY toplevel), 0), 1) AS pct_calls,
       round(buffers::numeric / calls, 1) AS buf_per_call, query
  FROM s ORDER BY toplevel DESC, buffers DESC LIMIT 25;
\echo === top statements by calls
SELECT toplevel AS top, calls, buffers, query FROM s ORDER BY calls DESC LIMIT 15;
\echo === top statements by WAL bytes
SELECT toplevel AS top, calls, wal_records, wal_fpi, wal_bytes, round(wal_bytes::numeric / calls) AS wal_per_call, query
  FROM s ORDER BY wal_bytes DESC LIMIT 10;
\echo === totals (a nested statement — the balance trigger, an FK check — is counted again inside the top-level statement that fired it, so top = everything)
SELECT toplevel AS top, sum(calls) AS calls, sum(buffers) AS buffers, sum(temp_w) AS temp_written, sum(wal_fpi) AS wal_fpi, sum(wal_bytes) AS wal_bytes
  FROM s GROUP BY toplevel ORDER BY toplevel DESC;
\echo === WAL since start (pg_current_wal_lsn delta, all activity)
SELECT pg_wal_lsn_diff(pg_current_wal_lsn(), :'lsn0') AS wal_bytes_total;
\echo === pg_stat_wal (reset before the workload)
SELECT wal_records, wal_fpi, wal_bytes FROM pg_stat_wal;
\echo === pg_stat_user_tables
SELECT relname, seq_scan, seq_tup_read, idx_scan, idx_tup_fetch, n_tup_ins, n_tup_upd, n_tup_hot_upd, n_dead_tup
  FROM pg_stat_user_tables WHERE relname LIKE '\_autumn\_money%' ORDER BY relname;
\echo === pg_stat_user_indexes
SELECT relname, indexrelname, idx_scan, idx_tup_read
  FROM pg_stat_user_indexes WHERE relname LIKE '\_autumn\_money%' ORDER BY relname, indexrelname;
\echo === pg_statio_user_tables
SELECT relname, heap_blks_read + heap_blks_hit AS heap, idx_blks_read + idx_blks_hit AS idx,
       coalesce(toast_blks_read + toast_blks_hit, 0) AS toast
  FROM pg_statio_user_tables WHERE relname LIKE '\_autumn\_money%' ORDER BY relname;
