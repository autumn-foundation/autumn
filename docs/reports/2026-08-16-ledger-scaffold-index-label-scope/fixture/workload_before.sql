-- Simulates ONE `GET /comments` request against the CURRENT (pre-fix)
-- generated `index` handler (`Pg{Model}Repository::page()` plus the #1146
-- label load autumn-cli's `render_index_reference_label_loads` used to
-- emit). Three statements, matching the three round trips the handler
-- issues: the pagination COUNT, the LIMIT/OFFSET page fetch, and the
-- reference-label load.
--
-- Run after `SELECT pg_stat_statements_reset();` to profile this workload in
-- isolation.

-- 1. repo.page(&page_req)'s COUNT(*).
SELECT count(*) FROM comments;

-- 2. repo.page(&page_req)'s LIMIT/OFFSET page fetch — page 1,
--    DEFAULT_PAGE_SIZE (autumn_web::pagination::DEFAULT_PAGE_SIZE) = 20.
SELECT * FROM comments ORDER BY id DESC LIMIT 20 OFFSET 0;

-- 3. BEFORE: the #1146 index label load. The old
--    `render_index_reference_label_loads` called the create/edit form's
--    `post_id_select_options(&mut db)` loader — a full, unfiltered scan of
--    the ENTIRE `posts` table, regardless of how many distinct `post_id`
--    values actually appear on the page fetched in statement 2.
SELECT id, title FROM posts ORDER BY id ASC;
