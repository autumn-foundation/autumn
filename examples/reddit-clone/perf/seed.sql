-- Deterministic, production-shaped fixture for the reddit-clone read workload.
-- Run AFTER the app has booted once (it applies the migrations).
-- Skew: subreddit popularity and post comment counts are power-law (u^k).
SELECT setseed(0.42);
TRUNCATE users, subreddits, posts, comments, votes, tags, post_tags RESTART IDENTITY CASCADE;

INSERT INTO users (username, password_hash, karma, role, created_at)
SELECT 'user' || g, 'x', (random()*1000)::bigint, 'user', now() - (random()*900||' days')::interval
FROM generate_series(1, 100000) g;

INSERT INTO subreddits (name, slug, description, creator_id, subscriber_count, created_at)
SELECT 'Sub' || g, 'sub' || g, 'desc ' || g, 1 + (random()*99999)::int, (random()*100000)::bigint,
       now() - (random()*900||' days')::interval
FROM generate_series(1, 1000) g;

-- 500k posts; subreddit chosen with power-law skew (few huge subs, long tail)
INSERT INTO posts (title, slug, body, url, author_id, subreddit_id, score, hot_rank, comment_count, created_at, updated_at)
SELECT 'Post title ' || g, 'post-' || g,
       CASE WHEN random() < 0.4 THEN '' ELSE repeat('body text ', 1 + (random()*40)::int) END,
       CASE WHEN random() < 0.5 THEN 'https://example.com/' || g END,
       1 + (random()*99999)::int,
       1 + floor(1000 * power(random(), 3))::int,
       (random()*5000)::bigint - 500, random()*100, 0,
       t, t
FROM (SELECT g, now() - (random()*900||' days')::interval AS t FROM generate_series(1, 500000) g) s;

-- 3M comments; target post chosen with heavy skew; ~30% replies; ~2% soft-deleted
INSERT INTO comments (body, author_id, commentable_type, commentable_id, parent_id, score, created_at, deleted_at)
SELECT 'comment body ' || g, 1 + (random()*99999)::int, 'Post',
       1 + floor(500000 * power(random(), 4))::int, NULL,
       (random()*200)::bigint - 20, now() - (random()*800||' days')::interval,
       CASE WHEN random() < 0.02 THEN now() END
FROM generate_series(1, 3000000) g;
-- replies: attach 30% to an earlier comment on the same post
UPDATE comments c SET parent_id = p.id
FROM (SELECT DISTINCT ON (commentable_id) commentable_id, id FROM comments ORDER BY commentable_id, id) p
WHERE c.commentable_id = p.commentable_id AND c.id > p.id AND c.id % 10 < 3;

UPDATE posts p SET comment_count = x.n FROM
 (SELECT commentable_id, count(*) n FROM comments GROUP BY 1) x WHERE x.commentable_id = p.id;
UPDATE subreddits s SET comment_count = x.n FROM
 (SELECT p.subreddit_id, sum(p.comment_count) n FROM posts p GROUP BY 1) x WHERE x.subreddit_id = s.id;

-- 3M votes (unique per user/post), mostly on popular posts
INSERT INTO votes (user_id, post_id, value, created_at)
SELECT DISTINCT ON (u, p) u, p, CASE WHEN random() < 0.8 THEN 1 ELSE -1 END, now()
FROM (SELECT 1 + (random()*99999)::int u, 1 + floor(500000 * power(random(), 3))::int p
      FROM generate_series(1, 3000000)) s;

INSERT INTO tags (name, slug) SELECT 'tag'||g, 'tag'||g FROM generate_series(1, 300) g;
INSERT INTO post_tags SELECT DISTINCT p, 1 + floor(300*power(random(),2))::int
FROM (SELECT 1 + (random()*499999)::int p FROM generate_series(1, 400000)) s;

-- dead-tuple ratio: churn ~15% of posts, leave them for autovacuum
ALTER TABLE posts SET (autovacuum_enabled = false);
UPDATE posts SET score = score + 1 WHERE id % 7 = 0;
ANALYZE;
