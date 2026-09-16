-- Reverse the version history migration.
-- WARNING: this permanently destroys all captured history.
DROP TABLE IF EXISTS _autumn_version_history;
