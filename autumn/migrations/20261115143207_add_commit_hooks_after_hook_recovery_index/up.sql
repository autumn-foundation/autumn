-- Serves the stale after-hook recovery that every commit-hook worker runs on
-- each 250 ms tick:
--   UPDATE ... WHERE status IN ('pending_after_hook', 'after_hook_succeeded')
--     AND claimed_at < NOW() - <stale window>
-- This set's idx_autumn_repository_commit_hooks_pending_recovery holds only
-- 'pending_after_hook' rows, so it cannot serve the two-status predicate, and
-- every tick read the whole table, finished history included. Retention keeps
-- that history forever by default, so the scan only grows.
--
-- The standalone repository_commit_hook_migrations set already ships the
-- two-status predicate under the pending_recovery name. A database that ran
-- that set first has it, so the index is made only when no claimed_at index
-- on the table covers 'after_hook_succeeded'.
--
-- It holds only rows mid-hook, a handful at a time. Plain CREATE INDEX takes a
-- SHARE lock on autumn_repository_commit_hooks for the build (one heap scan),
-- so hook enqueue, claim and ack writes wait until it finishes.
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_indexes
         WHERE schemaname = current_schema()
           AND tablename = 'autumn_repository_commit_hooks'
           AND indexdef LIKE '%(claimed_at)%'
           AND indexdef LIKE '%after_hook_succeeded%'
    ) THEN
        CREATE INDEX IF NOT EXISTS idx_autumn_repository_commit_hooks_after_hook_recovery
            ON autumn_repository_commit_hooks (claimed_at)
            WHERE status IN ('pending_after_hook', 'after_hook_succeeded');
    END IF;
END
$$;
