-- usage: psql -v with_unread_index=true|false -f write_cost.sql  (run on the fixture database)
\set QUIET on
DROP TABLE IF EXISTS n_w; CREATE TABLE n_w (LIKE notifications INCLUDING DEFAULTS);
ALTER TABLE n_w ADD PRIMARY KEY (id);
CREATE INDEX ON n_w (tenant_id, recipient_id);
\if :with_unread_index
CREATE INDEX ON n_w (tenant_id, recipient_id) WHERE read_at IS NULL;
\endif
SELECT pg_current_wal_lsn() AS lsn0 \gset
SELECT setseed(0.7);
INSERT INTO n_w (id, tenant_id, recipient_id, kind, payload, read_at, created_at)
SELECT g, '', 1 + floor(20000 * power(random(),4))::bigint, 'like', repeat('x',80), NULL, now() FROM generate_series(1,200000) g;
SELECT pg_wal_lsn_diff(pg_current_wal_lsn(), :'lsn0') AS wal_bytes_insert_200k_all_unread;
SELECT pg_current_wal_lsn() AS lsn1 \gset
UPDATE n_w SET read_at = now() WHERE id % 2 = 0;
SELECT pg_wal_lsn_diff(pg_current_wal_lsn(), :'lsn1') AS wal_bytes_mark_100k_read;
SELECT pg_size_pretty(pg_indexes_size('n_w')) AS index_bytes;
DROP TABLE n_w;
