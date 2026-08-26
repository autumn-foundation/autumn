-- Deterministic, production-shaped `posts`/`comments` fixture for the
-- generated `GET /comments` index-handler workload.
--
-- Usage:
--   psql -d ledger_bench -v posts=500000 -v comments=2000000 -f seed.sql
--
-- `:posts`    — total rows in the referenced `posts` table (the #1146
--               display-label source the index page loads from). This is
--               the dimension the fix scales with: the buggy loader scans
--               ALL of it on every index view regardless of `:comments` or
--               page size.
-- `:comments` — total rows in the paginated child table, skewed so the most
--               recent 20% of posts (a "trending/recent" slice, matching how
--               a real blog's comment volume concentrates on new posts)
--               receive 80% of comments; the remaining 20% spread uniformly
--               across every post. Mirrors a multi-year blog/forum with a
--               long tail of old, quiet posts.

SELECT setseed(0.4241);

TRUNCATE comments, posts RESTART IDENTITY;

INSERT INTO posts (title, created_at)
SELECT
    'Post #' || gs || ': ' || md5(gs::text),
    NOW() - ((:posts - gs) * INTERVAL '1 hour')
FROM generate_series(1, :posts) AS gs;

INSERT INTO comments (post_id, body, created_at)
SELECT
    CASE
        WHEN r < 0.8 THEN
            hot_start + 1 + floor(random() * GREATEST(:posts - hot_start, 1))::bigint
        ELSE
            1 + floor(random() * :posts)::bigint
    END,
    'Comment body ' || gs || ': ' || md5((gs * 7)::text),
    NOW() - ((:comments - gs) * INTERVAL '1 minute')
FROM (
    SELECT gs, random() AS r, (:posts - GREATEST(:posts / 5, 1))::bigint AS hot_start
    FROM generate_series(1, :comments) AS gs
) s;

VACUUM ANALYZE posts;
VACUUM ANALYZE comments;
