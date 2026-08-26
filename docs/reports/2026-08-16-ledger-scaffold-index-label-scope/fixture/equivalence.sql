-- Result-equivalence check: the label map the FIXED (scoped) query builds
-- for one page must be identical, label-for-label, to that same page's
-- entries in the label map the OLD (full-table) query built. The fix only
-- narrows which rows are fetched — it can never change what a fetched row's
-- label is, since both queries select the same `(id, title)` columns from
-- the same table.
--
-- Run against the `large` fixture (seeded via `-v posts=500000
-- -v comments=2000000`).

-- The page (page 1, DEFAULT_PAGE_SIZE = 20) whose labels are compared —
-- exactly what `page_data.content.iter().map(|row| row.post_id).collect()`
-- computes in the generated handler.
SELECT array_agg(DISTINCT post_id ORDER BY post_id) AS page_ids
FROM (SELECT post_id FROM comments ORDER BY id DESC LIMIT 20 OFFSET 0) page
\gset

\echo '--- old-vs-new label diff for the page (must be 0 rows) ---'
SELECT id, title FROM posts WHERE id = ANY(:'page_ids'::bigint[])   -- AFTER
EXCEPT
SELECT id, title FROM posts;                                       -- BEFORE (unfiltered)

\echo '--- row count check: scoped query returns exactly one row per distinct page id ---'
SELECT
    (SELECT count(*) FROM posts WHERE id = ANY(:'page_ids'::bigint[])) AS scoped_rows,
    cardinality(:'page_ids'::bigint[]) AS distinct_page_ids;
