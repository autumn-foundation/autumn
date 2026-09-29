-- Shim, SQLite variant. `_autumn_shard_directory` is not created here.
-- SQLite deployments cannot shard: `sqlite_sharding_unsupported_guard`
-- refuses a `sqlite://` control target with sharding configured. This
-- shim keeps the version in step with the Postgres set.
SELECT 1;
