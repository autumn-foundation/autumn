-- Insert WAL for two batches of 100,000 unread notifications, then ROLLBACK, so every variant
-- runs on the same rows. Batch 1 follows a CHECKPOINT, so it pays full-page images (cold).
-- Batch 2 touches pages already logged since that checkpoint (warm). Run `indexes.sql` first.
\set ON_ERROR_STOP on
SELECT setseed(0.7);
CHECKPOINT;
SELECT pg_current_wal_insert_lsn() AS l0 \gset
BEGIN;
INSERT INTO notifications (tenant_id, recipient_id, kind, payload)
SELECT '', 1 + floor(50000 * power(random(), 3))::bigint, 'kind' || (random() * 5)::int, repeat('x', 80)
FROM generate_series(1, 100000);
SELECT pg_current_wal_insert_lsn() AS l1 \gset
INSERT INTO notifications (tenant_id, recipient_id, kind, payload)
SELECT '', 1 + floor(50000 * power(random(), 3))::bigint, 'kind' || (random() * 5)::int, repeat('x', 80)
FROM generate_series(1, 100000);
SELECT pg_current_wal_insert_lsn() AS l2 \gset
ROLLBACK;
SELECT :'variant' || ' cold_wal_bytes=' || pg_wal_lsn_diff(:'l1', :'l0') || ' warm_wal_bytes=' || pg_wal_lsn_diff(:'l2', :'l1');
