-- Index for the retention sweep (#1605, #2396).
--
-- The full framework set already adds this index. Apps that install only
-- this set need it too. IF NOT EXISTS makes a second run safe.
CREATE INDEX IF NOT EXISTS idx_autumn_commit_hooks_finished
    ON autumn_repository_commit_hooks (finished_at)
    WHERE status IN ('completed', 'failed', 'after_hook_failed');
