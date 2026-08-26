-- Simulates ONE `GET /comments` request against the FIXED generated `index`
-- handler. Statements 1 and 2 are byte-for-byte identical to
-- workload_before.sql (pagination is untouched by this change) — only
-- statement 3 (the #1146 label load) differs, so any buffer delta between
-- the before/after profiles is attributable entirely to that one statement.
--
-- Takes `:page_ids` — a Postgres array-literal string of the `post_id`
-- values that appear on the page statement 2 fetches (computed once per
-- fixture size from the *same* page-2 query, OUTSIDE this script and before
-- `pg_stat_statements_reset()`, mirroring how the real Rust handler already
-- holds those ids in memory from its own `Vec<Comment>` — no extra DB round
-- trip is needed to learn them). See README "Reproduce" for the capture
-- step.
--
-- Run after `SELECT pg_stat_statements_reset();` to profile this workload in
-- isolation.

-- 1. repo.page(&page_req)'s COUNT(*) — identical to workload_before.sql.
SELECT count(*) FROM comments;

-- 2. repo.page(&page_req)'s LIMIT/OFFSET page fetch — identical to
--    workload_before.sql.
SELECT * FROM comments ORDER BY id DESC LIMIT 20 OFFSET 0;

-- 3. AFTER: the fixed #1146 index label load, scoped to the page's own FK
--    values via `WHERE id = ANY(...)` — matching the generated
--    `render_index_reference_label_loads` query exactly.
SELECT id, title FROM posts WHERE id = ANY(:'page_ids'::bigint[]);
