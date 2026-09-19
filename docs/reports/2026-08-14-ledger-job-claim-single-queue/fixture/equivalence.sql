-- Result-equivalence check: both claim query variants must pick the same row
-- on the same data (array_position over a 1-element array is constant, so the
-- ORDER BY reduces to run_at ASC in both cases). Run each in a rolled-back
-- transaction so the fixture is unmodified between comparisons.
BEGIN;
SELECT 'BEFORE (array_position, queue = ANY($2))' AS variant;
SELECT candidate.id, candidate.run_at, candidate.name FROM autumn_jobs candidate
WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
  AND candidate.queue = ANY(ARRAY['default'])
  AND (candidate.concurrency_limit IS NULL OR (
    SELECT COUNT(*) FROM autumn_jobs running
    WHERE running.status = 'running' AND running.name = candidate.name
      AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
  ) < candidate.concurrency_limit)
ORDER BY array_position(ARRAY['default'], candidate.queue), candidate.run_at ASC
LIMIT 1
FOR UPDATE SKIP LOCKED;
ROLLBACK;

BEGIN;
SELECT 'AFTER (queue = $2 scalar, run_at only)' AS variant;
SELECT candidate.id, candidate.run_at, candidate.name FROM autumn_jobs candidate
WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
  AND candidate.queue = 'default'
  AND (candidate.concurrency_limit IS NULL OR (
    SELECT COUNT(*) FROM autumn_jobs running
    WHERE running.status = 'running' AND running.name = candidate.name
      AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
  ) < candidate.concurrency_limit)
ORDER BY candidate.run_at ASC
LIMIT 1
FOR UPDATE SKIP LOCKED;
ROLLBACK;

-- Sanity: how many rows tie for the minimum run_at (must be 1 for a clean
-- single-winner comparison; a fixture with >1 tied rows would make "same id"
-- too strong a check, since SKIP LOCKED is not order-deterministic among ties).
SELECT count(*) AS rows_at_min_run_at FROM autumn_jobs
WHERE status = 'enqueued' AND queue = 'default' AND run_at = (
  SELECT min(run_at) FROM autumn_jobs WHERE status = 'enqueued' AND queue = 'default' AND run_at <= NOW()
);
