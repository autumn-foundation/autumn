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
-- oldest row (lowest `id`) in each duplicate group untouched, and reassign
-- every later duplicate the next free `base-2`/`base-3`/... slug — the same
-- suffix search `unique_slug` itself does, checked against the table's LIVE
-- state (each `UPDATE` below is visible to the next iteration's `EXISTS`,
-- same transaction) rather than assumed unique from the row id alone. A
-- first cut of this migration suffixed with the row's own id instead
-- (`base-dup-<id>`), which does not hold up: an id-based suffix can itself
-- already be taken by some unrelated row (e.g. `foo` duplicated at id=2
-- while an unrelated `foo-2` already exists elsewhere in the same
-- subreddit), which would make the `ADD CONSTRAINT` below fail on exactly
-- the collision this cleanup exists to remove (Codex review on this PR).
-- This never fires on a fresh database (no rows yet) or one that never hit
-- the race.
--
-- During a rolling deploy, an old (pre-fix) process can still be accepting
-- `/submit` while this migration runs. Diesel applies this whole file
-- inside one transaction, but the reconciliation below only takes row-level
-- locks — an old process could commit a brand-new duplicate after the scan
-- but before `ADD CONSTRAINT` takes its lock, failing validation on a row
-- reconciliation never saw (Codex review on this PR). Take a table lock
-- upfront so cleanup and constraint creation share one consistent snapshot.
--
-- That lock has to be `ACCESS EXCLUSIVE` from the start, not the weaker
-- `SHARE ROW EXCLUSIVE` (blocks writers, not readers) an earlier revision of
-- this migration took: `ADD CONSTRAINT` below still needs `ACCESS EXCLUSIVE`
-- regardless, and requesting it from a transaction that already holds a
-- weaker, conflicting-with-others lock is a textbook lock-upgrade deadlock.
-- Postgres's lock queue is fair: if a writer's `ROW EXCLUSIVE` request
-- arrives (and queues) while this transaction holds only `SHARE ROW
-- EXCLUSIVE`, this transaction's later request to upgrade to `ACCESS
-- EXCLUSIVE` queues *behind* that writer — which is itself waiting on this
-- transaction to finish. Neither can proceed; Postgres's deadlock detector
-- eventually kills one of the two with an ugly error (Codex review on this
-- PR). Acquiring `ACCESS EXCLUSIVE` immediately needs no later upgrade, so
-- there is nothing for a subsequent writer to queue ahead of. The cost is
-- that reads block for this transaction's (brief) duration too — no worse
-- than `ADD CONSTRAINT` alone already imposes, just moved earlier.
LOCK TABLE posts IN ACCESS EXCLUSIVE MODE;

DO $$
DECLARE
    dup RECORD;
    candidate TEXT;
    suffix BIGINT;
BEGIN
    FOR dup IN
        SELECT id, subreddit_id, slug AS base_slug
        FROM (
            SELECT id, subreddit_id, slug,
                   row_number() OVER (
                       PARTITION BY subreddit_id, slug ORDER BY id ASC
                   ) AS rn
            FROM posts
        ) ranked
        WHERE rn > 1
        ORDER BY id ASC
    LOOP
        suffix := 2;
        LOOP
            candidate := dup.base_slug || '-' || suffix;
            EXIT WHEN NOT EXISTS (
                SELECT 1 FROM posts
                WHERE subreddit_id = dup.subreddit_id AND slug = candidate
            );
            suffix := suffix + 1;
        END LOOP;
        UPDATE posts SET slug = candidate WHERE id = dup.id;
    END LOOP;
END $$;

ALTER TABLE posts ADD CONSTRAINT posts_subreddit_id_slug_key UNIQUE (subreddit_id, slug);
