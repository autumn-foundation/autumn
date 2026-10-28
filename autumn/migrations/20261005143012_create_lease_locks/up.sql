-- Fencing lease locks (issue #3053). One row per lock name. The row stays
-- after release, so `generation` never goes down. The runtime also runs this
-- statement on first use, so it must stay idempotent.
CREATE TABLE IF NOT EXISTS autumn_lease_locks (
    name        TEXT        PRIMARY KEY,
    owner       TEXT,
    generation  BIGINT      NOT NULL CHECK (generation > 0),
    expires_at  TIMESTAMPTZ NOT NULL,
    acquired_at TIMESTAMPTZ NOT NULL
);
