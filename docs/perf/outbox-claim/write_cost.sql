-- Write-side cost of idx_autumn_outbox_claim: 200,000 ingest INSERTs, then
-- marking them sent (the transition that leaves the partial index). WAL bytes
-- are pg_current_wal_lsn() deltas; the elapsed time is informational only.
\set ON_ERROR_STOP on
DROP TABLE IF EXISTS autumn_outbox, autumn_inbox;
\i schema.sql
\if :with_claim_index
\else
DROP INDEX idx_autumn_outbox_claim;
\endif
CHECKPOINT;
SELECT pg_current_wal_lsn() AS l0 \gset
INSERT INTO autumn_outbox (id, aggregate, topic, payload, created_at, available_at)
SELECT 'w' || g, 'agg-' || (g % 5000), 'order.placed', repeat('x', 200), g, g FROM generate_series(1, 200000) g;
SELECT pg_current_wal_lsn() AS l1 \gset
CHECKPOINT;
SELECT pg_current_wal_lsn() AS l1b \gset
UPDATE autumn_outbox SET dispatched_at = 1 WHERE seq <= 100000;
SELECT pg_current_wal_lsn() AS l2 \gset
SELECT :with_claim_index AS with_claim_index,
       pg_wal_lsn_diff(:'l1', :'l0') AS insert_wal_bytes,
       pg_wal_lsn_diff(:'l2', :'l1b') AS mark_sent_wal_bytes,
       pg_relation_size('autumn_outbox') AS heap_bytes;
