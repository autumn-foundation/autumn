CREATE TABLE IF NOT EXISTS autumn_migration_checksums (
    version    TEXT PRIMARY KEY,
    checksum   TEXT NOT NULL,
    algorithm  TEXT NOT NULL DEFAULT 'sha256',
    recorded_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
