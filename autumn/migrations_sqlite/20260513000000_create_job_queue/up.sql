-- Shim, SQLite variant. `autumn_jobs` is not created here.
-- `job/sqlite.rs::ensure_schema` creates and owns this table on SQLite.
-- This shim keeps the version in step with the Postgres set.
SELECT 1;
