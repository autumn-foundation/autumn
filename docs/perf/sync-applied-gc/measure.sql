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
-- Write-side cost: one day of pushes = 200 pushes x 250 dedup rows (50k rows).
-- Shaped like pg_insert_applied: one INSERT per push, one scope/device, and
-- applied_at left to its now() default, so every row of a push shares one
-- timestamp (pinned here per push, 432 s apart across the day).
SELECT pg_stat_statements_reset() IS NOT NULL AS reset \gset
SELECT pg_current_wal_lsn() AS lsn1 \gset
SELECT format($f$INSERT INTO autumn_sync_applied (scope, device_id, change_id, version, applied_at)
  SELECT 'tenant-%1$s', 'dev-%2$s', 'new-%3$s-' || g, g, timestamptz '2026-11-01 00:00:00+00' + %3$s * interval '432 seconds'
  FROM generate_series(1, 250) g$f$, p % 200, p % 5000, p)
FROM generate_series(1, 200) p \gexec
SELECT pg_wal_lsn_diff(pg_current_wal_lsn(), :'lsn1') AS ingest_wal_bytes_50k,
       (SELECT sum(shared_blks_hit + shared_blks_read) FROM pg_stat_statements WHERE query LIKE 'INSERT INTO autumn_sync_applied%') AS ingest_buffers_50k,
       (SELECT sum(calls) FROM pg_stat_statements WHERE query LIKE 'INSERT INTO autumn_sync_applied%') AS ingest_statements;
