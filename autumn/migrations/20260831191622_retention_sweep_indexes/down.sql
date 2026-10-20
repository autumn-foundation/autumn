-- idx_autumn_commit_hooks_finished is also created by the standalone
-- commit-hook migration set. It stays here so a rollback of either set
-- cannot remove an index the other set still needs.
DROP INDEX IF EXISTS idx_autumn_exp_assignments_assigned_at;
DROP INDEX IF EXISTS idx_autumn_job_tracking_updated_at;
