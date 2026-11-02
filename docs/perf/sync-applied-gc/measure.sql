\pset pager off
CREATE EXTENSION IF NOT EXISTS pg_stat_statements;
\echo === variant :variant rows :n
SELECT pg_size_pretty(pg_relation_size('autumn_sync_applied')) AS heap,
       pg_relation_size('autumn_sync_applied')/8192 AS heap_blocks,
       (SELECT string_agg(indexrelid::regclass || '=' || pg_relation_size(indexrelid)/8192 || ' blocks', ', ')
          FROM pg_index WHERE indrelid = 'autumn_sync_applied'::regclass) AS index_blocks;
-- Equivalence fingerprint of the rows that must be deleted / survive.
SELECT count(*) FILTER (WHERE applied_at <  timestamptz '2026-10-02 00:00:00+00') AS to_delete,
       count(*) FILTER (WHERE applied_at >= timestamptz '2026-10-02 00:00:00+00') AS survivors,
       count(*) FILTER (WHERE applied_at =  timestamptz '2026-10-02 00:00:00+00') AS at_boundary
FROM autumn_sync_applied;
SELECT pg_stat_statements_reset() IS NOT NULL AS reset \gset
\echo === EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS) of gc_applied
BEGIN;
EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS)
DELETE FROM autumn_sync_applied WHERE applied_at < timestamptz '2026-10-02 00:00:00+00';
ROLLBACK;
-- Real execution under pg_stat_statements (generic text, same as the Diesel bind).
SELECT pg_current_wal_lsn() AS lsn0 \gset
PREPARE gc(timestamptz) AS DELETE FROM autumn_sync_applied WHERE applied_at < $1;
EXECUTE gc('2026-10-02 00:00:00+00');
SELECT pg_wal_lsn_diff(pg_current_wal_lsn(), :'lsn0') AS gc_wal_bytes \gset
\echo === pg_stat_statements for the sweep
SELECT calls, rows, shared_blks_hit + shared_blks_read AS total_buffers, shared_blks_hit, shared_blks_read,
       temp_blks_written, wal_bytes, left(query, 60) AS query
FROM pg_stat_statements WHERE query LIKE '%DELETE FROM autumn_sync_applied%' AND query NOT LIKE 'EXPLAIN%';
\echo gc_wal_bytes :gc_wal_bytes
SELECT count(*) AS rows_after, md5(string_agg(change_id, ',' ORDER BY scope, device_id, change_id)) AS survivors_md5
FROM autumn_sync_applied;
SELECT seq_scan, seq_tup_read, idx_scan, idx_tup_fetch FROM pg_stat_user_tables WHERE relname='autumn_sync_applied';
SELECT indexrelname, idx_scan FROM pg_stat_user_indexes WHERE relname='autumn_sync_applied';
-- Write-side cost: a 50k-row dedup ingest (one day of pushes), WAL + buffers.
SELECT pg_stat_statements_reset() IS NOT NULL AS reset \gset
SELECT pg_current_wal_lsn() AS lsn1 \gset
INSERT INTO autumn_sync_applied (scope, device_id, change_id, version, applied_at)
SELECT 'tenant-' || (i % 200), 'dev-' || (i % 5000), 'new-' || i, i, timestamptz '2026-11-01 00:00:00+00' + i * interval '1 second'
FROM generate_series(1, 50000) i;
SELECT pg_wal_lsn_diff(pg_current_wal_lsn(), :'lsn1') AS ingest_wal_bytes_50k,
       (SELECT shared_blks_hit + shared_blks_read FROM pg_stat_statements WHERE query LIKE 'INSERT INTO autumn_sync_applied%') AS ingest_buffers_50k;
