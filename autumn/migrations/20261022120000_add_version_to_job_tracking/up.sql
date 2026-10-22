-- Write counter for `reset_for_retry`'s compare-and-swap (issue #2581).
-- Every write adds 1. A timestamp can repeat; this cannot.
ALTER TABLE autumn_job_tracking
    ADD COLUMN IF NOT EXISTS version BIGINT NOT NULL DEFAULT 0;
