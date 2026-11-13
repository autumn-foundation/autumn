-- Serves the TTL-window dedup check of a unique job enqueue: `NOT EXISTS
-- (... WHERE name = $2 AND unique_key = $6 AND enqueued_at > NOW() - ttl)`.
-- That check has no status filter (a finished job still holds its key until
-- the window passes), so `idx_autumn_jobs_unique_inflight`, which holds only
-- in-flight rows, cannot serve it, and every TTL enqueue read the whole table,
-- finished history included. Retention keeps TTL-keyed rows, so that history
-- only grows.
--
-- Only rows with a unique key are indexed. Plain `CREATE INDEX` takes a SHARE
-- lock on `autumn_jobs` for the build, so enqueue/claim/ack writes wait until
-- it finishes.
CREATE INDEX IF NOT EXISTS idx_autumn_jobs_unique_key_enqueued
    ON autumn_jobs (name, unique_key, enqueued_at)
    WHERE unique_key IS NOT NULL;
