-- Bitemporal, tamper-evident record ledger (issue #1699), SQLite variant.
--
-- Backend-forked from the Postgres migration in `version_history_migrations/`,
-- following the same convention as `20260526000000_create_version_history`: the
-- version dir name is kept identical so `__diesel_schema_migrations` bookkeeping
-- and `framework_migration_versions()` do not diverge across backends.
-- Differences from the Postgres DDL:
--   * `id INTEGER PRIMARY KEY` — a rowid alias that autoincrements (SQLite gives
--     `BIGSERIAL` mere NUMERIC affinity, so the id-less INSERT would write NULL);
--   * `snapshot TEXT` — SQLite has no `JSONB` type; JSON is stored as TEXT;
--   * `valid_from` / `recorded_at` are TEXT — the writer always binds them
--     explicitly (there is no server-side default), so stored and filter values
--     share one encoding and the range comparisons stay monotonic;
--   * `IFNULL` rather than `COALESCE` is unnecessary — SQLite has `COALESCE`,
--     and expression indexes are supported, so the unique index is identical.

CREATE TABLE IF NOT EXISTS _autumn_ledger_revisions (
    id          INTEGER PRIMARY KEY,
    table_name  TEXT    NOT NULL,
    tenant_id   TEXT,
    record_id   BIGINT  NOT NULL,
    seq         BIGINT  NOT NULL,
    op          TEXT    NOT NULL CHECK (op IN ('insert', 'update', 'delete')),
    actor       TEXT    NOT NULL DEFAULT 'system',
    request_id  TEXT,
    snapshot    TEXT    NOT NULL DEFAULT '{}',
    valid_from  TEXT    NOT NULL,
    recorded_at TEXT    NOT NULL,
    prev_hash   TEXT,
    hash        TEXT    NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_autumn_ledger_revisions_chain
    ON _autumn_ledger_revisions (table_name, COALESCE(tenant_id, ''), record_id, seq);

CREATE INDEX IF NOT EXISTS idx_autumn_ledger_revisions_record
    ON _autumn_ledger_revisions (table_name, record_id, seq ASC);
