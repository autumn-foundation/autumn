-- No `IF EXISTS` on `DROP COLUMN` — SQLite's `DROP COLUMN` has no such
-- clause (supported since SQLite 3.35, the floor this workspace targets).
ALTER TABLE api_tokens DROP COLUMN last_used_at;
ALTER TABLE api_tokens DROP COLUMN expires_at;
ALTER TABLE api_tokens DROP COLUMN scopes;
ALTER TABLE api_tokens DROP COLUMN name;
