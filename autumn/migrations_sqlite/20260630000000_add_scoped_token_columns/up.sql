-- Scoped service tokens (#1158), SQLite variant. Backend-forked from the
-- Postgres migration in `autumn/migrations/`. Differences from the Postgres
-- DDL:
--   * no `IF NOT EXISTS` on `ADD COLUMN` — SQLite's `ADD COLUMN` has no such
--     clause; a version only ever runs once, so it is not needed for
--     idempotency;
--   * `scopes` is TEXT — SQLite has no `JSONB` type;
--   * `expires_at`/`last_used_at` are TEXT — SQLite has no timestamp type.
ALTER TABLE api_tokens ADD COLUMN name TEXT NOT NULL DEFAULT '';
ALTER TABLE api_tokens ADD COLUMN scopes TEXT NOT NULL DEFAULT '[]';
ALTER TABLE api_tokens ADD COLUMN expires_at TEXT;
ALTER TABLE api_tokens ADD COLUMN last_used_at TEXT;
