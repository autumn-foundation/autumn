-- Single-call EXPLAIN for the CURRENT (pre-fix) #1146 index label load —
-- the query `render_index_reference_label_loads` (via the form's
-- `post_id_select_options` loader) emits on every `GET /comments` request,
-- regardless of page size or how many distinct `post_id` values the page
-- actually contains.

\echo '=== BEFORE: full-table label load (current codegen) ==='
EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS)
SELECT id, title FROM posts ORDER BY id ASC;
