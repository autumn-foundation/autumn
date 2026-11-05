-- Per recipient tier: unread_count (badge) and unread list (page 20). Buffers from EXPLAIN (ANALYZE, BUFFERS).
\set QUIET on
\pset format unaligned
\pset tuples_only on
\echo === recipient :rid
PREPARE c(bigint) AS SELECT COUNT(*) FROM notifications WHERE tenant_id = '' AND recipient_id = $1 AND read_at IS NULL;
PREPARE l(bigint) AS SELECT * FROM notifications WHERE tenant_id = '' AND recipient_id = $1 AND read_at IS NULL ORDER BY created_at DESC, id DESC LIMIT 20;
EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS, TIMING OFF) EXECUTE c(:rid);
EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS, TIMING OFF) EXECUTE l(:rid);
