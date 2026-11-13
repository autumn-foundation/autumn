-- 100 worker cycles: claim, then ack the claimed job. Claimed ids go to :out.
\i :claim
CHECKPOINT;
SELECT pg_stat_statements_reset();
SELECT pg_stat_reset();
\o :out
\set i 0
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
EXECUTE claim('worker-1', 'default', '{}') \gset c_
SELECT :'c_id' AS claimed;
EXECUTE ack(:'c_id', 'worker-1');
\o
