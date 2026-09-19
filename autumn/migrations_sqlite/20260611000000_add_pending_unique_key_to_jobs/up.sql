-- Shim, SQLite variant. `pending_unique_key` is not added here.
-- `job/sqlite.rs::ensure_schema` already creates `autumn_jobs` with it.
-- This shim keeps the version in step with the Postgres set.
SELECT 1;
