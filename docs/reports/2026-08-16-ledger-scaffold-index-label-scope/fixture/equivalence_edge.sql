-- Edge cases the plain equivalence.sql page doesn't exercise:
--
-- 1. Duplicate FK values on one page (several comments on the same post) —
--    `page_data.content.iter().map(...)` collects one id PER ROW, so
--    `{name}_ids` can contain duplicates; `id = ANY($1)` on `posts`' PRIMARY
--    KEY must still return exactly one row per DISTINCT id, matching the
--    lookup contract (`{name}_labels.get(&row.{name}.to_string())` is a
--    HashMap keyed by id, so a duplicate entry would just overwrite itself
--    with the same value — never a correctness issue, but worth confirming
--    the query itself doesn't fan out extra rows).
-- 2. An empty page (0 comments matched, e.g. a brand-new resource or the
--    last page past the end) — `{name}_ids` is `vec![]`; `id = ANY('{}')`
--    must return zero rows without erroring, matching what the OLD
--    full-table loader would have shown (a non-empty label map that's
--    simply never looked up, since there are no rows to render).
-- 3. A `post_id` with no matching `posts` row (the label lookup's existing
--    "-" fallback path, e.g. a stale reference) — the scoped query and the
--    full-table query must agree that no such id exists.

BEGIN;

\echo '--- 1. duplicate FK values: one distinct post, several comments ---'
-- Every comment on `post_id = 1` (guaranteed to have at least one comment in
-- any fixture size — post 1 is the oldest, so it accumulates comments
-- across the whole seed window).
SELECT id, post_id FROM comments WHERE post_id = 1 ORDER BY id LIMIT 5;

-- Simulate `{name}_ids` for a page where every row references post_id = 1:
-- the array has N copies of the same id.
SELECT count(*) AS rows_returned, count(DISTINCT id) AS distinct_ids
FROM posts
WHERE id = ANY(ARRAY[1, 1, 1, 1, 1]::bigint[]);
-- Expect rows_returned = 1, distinct_ids = 1 — no fan-out from duplicate
-- array elements.

\echo '--- 2. empty page: id = ANY(empty array) ---'
SELECT count(*) AS must_be_zero FROM posts WHERE id = ANY(ARRAY[]::bigint[]);

\echo '--- 3. FK with no matching parent row ---'
SELECT count(*) AS must_be_zero FROM posts WHERE id = ANY(ARRAY[-1]::bigint[]);
SELECT count(*) AS must_also_be_zero FROM posts WHERE id = -1;
-- Both the scoped query and a full-table scan agree: id -1 was never
-- inserted (posts is BIGSERIAL starting at 1), so both correctly produce no
-- label for it and the caller's existing "—" fallback applies either way.

ROLLBACK;
