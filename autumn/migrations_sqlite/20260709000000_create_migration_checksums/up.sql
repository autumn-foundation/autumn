-- Migration checksum ledger, SQLite variant. Backend-forked from the
-- Postgres migration in `autumn/migrations/`. Only difference: `recorded_at`
-- is TEXT with a `CURRENT_TIMESTAMP` default — SQLite has neither
-- `TIMESTAMPTZ` nor `now()`.
CREATE TABLE IF NOT EXISTS autumn_migration_checksums (
    version     TEXT PRIMARY KEY,
    checksum    TEXT NOT NULL,
    algorithm   TEXT NOT NULL DEFAULT 'sha256',
    recorded_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);
