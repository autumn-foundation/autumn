-- Serves the cost-deferral claim (issue #1720): while deferrable job names are
-- parked, the claim walks the ready names with an index-only skip scan and
-- probes `(queue, name, run_at)` once per runnable name, instead of filtering
-- the parked names' whole backlog out of `idx_autumn_jobs_queue_ready` row by
-- row (O(parked backlog) buffers per poll).
--
-- Plain `CREATE INDEX` takes a SHARE lock on `autumn_jobs` for the build, so
-- enqueue/claim/ack writes wait until it finishes. The ready set is the only
-- part indexed, so the build reads the heap once and writes an index the size
-- of the ready backlog, not of the finished history.
CREATE INDEX IF NOT EXISTS idx_autumn_jobs_queue_name_ready
    ON autumn_jobs (queue, name, run_at)
    WHERE status = 'enqueued';
