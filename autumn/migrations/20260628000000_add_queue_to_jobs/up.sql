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
