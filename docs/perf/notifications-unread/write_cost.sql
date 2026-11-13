-- WAL cost of the unread index on the ingest path. psql -v with_index=true|false -f write_cost.sql
-- 100,000 inserts (all unread) then mark-all-read of them (one UPDATE), WAL bytes from LSN deltas.
\set ON_ERROR_STOP on
\i fixture.sql
DROP INDEX IF EXISTS idx_notifications_unread;
SELECT :'with_index' = 'true' AS wi \gset
\if :wi
CREATE INDEX idx_notifications_unread ON notifications (tenant_id, recipient_id, id) WHERE read_at IS NULL;
\endif
CHECKPOINT;
SELECT pg_current_wal_lsn() AS l0 \gset
INSERT INTO notifications (recipient_id, kind, payload) SELECT 5000 + g % 2000, 'mention', repeat('x',120) FROM generate_series(1,100000) g;
SELECT pg_current_wal_lsn() AS l1 \gset
UPDATE notifications SET read_at = now() WHERE read_at IS NULL AND recipient_id >= 5000;
SELECT pg_current_wal_lsn() AS l2 \gset
SELECT :'with_index' AS with_index, pg_wal_lsn_diff(:'l1',:'l0') AS insert_wal_bytes, pg_wal_lsn_diff(:'l2',:'l1') AS mark_read_wal_bytes;
