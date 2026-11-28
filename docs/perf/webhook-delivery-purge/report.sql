-- Profile of one run.sh workload. pg_stat_statements, pg_stat_user_tables,
-- pg_statio_user_tables, pg_current_wal_lsn. Run with -v lsn0=... -v lsn1=...
\pset footer off
\echo == workload totals (pg_stat_statements, this database)
SELECT sum(calls) AS calls,
       sum(shared_blks_hit + shared_blks_read) AS buffers,
       sum(temp_blks_written) AS temp_blks_written,
       sum(wal_bytes) AS wal_bytes,
       pg_wal_lsn_diff(pg_current_wal_lsn(), :'lsn0') AS lsn_wal_bytes,
       pg_wal_lsn_diff(pg_current_wal_lsn(), :'lsn1') AS lsn_wal_bytes_purge
FROM pg_stat_statements s JOIN pg_database d ON d.oid = s.dbid
WHERE d.datname = current_database() AND query !~* 'pg_stat|pg_current_wal';

CREATE TEMP VIEW prof AS
SELECT left(regexp_replace(query, '\s+', ' ', 'g'), 70) AS statement,
       calls, rows,
       shared_blks_hit + shared_blks_read AS buffers,
       shared_blks_hit AS hit, shared_blks_read AS read,
       temp_blks_read, temp_blks_written, wal_bytes, wal_fpi
FROM pg_stat_statements s JOIN pg_database d ON d.oid = s.dbid
WHERE d.datname = current_database() AND query !~* 'pg_stat|pg_current_wal|^(BEGIN|COMMIT)';

\echo == top 10 by buffers
SELECT statement, calls, buffers,
       round(100.0 * buffers / sum(buffers) OVER (), 1) AS pct_buffers,
       round(100.0 * calls / sum(calls) OVER (), 1) AS pct_calls,
       hit, read, rows, temp_blks_written, wal_bytes, wal_fpi
FROM prof ORDER BY buffers DESC LIMIT 10;
\echo == top 10 by calls
SELECT statement, calls, round(100.0 * calls / sum(calls) OVER (), 1) AS pct_calls, buffers
FROM prof ORDER BY calls DESC LIMIT 10;
\echo == top 10 by WAL bytes
SELECT statement, calls, wal_bytes, round(100.0 * wal_bytes / NULLIF(sum(wal_bytes) OVER (), 0), 1) AS pct_wal, wal_fpi
FROM prof ORDER BY wal_bytes DESC LIMIT 10;
\echo == statements with temp blocks
SELECT statement, temp_blks_read, temp_blks_written FROM prof WHERE temp_blks_written > 0;

\echo == pg_stat_user_tables
SELECT relname, seq_scan, seq_tup_read, idx_scan, idx_tup_fetch, n_tup_ins, n_tup_upd,
       n_tup_hot_upd, n_tup_del, n_live_tup, n_dead_tup
FROM pg_stat_user_tables ORDER BY seq_tup_read DESC;
\echo == pg_statio_user_tables
SELECT relname, heap_blks_read + heap_blks_hit AS heap_blks, idx_blks_read + idx_blks_hit AS idx_blks,
       toast_blks_read + toast_blks_hit AS toast_blks
FROM pg_statio_user_tables ORDER BY relname;
\echo == pg_stat_user_indexes
SELECT indexrelname, idx_scan, idx_tup_read, pg_size_pretty(pg_relation_size(indexrelid)) AS size,
       pg_relation_size(indexrelid) / 8192 AS pages
FROM pg_stat_user_indexes ORDER BY indexrelname;
\echo == relation sizes
SELECT pg_size_pretty(pg_relation_size('autumn_webhook_deliveries')) AS heap,
       pg_relation_size('autumn_webhook_deliveries') / 8192 AS heap_pages,
       pg_size_pretty(pg_total_relation_size('autumn_webhook_deliveries')) AS total;
