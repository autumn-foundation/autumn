-- Seeded, deterministic fixture for the paged comment-thread read.
-- Schema: the shared `comments` table exactly as `autumn generate scaffold
-- ... comments:commentable` writes it (autumn-cli/src/generate/commentable.rs),
-- plus a `posts` parent with `comment_count`.
--
-- Shape (scale = :scale, default 150000):
--   * 40,000 posts. Post p has floor(scale / p) comments (min 1): Zipf skew,
--     the first post is a viral thread, most posts have a handful.
--   * 35% of comments are top-level. A reply's parent is an earlier comment
--     in the same thread, biased toward recent ones, so threads nest.
--   * created_at is monotone within a thread; threads start at random times
--     over two years and rows are inserted in global time order, so one
--     thread's rows are scattered over the heap as in production. ids follow
--     insert order, as BIGSERIAL does.
--   * ~3% of comments are soft-deleted together with their whole subtree
--     (the framework's delete_comment rule: no live reply under a dead parent).
--   * 5% of rows edited, then VACUUM ANALYZE, then another 1% edited with no
--     vacuum, so the table carries live dead tuples and a mostly-set
--     visibility map.
\if :{?scale}
\else
\set scale 150000
\endif
SELECT setseed(0.4242);
DROP TABLE IF EXISTS comments, posts, _seed;
CREATE TABLE posts (
    id BIGSERIAL PRIMARY KEY,
    title TEXT NOT NULL,
    comment_count BIGINT NOT NULL DEFAULT 0
);
INSERT INTO posts (title) SELECT 'post ' || g FROM generate_series(1, 40000) g;

CREATE TABLE comments (
    id BIGSERIAL PRIMARY KEY,
    commentable_type TEXT NOT NULL,
    commentable_id BIGINT NOT NULL,
    parent_id BIGINT REFERENCES comments(id) ON DELETE CASCADE,
    author_id BIGINT NOT NULL,
    body TEXT NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT NOW(),
    deleted_at TIMESTAMP
);
CREATE INDEX IF NOT EXISTS idx_comments_thread
    ON comments (commentable_type, commentable_id, created_at, id);
CREATE INDEX IF NOT EXISTS idx_comments_parent_id ON comments (parent_id);

-- One row per comment: thread, ordinal k, parent ordinal, timestamp.
CREATE UNLOGGED TABLE _seed AS
SELECT p AS post, k,
       CASE WHEN k = 0 OR random() < 0.35 THEN NULL
            ELSE greatest(0, k - 1 - floor((random() ^ 3) * k)::int) END AS parent_k,
       timestamp '2024-11-01' + (start_s + k * step_s) * interval '1 second' AS created_at,
       1 + floor(random() * 200000)::bigint AS author_id
FROM (SELECT p, greatest(1, floor(:scale::numeric / p))::int AS n,
             random() * 86400 * 600 AS start_s,
             -- a viral thread lives for weeks, a quiet one for days
             86400 * (3 + random() * 30) / greatest(1, floor(:scale::numeric / p)) AS step_s
        FROM generate_series(1, 40000) p) t,
     LATERAL generate_series(0, t.n - 1) k;
CREATE INDEX ON _seed (post, k);
ALTER TABLE _seed ADD COLUMN id BIGINT;
UPDATE _seed s SET id = o.rn FROM (
  SELECT post, k, row_number() OVER (ORDER BY created_at, post, k) AS rn FROM _seed) o
 WHERE o.post = s.post AND o.k = s.k;

INSERT INTO comments (id, commentable_type, commentable_id, parent_id, author_id, body, created_at)
SELECT s.id, 'Post', s.post, par.id, s.author_id,
       repeat(md5(s.id::text), 1 + (s.id % 9)::int), s.created_at
  FROM _seed s LEFT JOIN _seed par ON par.post = s.post AND par.k = s.parent_k
 ORDER BY s.id;
SELECT setval('comments_id_seq', (SELECT max(id) FROM comments));

-- Soft-delete ~3% of comments with their subtrees.
WITH RECURSIVE dead(id) AS (
  SELECT id FROM comments WHERE random() < 0.03
  UNION
  SELECT c.id FROM comments c JOIN dead d ON c.parent_id = d.id
)
UPDATE comments c SET deleted_at = c.created_at + interval '1 hour'
  FROM dead WHERE c.id = dead.id;
UPDATE posts p SET comment_count = n FROM (
  SELECT commentable_id, count(*) n FROM comments WHERE deleted_at IS NULL GROUP BY 1) x
 WHERE x.commentable_id = p.id;
DROP TABLE _seed;

UPDATE comments SET body = body || ' (edited)' WHERE id % 20 = 7;
VACUUM ANALYZE comments;
VACUUM ANALYZE posts;
UPDATE comments SET body = body || ' (edited again)' WHERE id % 100 = 13;
ANALYZE comments;

-- The author model named by `#[commentable(by = User, author_name = username)]`.
DROP TABLE IF EXISTS users;
CREATE TABLE users (id BIGSERIAL PRIMARY KEY, username TEXT NOT NULL);
INSERT INTO users (username) SELECT 'user_' || g FROM generate_series(1, 200000) g;
VACUUM ANALYZE users;
