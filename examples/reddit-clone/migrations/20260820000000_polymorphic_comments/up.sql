-- Polymorphic, threaded comments (issue #1367).
--
-- The `comments` table was hard-wired to `post_id`, so adding comments to a
-- second model would have meant a second table, a second set of routes, and a
-- second threading query. Re-key it on `(commentable_type, commentable_id)`
-- and it serves every `#[commentable]` model in the app at once -- here `Post`
-- and `Subreddit`.

-- Nullable first so the backfill has somewhere to land.
ALTER TABLE comments ADD COLUMN commentable_type TEXT;
ALTER TABLE comments ADD COLUMN commentable_id BIGINT;

-- Every existing comment belongs to a post. `'Post'` is the discriminator
-- `#[commentable]` derives from the model's Rust type name.
UPDATE comments SET commentable_type = 'Post', commentable_id = post_id;

ALTER TABLE comments ALTER COLUMN commentable_type SET NOT NULL;
ALTER TABLE comments ALTER COLUMN commentable_id SET NOT NULL;

-- `commentable_id` deliberately gains no REFERENCES: a single column cannot
-- reference two tables. The framework's write path probes and row-locks the
-- parent before every insert instead, so an unknown parent is a 404 rather
-- than a dangling row.
--
-- Dropping `post_id` also drops its ON DELETE CASCADE, so deleting a post
-- would leave its comments behind: unreachable (the parent probe 404s) and
-- undeletable. A trigger restores the cascade for every commentable model at
-- once -- see `comments_delete_for_parent` below.
DROP INDEX IF EXISTS idx_comments_post_id;
ALTER TABLE comments DROP COLUMN post_id;

-- The thread query filters on the discriminator pair and orders by
-- (created_at, id), so one index covers it whole.
CREATE INDEX idx_comments_thread
    ON comments (commentable_type, commentable_id, created_at, id);

-- Deleting a comment cascades to its replies by soft-deleting the subtree, so
-- the thread read has to be soft-delete aware.
ALTER TABLE comments ADD COLUMN deleted_at TIMESTAMP;

-- The second commentable model (issue #1367 AC5). Adding comments to
-- `Subreddit` costs exactly this one counter column plus the `#[commentable]`
-- attribute -- no table, no routes, no queries.
ALTER TABLE subreddits ADD COLUMN comment_count BIGINT NOT NULL DEFAULT 0;

-- The cascade a polymorphic foreign key cannot express. One trigger function
-- serves every commentable model; TG_ARGV[0] is the discriminator.
CREATE OR REPLACE FUNCTION comments_delete_for_parent() RETURNS TRIGGER AS $$
BEGIN
    DELETE FROM comments
     WHERE commentable_type = TG_ARGV[0]
       AND commentable_id = OLD.id;
    RETURN OLD;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER posts_delete_comments
    AFTER DELETE ON posts
    FOR EACH ROW EXECUTE FUNCTION comments_delete_for_parent('Post');

CREATE TRIGGER subreddits_delete_comments
    AFTER DELETE ON subreddits
    FOR EACH ROW EXECUTE FUNCTION comments_delete_for_parent('Subreddit');
