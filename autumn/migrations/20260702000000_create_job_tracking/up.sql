CREATE TABLE IF NOT EXISTS autumn_job_tracking (
    key        TEXT        PRIMARY KEY,
    record     JSONB       NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at TIMESTAMPTZ NOT NULL
);

-- Lazy-expiry reads and cleanup both filter/scan by expiry.
CREATE INDEX IF NOT EXISTS idx_autumn_job_tracking_expires_at
    ON autumn_job_tracking (expires_at);
