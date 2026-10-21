-- #2573: resolve a taken post slug in the database.
--
-- `posts_subreddit_id_slug_key` (#2544) rejects a duplicate slug. The HTML
-- routes retry with a new suffix. The generated repository `save` and
-- `update_by_id` cannot: their hook has no connection. A race there ended in
-- an unhandled unique violation.
--
-- This trigger gives every write path the same rule. A taken slug gets the
-- next free `-2`, `-3`, ... suffix. The unique constraint stays as the last
-- guard.
--
-- The advisory lock serializes writers per subreddit until commit. A later
-- writer then sees the committed row and picks the next free suffix. The lock
-- is per subreddit, not per slug, so `foo-2` cannot race with a title `foo`.
--
-- Limit: one UPDATE that sets the same slug on many rows still fails. A row
-- trigger cannot see rows that the same statement changed.
CREATE FUNCTION posts_resolve_slug() RETURNS trigger AS $$
DECLARE
    base   TEXT := NEW.slug;
    suffix BIGINT := 2;
BEGIN
    PERFORM pg_advisory_xact_lock(hashtextextended('posts.slug:' || NEW.subreddit_id, 0));
    WHILE EXISTS (
        SELECT 1 FROM posts
        WHERE subreddit_id = NEW.subreddit_id
          AND slug = NEW.slug
          AND id <> NEW.id
    ) LOOP
        NEW.slug := base || '-' || suffix;
        suffix := suffix + 1;
    END LOOP;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER posts_resolve_slug_insert
    BEFORE INSERT ON posts
    FOR EACH ROW EXECUTE FUNCTION posts_resolve_slug();

-- Only when the slug or the subreddit changes: an edit that keeps both must
-- not rename the post.
CREATE TRIGGER posts_resolve_slug_update
    BEFORE UPDATE OF slug, subreddit_id ON posts
    FOR EACH ROW
    WHEN (NEW.slug IS DISTINCT FROM OLD.slug
          OR NEW.subreddit_id IS DISTINCT FROM OLD.subreddit_id)
    EXECUTE FUNCTION posts_resolve_slug();
