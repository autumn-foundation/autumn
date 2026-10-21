-- The column of the `sw_posts.comment_count` derivation. A fleet database
-- made before this migration needs a backfill.
ALTER TABLE sw_posts ADD COLUMN comment_count BIGINT NOT NULL DEFAULT 0;
