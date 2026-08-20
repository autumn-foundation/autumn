DROP TRIGGER IF EXISTS subreddits_delete_comments ON subreddits;
DROP TRIGGER IF EXISTS posts_delete_comments ON posts;
DROP FUNCTION IF EXISTS comments_delete_for_parent();

ALTER TABLE subreddits DROP COLUMN comment_count;

-- Only comments on posts can be represented by a `post_id` column, so the
-- others are dropped rather than silently re-pointed at a post that does not
-- exist. Same reason `commentable_id` never had a foreign key.
DELETE FROM comments WHERE commentable_type <> 'Post';

-- Soft-deleted comments are gone as far as the app is concerned, and
-- `posts.comment_count` was decremented when they went. Dropping the column
-- would resurrect them into every listing with the counter now permanently too
-- low, so they are removed for real first.
DELETE FROM comments WHERE deleted_at IS NOT NULL;
ALTER TABLE comments DROP COLUMN deleted_at;
DROP INDEX IF EXISTS idx_comments_thread;

-- A comment whose post was deleted while the polymorphic schema was in force
-- has no `post_id` to restore, and the NOT NULL foreign key below would refuse
-- it. Drop those rather than fail the revert half-way.
DELETE FROM comments
 WHERE commentable_id NOT IN (SELECT id FROM posts);

ALTER TABLE comments ADD COLUMN post_id BIGINT;
UPDATE comments SET post_id = commentable_id;
ALTER TABLE comments ALTER COLUMN post_id SET NOT NULL;
-- Spelled with ON DELETE CASCADE, matching the constraint the create migration
-- shipped: without it, deleting a post after a revert fails outright.
ALTER TABLE comments
    ADD CONSTRAINT comments_post_id_fkey
    FOREIGN KEY (post_id) REFERENCES posts(id) ON DELETE CASCADE;
CREATE INDEX idx_comments_post_id ON comments (post_id);

ALTER TABLE comments DROP COLUMN commentable_id;
ALTER TABLE comments DROP COLUMN commentable_type;
