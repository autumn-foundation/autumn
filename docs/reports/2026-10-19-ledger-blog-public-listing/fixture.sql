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

-- Boundary rows for the card preview (`body.len() > 200` is BYTES, the preview
-- is 200 CHARS): empty, 199/200/201 bytes, multibyte straddling the cut, a draft.
INSERT INTO posts (title, slug, body, published, created_at, updated_at) VALUES
 ('edge empty',   'edge-empty',   '',                       true,  now(), now()),
 ('edge 199',     'edge-199',     repeat('a', 199),         true,  now(), now()),
 ('edge 200',     'edge-200',     repeat('a', 200),         true,  now(), now()),
 ('edge 201',     'edge-201',     repeat('a', 201),         true,  now(), now()),
 ('edge 100x2B',  'edge-100x2b',  repeat('é', 100),         true,  now(), now()),
 ('edge 101x2B',  'edge-101x2b',  repeat('é', 101),         true,  now(), now()),
 ('edge 200x2B',  'edge-200x2b',  repeat('é', 200),         true,  now(), now()),
 ('edge 150x4B',  'edge-150x4b',  repeat('🍂', 150),        true,  now(), now()),
 ('edge draft',   'edge-draft',   repeat('d', 500),         false, now(), now());
-- distinct timestamps: ORDER BY created_at DESC must not depend on tie order.
UPDATE posts SET created_at = TIMESTAMP '2027-01-01' + id * interval '1 second', updated_at = created_at WHERE slug LIKE 'edge-%';
ANALYZE posts;
