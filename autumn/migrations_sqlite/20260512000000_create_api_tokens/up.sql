-- API tokens, SQLite variant. Backend-forked from the Postgres migration in
-- `autumn/migrations/`. The version dir name is kept identical so
-- `__diesel_schema_migrations` bookkeeping does not diverge across backends.
-- Differences from the Postgres DDL:
--   * `id INTEGER PRIMARY KEY` — a rowid alias that autoincrements
--     (`BIGSERIAL` has mere NUMERIC affinity on SQLite);
--   * `created_at`/`revoked_at` are TEXT — SQLite has no timestamp type, and
--     `NOW()` is not a SQLite function; `CURRENT_TIMESTAMP` is the fallback
--     default.

CREATE TABLE IF NOT EXISTS api_tokens (
    id           INTEGER PRIMARY KEY,
    token_hash   TEXT    NOT NULL UNIQUE,
    principal_id TEXT    NOT NULL,
    created_at   TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP,
    revoked_at   TEXT
);

CREATE INDEX IF NOT EXISTS idx_api_tokens_hash ON api_tokens (token_hash);
