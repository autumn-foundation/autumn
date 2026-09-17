-- Runtime configuration store, SQLite variant. Backend-forked from the
-- Postgres migration in `autumn/migrations/`. The version dir name matches
-- Postgres. This keeps `__diesel_schema_migrations` bookkeeping the same
-- across backends.
--
-- Differences from the Postgres DDL:
--   * `id INTEGER PRIMARY KEY` is a rowid alias. SQLite autoincrements it.
--     `BIGSERIAL` only gets NUMERIC affinity on SQLite, not autoincrement;
--   * `updated_at`/`changed_at` are TEXT — SQLite has no `TIMESTAMPTZ`, and
--     `NOW()` is not a SQLite function; `CURRENT_TIMESTAMP` is the fallback
--     default.
--
-- See the Postgres file for what each table is for.

CREATE TABLE IF NOT EXISTS autumn_runtime_config_values (
    key         TEXT    NOT NULL PRIMARY KEY,
    raw_value   TEXT    NOT NULL,
    updated_at  TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS idx_autumn_runtime_config_values_updated
    ON autumn_runtime_config_values (updated_at DESC);

CREATE TABLE IF NOT EXISTS autumn_runtime_config_changes (
    id          INTEGER PRIMARY KEY,
    key         TEXT    NOT NULL,
    old_value   TEXT,
    new_value   TEXT,
    actor       TEXT,
    changed_at  TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- Fast look-ups for `autumn config history <key>`
CREATE INDEX IF NOT EXISTS idx_autumn_runtime_config_changes_key_time
    ON autumn_runtime_config_changes (key, changed_at DESC);
