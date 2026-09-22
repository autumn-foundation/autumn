-- Shim, SQLite variant. The uniqueness/concurrency columns and indexes are
-- not added here. `job/sqlite.rs::ensure_schema` already creates
-- `autumn_jobs` with them. This shim keeps the version in step with the
-- Postgres set.
SELECT 1;
