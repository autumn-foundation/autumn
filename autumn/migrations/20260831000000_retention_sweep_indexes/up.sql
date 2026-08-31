-- Indexes supporting the unified framework-owned data-retention sweep (#1605).
--
-- The sweep's per-dataset predicate runs one COUNT plus up to 1000 batched
-- DELETE sub-selects per run. `autumn_jobs` is already covered by
-- idx_autumn_jobs_status_finished, but the other three sweepable tables had no
-- index on the column the sweep filters by, making every run a sequential
-- scan of the whole table -- exactly the table a retention policy exists
-- because it has grown large.
--
-- Partial where the terminal-state filter allows it, so the index stays small
-- and does not slow down the hot insert/claim paths.

CREATE INDEX IF NOT EXISTS idx_autumn_job_tracking_updated_at
    ON autumn_job_tracking (updated_at);

CREATE INDEX IF NOT EXISTS idx_autumn_exp_assignments_assigned_at
    ON autumn_experiment_assignments (assigned_at);

CREATE INDEX IF NOT EXISTS idx_autumn_commit_hooks_finished
    ON autumn_repository_commit_hooks (finished_at)
    WHERE status IN ('completed', 'failed');
