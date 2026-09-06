-- #2544: `unique_slug()`/`unique_slug_excluding()` in src/routes/posts.rs
-- guarded a post's slug with a plain SELECT COUNT before the INSERT/UPDATE
-- that used it, and nothing at the database level backed that check. Two
-- concurrent submits could both observe zero existing rows for the same
-- slug and both commit it — a classic check-then-act TOCTOU. Once that
-- happens, `show()`'s `.filter(slug...).filter(subreddit_id...).first()`
-- (an unordered `LIMIT 1`) returns an arbitrary one of the duplicate rows
-- forever, silently serving the wrong post at the other one's permalink.
--
-- `subreddits.slug` and `users.username` already carry a real UNIQUE
-- constraint for exactly this reason (see the initial migration); `posts`
-- was the one uniqueness claim in this schema resting on application logic
-- alone. This backs it with a real composite constraint, scoped the same
-- way the application-level check always was: unique per subreddit, not
-- globally. Application code (`unique_slug`/`unique_slug_excluding`) keeps
-- its SELECT as a fast-path guess, but now retries with the next candidate
-- slug when the database rejects the insert/update as a conflict instead of
-- trusting the guess outright — see `src/routes/posts.rs`.
--
-- An install already hit by #2544 before upgrading may already HAVE
-- duplicate `(subreddit_id, slug)` rows sitting in `posts` — exactly the
-- damage this migration exists to prevent from now on. `ADD CONSTRAINT`
-- validates every existing row, so it would otherwise fail outright on such
-- an install, leaving it unable to complete this migration or boot the
-- fixed application (Codex review on this PR). Reconcile first: keep the
-- oldest row (lowest `id`) in each duplicate group untouched, and suffix
-- every later duplicate with its own `id` — trivially unique, since ids
-- are — so the constraint below can always be added cleanly. This never
-- fires on a fresh database (no rows yet) or one that never hit the race.
WITH duplicates AS (
    SELECT id,
           row_number() OVER (
               PARTITION BY subreddit_id, slug ORDER BY id ASC
           ) AS rn
    FROM posts
)
UPDATE posts
SET slug = posts.slug || '-dup-' || posts.id
FROM duplicates
WHERE posts.id = duplicates.id AND duplicates.rn > 1;

ALTER TABLE posts ADD CONSTRAINT posts_subreddit_id_slug_key UNIQUE (subreddit_id, slug);
