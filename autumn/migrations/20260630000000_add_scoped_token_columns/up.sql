-- Scoped service tokens (#1158): extend the managed api_tokens table with a
-- human-readable name, granted scopes, an optional expiry, and last-used
-- tracking. All columns are additive with safe defaults so existing rows and
-- the legacy issue/verify/revoke paths keep working unchanged.
ALTER TABLE api_tokens ADD COLUMN IF NOT EXISTS name TEXT NOT NULL DEFAULT '';
ALTER TABLE api_tokens ADD COLUMN IF NOT EXISTS scopes JSONB NOT NULL DEFAULT '[]'::jsonb;
ALTER TABLE api_tokens ADD COLUMN IF NOT EXISTS expires_at TIMESTAMP;
ALTER TABLE api_tokens ADD COLUMN IF NOT EXISTS last_used_at TIMESTAMP;
