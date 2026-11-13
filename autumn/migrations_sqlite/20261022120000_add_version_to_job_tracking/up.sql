-- Shim, SQLite variant. `SqliteJobTrackingStore` owns this table and has
-- its own `version` column. This shim keeps the version in step with Postgres.
SELECT 1;
