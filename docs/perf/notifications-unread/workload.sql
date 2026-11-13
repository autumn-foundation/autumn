-- Per page view: the bell badge (unread_count) and the unread feed page (count + page),
-- SQL as diesel emits it. 50 views for the heavy recipient (1), 50 for a median one (1500).
\set ON_ERROR_STOP on
CREATE EXTENSION IF NOT EXISTS pg_stat_statements;
SELECT pg_stat_statements_reset() \gset
\o /dev/null
SELECT format($f$SELECT COUNT(*) FROM notifications WHERE tenant_id = '' AND recipient_id = %1$s AND read_at IS NULL$f$, r) FROM (VALUES (1),(1500)) v(r), generate_series(1,50) \gexec
SELECT format($f$SELECT id, tenant_id, recipient_id, kind, payload, read_at, created_at FROM notifications WHERE tenant_id = '' AND recipient_id = %1$s AND read_at IS NULL ORDER BY id DESC LIMIT 20 OFFSET 0$f$, r) FROM (VALUES (1),(1500)) v(r), generate_series(1,50) \gexec
\o
\echo '--- pg_stat_statements: notifications statements (buffers = hit+read)'
SELECT calls, shared_blks_hit + shared_blks_read AS buffers, shared_blks_hit AS hit, shared_blks_read AS read,
       temp_blks_written AS temp_w, rows, round(100.0*(shared_blks_hit+shared_blks_read)/sum(shared_blks_hit+shared_blks_read) OVER (),1) AS pct_buffers,
       round(100.0*calls/sum(calls) OVER (),1) AS pct_calls, left(query,90) AS q
FROM pg_stat_statements WHERE query ILIKE '%notifications%' AND query NOT ILIKE '%pg_stat%' ORDER BY buffers DESC;
\echo '--- EXPLAIN (heavy recipient): unread_count'
EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS) SELECT COUNT(*) FROM notifications WHERE tenant_id = '' AND recipient_id = 1 AND read_at IS NULL;
\echo '--- EXPLAIN (heavy recipient): unread page'
EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS) SELECT id, tenant_id, recipient_id, kind, payload, read_at, created_at FROM notifications WHERE tenant_id = '' AND recipient_id = 1 AND read_at IS NULL ORDER BY id DESC LIMIT 20 OFFSET 0;
\echo '--- EXPLAIN (median recipient): unread_count'
EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS) SELECT COUNT(*) FROM notifications WHERE tenant_id = '' AND recipient_id = 1500 AND read_at IS NULL;
\echo '--- equivalence: md5 of ordered unread ids, plus counts, per recipient'
SELECT recipient_id, count(*), md5(string_agg(id::text, ',' ORDER BY id)) FROM notifications WHERE tenant_id='' AND read_at IS NULL AND recipient_id IN (1,1500,1001,20999) GROUP BY recipient_id ORDER BY recipient_id;
