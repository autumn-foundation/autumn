-- Feature flags, SQLite variant. Backend-forked from the Postgres migration
-- in `autumn/migrations/`. The version dir name matches Postgres. This keeps
-- `__diesel_schema_migrations` bookkeeping the same across backends.
--
-- Differences from the Postgres DDL:
--   * `id INTEGER PRIMARY KEY` is a rowid alias. SQLite autoincrements it.
--     `BIGSERIAL` only gets NUMERIC affinity on SQLite, not autoincrement;
--   * `created_at`/`updated_at`/`changed_at` are TEXT — SQLite has no
--     `TIMESTAMPTZ`, and `NOW()` is not a SQLite function;
--     `CURRENT_TIMESTAMP` is the fallback default;
--   * `COMMENT ON COLUMN` is dropped — SQLite has no column comments;
--   * the `pg_notify` trigger is dropped — SQLite has no LISTEN/NOTIFY.
--     There is no `SQLite` feature-flags store yet to read a notify
--     channel, so this drops cache-invalidation reach, not behavior.
--
-- See the Postgres file for what each table is for.

CREATE TABLE IF NOT EXISTS autumn_feature_flags (
    id               INTEGER PRIMARY KEY,
    key              TEXT    NOT NULL UNIQUE,
    description      TEXT,
    enabled          BOOLEAN NOT NULL DEFAULT FALSE,
    rollout_pct      INTEGER NOT NULL DEFAULT 0 CHECK (rollout_pct BETWEEN 0 AND 100),
    actor_allowlist  TEXT    NOT NULL DEFAULT '[]',
    group_allowlist  TEXT    NOT NULL DEFAULT '[]',
    created_at       TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at       TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS idx_autumn_feature_flags_key
    ON autumn_feature_flags (key);

CREATE TABLE IF NOT EXISTS feature_flag_changes (
    id          INTEGER PRIMARY KEY,
    key         TEXT    NOT NULL,
    mutation    TEXT    NOT NULL,
    actor       TEXT,
    changed_at  TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS idx_feature_flag_changes_key_time
    ON feature_flag_changes (key, changed_at DESC);
