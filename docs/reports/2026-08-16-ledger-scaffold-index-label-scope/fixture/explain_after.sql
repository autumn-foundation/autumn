-- Single-call EXPLAIN for the FIXED #1146 index label load, scoped to the
-- FK values of one real page of `GET /comments` (page 1, DEFAULT_PAGE_SIZE
-- = 20; the `page_ids` setup query below reproduces exactly what
-- `page_data.content.iter().map(|row| row.post_id).collect()` computes in
-- the generated handler, over the same page the "before" plan's caller would
-- fetch).

SELECT array_agg(DISTINCT post_id ORDER BY post_id) AS page_ids
FROM (SELECT post_id FROM comments ORDER BY id DESC LIMIT 20 OFFSET 0) page
\gset

\echo '=== AFTER: page-scoped label load (this change) ==='
EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS)
SELECT id, title FROM posts WHERE id = ANY(:'page_ids'::bigint[]);
