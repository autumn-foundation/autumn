CREATE TABLE IF NOT EXISTS autumn_jobs (
    id                TEXT        PRIMARY KEY,
    name              TEXT        NOT NULL,
    payload           JSONB       NOT NULL DEFAULT '{}',
    status            TEXT        NOT NULL DEFAULT 'enqueued',
    attempt           INTEGER     NOT NULL DEFAULT 1,
    max_attempts      INTEGER     NOT NULL DEFAULT 5,
    initial_backoff_ms BIGINT     NOT NULL DEFAULT 250,
    enqueued_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    started_at        TIMESTAMPTZ,
    finished_at       TIMESTAMPTZ,
    claimed_by        TEXT,
    claimed_at        TIMESTAMPTZ,
    last_error        TEXT,
    run_at            TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Fast path for workers polling for ready jobs
CREATE INDEX IF NOT EXISTS idx_autumn_jobs_ready
    ON autumn_jobs (run_at ASC)
    WHERE status = 'enqueued';

-- Dashboard queries filter by status and finished_at
CREATE INDEX IF NOT EXISTS idx_autumn_jobs_status_finished
    ON autumn_jobs (status, finished_at DESC);

-- Dashboard enqueued-tab sorted by enqueued_at
CREATE INDEX IF NOT EXISTS idx_autumn_jobs_enqueued_dashboard
    ON autumn_jobs (enqueued_at DESC)
    WHERE status = 'enqueued';

-- Stale-claim recovery scans running jobs by claimed_at
CREATE INDEX IF NOT EXISTS idx_autumn_jobs_stale_recovery
    ON autumn_jobs (claimed_at)
    WHERE status = 'running';
ALTER TABLE autumn_jobs ADD COLUMN IF NOT EXISTS traceparent TEXT;
ALTER TABLE autumn_jobs ADD COLUMN IF NOT EXISTS tracestate  TEXT;
-- Additive columns for #[job] uniqueness keys and concurrency limits (#829).
-- All columns are nullable so existing rows and jobs without the new
-- attributes behave exactly as before.
ALTER TABLE autumn_jobs ADD COLUMN IF NOT EXISTS unique_key        TEXT;
ALTER TABLE autumn_jobs ADD COLUMN IF NOT EXISTS unique_window     TEXT;
ALTER TABLE autumn_jobs ADD COLUMN IF NOT EXISTS concurrency_key   TEXT;
ALTER TABLE autumn_jobs ADD COLUMN IF NOT EXISTS concurrency_limit INTEGER;

-- Distributed dedup backstop: at most one in-flight job per (name, unique_key).
-- Enqueue uses ON CONFLICT DO NOTHING against this index, so two racing
-- enqueues across app instances coalesce to a single row.
CREATE UNIQUE INDEX IF NOT EXISTS idx_autumn_jobs_unique_inflight
    ON autumn_jobs (name, unique_key)
    WHERE unique_key IS NOT NULL AND status IN ('enqueued', 'running');

-- Claim-time concurrency checks count running jobs per (name, concurrency_key).
CREATE INDEX IF NOT EXISTS idx_autumn_jobs_concurrency_running
    ON autumn_jobs (name, concurrency_key)
    WHERE status = 'running';
-- Preserve the unique_key across the claim→timeout→stale-recovery round-trip
-- for jobs with unique_window = 'pending'.
--
-- When a pending-window job is claimed the dedup key is cleared (unique_key = NULL)
-- so a fresh duplicate can be enqueued while the job runs.  If the worker dies the
-- stale-recovery path re-enqueues the row without the key, silently breaking dedup.
-- This column lets the claim SQL save the key before clearing it; stale recovery
-- reads it back and restores unique_key on the re-enqueued row.
ALTER TABLE autumn_jobs ADD COLUMN IF NOT EXISTS pending_unique_key TEXT;
-- Named job queues (#1053). Additive, backward-compatible: the column defaults
-- to 'default' so existing rows and apps that don't opt into priority queues
-- behave exactly as before. NOT NULL is safe because the DEFAULT backfills
-- every existing row at ALTER time.
ALTER TABLE autumn_jobs ADD COLUMN IF NOT EXISTS queue TEXT NOT NULL DEFAULT 'default';

-- Workers claim the highest-priority non-empty queue first; this partial index
-- keeps the per-queue, ready-ordered scan cheap.
CREATE INDEX IF NOT EXISTS idx_autumn_jobs_queue_ready
    ON autumn_jobs (queue, run_at)
    WHERE status = 'enqueued';
