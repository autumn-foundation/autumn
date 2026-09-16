-- Shim, SQLite variant. The `queue` column and its index are not added
-- here. `job/sqlite.rs::ensure_schema` already creates `autumn_jobs` with
-- them. This shim keeps the version in step with the Postgres set.
SELECT 1;
