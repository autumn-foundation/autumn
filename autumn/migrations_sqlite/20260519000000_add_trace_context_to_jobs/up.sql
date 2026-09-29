-- Shim, SQLite variant. `traceparent`/`tracestate` are not added here.
-- `job/sqlite.rs::ensure_schema` already creates `autumn_jobs` with both
-- columns. This shim keeps the version in step with the Postgres set.
SELECT 1;
