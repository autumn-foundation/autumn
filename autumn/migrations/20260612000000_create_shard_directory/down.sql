DROP TRIGGER IF EXISTS autumn_shard_directory_notify ON _autumn_shard_directory;
DROP FUNCTION IF EXISTS autumn_notify_shard_directory_change();
-- Pre-LISTEN builds also created a change-log table; drop it if present so the
-- down migration is valid against either schema version.
DROP TABLE IF EXISTS _autumn_shard_directory_changes;
DROP TABLE IF EXISTS _autumn_shard_directory;
