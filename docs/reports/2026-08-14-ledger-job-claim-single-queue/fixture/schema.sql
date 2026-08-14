-- autumn_jobs schema, assembled from autumn/migrations/20260513000000_create_job_queue,
-- 20260519000000_add_trace_context_to_jobs, 20260610000000_add_job_uniqueness_concurrency,
-- 20260611000000_add_pending_unique_key_to_jobs, and 20260628000000_add_queue_to_jobs.
-- Used to stand up a throwaway Postgres database for the job-claim benchmark without
-- needing to boot the full autumn-web migration runner.
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
    run_at            TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    traceparent       TEXT,
    tracestate        TEXT,
    unique_key        TEXT,
    unique_window     TEXT,
    concurrency_key   TEXT,
    concurrency_limit INTEGER,
    pending_unique_key TEXT,
    queue             TEXT NOT NULL DEFAULT 'default'
);

CREATE INDEX IF NOT EXISTS idx_autumn_jobs_ready
    ON autumn_jobs (run_at ASC)
    WHERE status = 'enqueued';

CREATE INDEX IF NOT EXISTS idx_autumn_jobs_status_finished
    ON autumn_jobs (status, finished_at DESC);

CREATE INDEX IF NOT EXISTS idx_autumn_jobs_enqueued_dashboard
    ON autumn_jobs (enqueued_at DESC)
    WHERE status = 'enqueued';

CREATE INDEX IF NOT EXISTS idx_autumn_jobs_stale_recovery
    ON autumn_jobs (claimed_at)
    WHERE status = 'running';

CREATE UNIQUE INDEX IF NOT EXISTS idx_autumn_jobs_unique_inflight
    ON autumn_jobs (name, unique_key)
    WHERE unique_key IS NOT NULL AND status IN ('enqueued', 'running');

CREATE INDEX IF NOT EXISTS idx_autumn_jobs_concurrency_running
    ON autumn_jobs (name, concurrency_key)
    WHERE status = 'running';

CREATE INDEX IF NOT EXISTS idx_autumn_jobs_queue_ready
    ON autumn_jobs (queue, run_at)
    WHERE status = 'enqueued';
