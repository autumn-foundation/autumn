ALTER TABLE subreddits DROP COLUMN comment_count;

-- Only comments on posts can be represented by a `post_id` column, so the
-- others are dropped rather than silently re-pointed at a post that does not
-- exist. Same reason `commentable_id` never had a foreign key.
DELETE FROM comments WHERE commentable_type <> 'Post';

ALTER TABLE comments DROP COLUMN deleted_at;
DROP INDEX IF EXISTS idx_comments_thread;

ALTER TABLE comments ADD COLUMN post_id BIGINT;
UPDATE comments SET post_id = commentable_id;
ALTER TABLE comments ALTER COLUMN post_id SET NOT NULL;
ALTER TABLE comments
    ADD CONSTRAINT comments_post_id_fkey
    FOREIGN KEY (post_id) REFERENCES posts(id) ON DELETE CASCADE;
CREATE INDEX idx_comments_post_id ON comments (post_id);

ALTER TABLE comments DROP COLUMN commentable_id;
ALTER TABLE comments DROP COLUMN commentable_type;
