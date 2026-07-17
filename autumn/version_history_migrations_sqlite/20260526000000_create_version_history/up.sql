-- Automatic record version history table (issue #700), SQLite variant (#1996).
--
-- Backend-forked from the Postgres migration in
-- `version_history_migrations/`. The version dir name is kept identical so
-- `__diesel_schema_migrations` bookkeeping and `framework_migration_versions()`
-- do not diverge across backends. Differences from the Postgres DDL:
--   * `id INTEGER PRIMARY KEY` — a rowid alias that autoincrements (SQLite gives
--     `BIGSERIAL` mere NUMERIC affinity, so the id-less INSERT would write NULL);
--   * `changes TEXT` — SQLite has no `JSONB` type; JSON is stored as TEXT;
--   * `recorded_at TEXT DEFAULT CURRENT_TIMESTAMP` — `NOW()` is not a SQLite
--     function. The repository writer binds `recorded_at` explicitly, so the
--     default is only a fallback for direct inserts;
--   * no `ALTER TABLE ... ADD COLUMN IF NOT EXISTS tenant_id` — SQLite `ADD
--     COLUMN` has no `IF NOT EXISTS`, and `tenant_id` is already in the CREATE
--     (the ALTER only existed to retrofit pre-tenant Postgres deployments).

CREATE TABLE IF NOT EXISTS _autumn_version_history (
    id          INTEGER PRIMARY KEY,
    table_name  TEXT    NOT NULL,
    tenant_id   TEXT,
    record_id   BIGINT  NOT NULL,
    op          TEXT    NOT NULL CHECK (op IN ('insert', 'update', 'delete')),
    actor       TEXT    NOT NULL DEFAULT 'system',
    request_id  TEXT,
    changes     TEXT    NOT NULL DEFAULT '[]',
    recorded_at TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS idx_autumn_version_history_record
    ON _autumn_version_history (table_name, record_id, recorded_at ASC);

CREATE INDEX IF NOT EXISTS idx_autumn_version_history_tenant_record
    ON _autumn_version_history (table_name, tenant_id, record_id, recorded_at ASC);

CREATE INDEX IF NOT EXISTS idx_autumn_version_history_time
    ON _autumn_version_history (table_name, recorded_at ASC);

CREATE INDEX IF NOT EXISTS idx_autumn_version_history_actor
    ON _autumn_version_history (actor, recorded_at DESC);
