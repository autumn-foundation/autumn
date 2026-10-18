BEGIN;
INSERT INTO comments(body,author_id,commentable_type,commentable_id) VALUES ('x',1,'Post',499999) RETURNING id \gset c_
INSERT INTO votes(user_id,comment_id,value) SELECT g, :c_id, 1 FROM generate_series(1,50) g;
INSERT INTO votes(user_id,post_id,value) SELECT 99000+g, 499990, -1 FROM generate_series(1,40) g ON CONFLICT DO NOTHING;
INSERT INTO votes(user_id,post_id,value) SELECT 99000+g, 499993, CASE WHEN g%2=0 THEN 1 ELSE -1 END FROM generate_series(1,40) g ON CONFLICT DO NOTHING;
ANALYZE votes;
\o /dev/null
SELECT 'x' \gset
\o
\echo -- index path
SELECT count(*) AS groups, md5(string_agg(post_id||':'||s, ',' ORDER BY s DESC, post_id)) AS md5, count(*) FILTER (WHERE s<0) AS neg, count(*) FILTER (WHERE s=0) AS zero FROM (SELECT post_id, SUM(value)::bigint s FROM votes WHERE post_id IS NOT NULL GROUP BY post_id) t;
EXPLAIN (COSTS OFF) SELECT post_id, SUM(value) FROM votes WHERE post_id IS NOT NULL GROUP BY post_id;
SET LOCAL enable_indexonlyscan=off; SET LOCAL enable_indexscan=off; SET LOCAL enable_bitmapscan=off;
\echo -- heap path
SELECT count(*) AS groups, md5(string_agg(post_id||':'||s, ',' ORDER BY s DESC, post_id)) AS md5, count(*) FILTER (WHERE s<0) AS neg, count(*) FILTER (WHERE s=0) AS zero FROM (SELECT post_id, SUM(value)::bigint s FROM votes WHERE post_id IS NOT NULL GROUP BY post_id) t;
EXPLAIN (COSTS OFF) SELECT post_id, SUM(value) FROM votes WHERE post_id IS NOT NULL GROUP BY post_id;
ROLLBACK;
