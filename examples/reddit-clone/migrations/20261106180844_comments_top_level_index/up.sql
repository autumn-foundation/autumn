-- #3325: a post page reads its comments one page of top-level comments at a
-- time. This index holds only the live top-level comments, in page order. A
-- page then reads its top-level comments in order, with no scan past the
-- replies between them. The check for a next page reads one index entry.
--
-- The predicate is the one the framework writes: `parent_id IS NULL AND
-- deleted_at IS NULL`.
--
-- `CONCURRENTLY` lets reads and writes continue during the build. It cannot
-- run in a transaction: `metadata.toml` sets `run_in_transaction = false`.
-- No `IF NOT EXISTS`: after a failed build, drop the INVALID index with
-- `DROP INDEX CONCURRENTLY idx_comments_top_level` and run again.
CREATE INDEX CONCURRENTLY idx_comments_top_level
    ON comments (commentable_type, commentable_id, created_at, id)
    WHERE parent_id IS NULL AND deleted_at IS NULL;
