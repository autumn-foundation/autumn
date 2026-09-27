-- Shim, SQLite variant. These indexes serve the data-retention sweep,
-- which is Postgres-only (`data_retention.rs` is
-- `#[cfg(all(feature = "db", not(feature = "sqlite")))]`). This shim
-- keeps the version in step with the Postgres set.
SELECT 1;
