-- Durable repository commit-hook queue, SQLite variant. Backend-forked from
-- the control-plane Postgres migration in `autumn/migrations/`.
--
-- Note: not forked from `repository_commit_hook_migrations/`. That fork's
-- own `_sqlite` sibling has since drifted from this one: its
-- `idx_..._pending_recovery` index also covers `after_hook_succeeded`; this
-- copy does not yet.
--
-- The version dir name matches Postgres. This keeps
-- `__diesel_schema_migrations` bookkeeping the same across backends.
--
-- Differences from the Postgres DDL:
--   * `context`/`record` are TEXT — SQLite has no `JSONB` type;
--   * every `TIMESTAMPTZ` becomes TEXT, and `NOW()` becomes
--     `CURRENT_TIMESTAMP` — neither exists on SQLite;
--   * the partial indexes are kept verbatim — SQLite supports them
--     (>= 3.8.0).

CREATE TABLE IF NOT EXISTS autumn_repository_commit_hooks (
    id                  TEXT    PRIMARY KEY,
    handler_key         TEXT    NOT NULL,
    hook_name           TEXT    NOT NULL,
    context             TEXT    NOT NULL DEFAULT '{}',
    record              TEXT    NOT NULL DEFAULT '{}',
    status              TEXT    NOT NULL DEFAULT 'enqueued',
    attempt             INTEGER NOT NULL DEFAULT 1,
    max_attempts        INTEGER NOT NULL DEFAULT 5,
    initial_backoff_ms  BIGINT  NOT NULL DEFAULT 1000,
    enqueued_at         TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP,
    started_at          TEXT,
    finished_at         TEXT,
    claimed_by          TEXT,
    claimed_at          TEXT,
    last_error          TEXT,
    run_at              TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- Workers claim ready rows under a BEGIN IMMEDIATE write lock (SQLite is
-- single-writer, so there is no FOR UPDATE SKIP LOCKED).
CREATE INDEX IF NOT EXISTS idx_autumn_repository_commit_hooks_ready
    ON autumn_repository_commit_hooks (run_at ASC, enqueued_at ASC)
    WHERE status = 'enqueued';

-- Dispatchers only claim hooks whose generated runner is registered locally.
CREATE INDEX IF NOT EXISTS idx_autumn_repository_commit_hooks_handler_ready
    ON autumn_repository_commit_hooks (handler_key, run_at ASC)
    WHERE status = 'enqueued';

-- Stale-claim recovery lets another replica retry work abandoned by a dead worker.
CREATE INDEX IF NOT EXISTS idx_autumn_repository_commit_hooks_stale_recovery
    ON autumn_repository_commit_hooks (claimed_at)
    WHERE status = 'running';

-- Staged create/update hooks are leased by the request until regular after hooks finish.
CREATE INDEX IF NOT EXISTS idx_autumn_repository_commit_hooks_pending_recovery
    ON autumn_repository_commit_hooks (claimed_at)
    WHERE status = 'pending_after_hook';
