CREATE INDEX IF NOT EXISTS idx_autumn_jobs_queue_name_ready
    ON autumn_jobs (queue, name, run_at)
    WHERE status = 'enqueued';
ANALYZE autumn_jobs;
