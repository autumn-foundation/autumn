-- Shim, SQLite variant. `autumn_job_tracking` is not created here.
-- `job_tracking.rs::SqliteJobTrackingStore` creates and owns this table
-- on SQLite. This shim keeps the version in step with the Postgres set.
SELECT 1;
