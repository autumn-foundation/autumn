\pset format unaligned
SELECT 'rows_read_by_filter_note' AS n;
SELECT relname, seq_scan, seq_tup_read, idx_scan, idx_tup_fetch FROM pg_stat_user_tables WHERE relname='notifications';
SELECT indexrelname, idx_scan, pg_relation_size(indexrelid) AS bytes FROM pg_stat_user_indexes WHERE relname='notifications';
SELECT calls, shared_blks_hit+shared_blks_read AS buffers, shared_blks_hit, shared_blks_read, temp_blks_written, wal_bytes, left(query,90) q
FROM pg_stat_statements WHERE query ILIKE '%notifications%' AND query NOT ILIKE '%pg_stat%' AND query NOT ILIKE '%EXPLAIN%' AND query NOT ILIKE 'CREATE%' AND query NOT ILIKE 'INSERT%' AND query NOT ILIKE 'PREPARE%' ORDER BY 2 DESC;
