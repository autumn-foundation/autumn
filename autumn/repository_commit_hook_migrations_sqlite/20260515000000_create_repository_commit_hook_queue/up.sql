-- Durable repository commit-hook queue table, SQLite variant (#1996 item 5).
--
-- Backend-forked from the Postgres migration in
-- `repository_commit_hook_migrations/`. The version dir name is kept identical
-- so `__diesel_schema_migrations` bookkeeping and the framework migration set
-- do not diverge across backends. Differences from the Postgres DDL:
--   * `context`/`record` are `TEXT` — SQLite has no `JSONB` type; the durable
--     worker round-trips the same JSON string it does on Postgres (only the
--     storage type differs), so tenant/scope/actor context is byte-identical;
--   * every `TIMESTAMPTZ` becomes `TEXT` — SQLite has no timestamp type. The
--     worker binds explicit UTC `YYYY-MM-DD HH:MM:SS[.fff]` strings for
--     comparable, lexicographically-orderable timestamps, so `CURRENT_TIMESTAMP`
--     (a UTC `YYYY-MM-DD HH:MM:SS` string) is only a fallback default;
--   * `NOW()` is not a SQLite function, so defaults use `CURRENT_TIMESTAMP`.
-- The partial indexes are preserved verbatim — SQLite supports partial indexes
-- (>= 3.8.0), and they gate the same claim / stale-recovery access paths.

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

-- Stale-claim recovery lets the worker retry work abandoned by a dead process.
CREATE INDEX IF NOT EXISTS idx_autumn_repository_commit_hooks_stale_recovery
    ON autumn_repository_commit_hooks (claimed_at)
    WHERE status = 'running';

-- Staged create/update hooks are leased by the request until regular after hooks finish.
-- `after_hook_succeeded` rows have durably persisted finalized after-hook payloads and may be
-- recovered to `enqueued`; ambiguous `pending_after_hook` rows must fail closed instead.
CREATE INDEX IF NOT EXISTS idx_autumn_repository_commit_hooks_pending_recovery
    ON autumn_repository_commit_hooks (claimed_at)
    WHERE status IN ('pending_after_hook', 'after_hook_succeeded');
