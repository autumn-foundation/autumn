-- Front-page "top posts by votes" leaderboard (routes::posts::front_page):
--   SELECT post_id, SUM(value) FROM votes WHERE post_id IS NOT NULL
--   GROUP BY post_id ORDER BY 2 DESC LIMIT 5
-- aggregates every vote on every page view. Measured on a 3M-vote fixture it
-- was a Parallel Seq Scan (24.9k buffers) feeding a spilling HashAggregate
-- (14.4k temp blocks written). A covering index lets it run as an index-only
-- scan: buffers -49%, temp blocks 0, identical result. The scan is still
-- O(votes); this removes the heap visit and the spill, not the shape.
-- Report: docs/reports/2026-10-18-ledger-reddit-votes-covering-index/.
--
-- Write cost (measured, 100k vote inserts): +5.4% WAL; index is ~90 MB on a
-- 201 MB table. `idx_votes_post_id` is deliberately NOT dropped here (it may
-- back FK-cascade lookups and dropping an index needs its own review); it is
-- redundant with this one for post_id lookups and is a follow-up candidate.
--
-- CONCURRENTLY: SHARE UPDATE EXCLUSIVE only; does not block reads or writes.
-- Deliberately no IF NOT EXISTS: a cancelled concurrent build leaves an INVALID
-- index under this name, and IF NOT EXISTS would let a retry skip it silently.
-- Detect with: SELECT indexrelid::regclass FROM pg_index WHERE NOT indisvalid;
-- Clean up with: DROP INDEX CONCURRENTLY idx_votes_post_id_value;
CREATE INDEX CONCURRENTLY idx_votes_post_id_value
    ON votes (post_id) INCLUDE (value)
    WHERE post_id IS NOT NULL;
