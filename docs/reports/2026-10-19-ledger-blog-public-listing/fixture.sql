-- Seeded, deterministic blog fixture. Usage: psql -v n=100000 -f fixture.sql blog
-- ~5% drafts, ~8 KB bodies (TOAST-sized), created_at spread over 8 years.
SELECT setseed(0.42);
TRUNCATE posts RESTART IDENTITY;
INSERT INTO posts (title, slug, body, published, created_at, updated_at)
SELECT 'Post ' || g,
       'post-' || g,
       (SELECT string_agg(md5(random()::text || k), ' ') FROM generate_series(1, 240) k WHERE g > 0),
       (g % 20) <> 0,
       TIMESTAMP '2018-01-01' + (g * interval '28 seconds' * (2900000.0 / :n)),
       TIMESTAMP '2018-01-01' + (g * interval '28 seconds' * (2900000.0 / :n))
FROM generate_series(1, :n) g;
ANALYZE posts;
