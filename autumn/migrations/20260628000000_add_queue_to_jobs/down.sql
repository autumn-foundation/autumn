DROP INDEX IF EXISTS idx_autumn_jobs_queue_ready;
ALTER TABLE autumn_jobs DROP COLUMN IF EXISTS queue;
