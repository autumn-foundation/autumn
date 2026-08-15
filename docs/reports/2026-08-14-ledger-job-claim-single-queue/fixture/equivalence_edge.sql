-- Edge case: the single lowest-run_at ready row is concurrency-blocked (its
-- name is already at concurrency_limit=1 with a running job). Both variants
-- must skip it and pick the same next-lowest eligible row.
BEGIN;
DELETE FROM autumn_jobs WHERE id IN ('edge-blocked', 'edge-next', 'edge-running');
INSERT INTO autumn_jobs (id, name, queue, status, run_at, enqueued_at, concurrency_key, concurrency_limit)
VALUES
  ('edge-blocked', 'reconcile_ledger', 'default', 'enqueued', NOW() - interval '1 hour', NOW(), 'reconcile_ledger', 1),
  ('edge-running', 'reconcile_ledger', 'default', 'running', NOW() - interval '2 hours', NOW(), 'reconcile_ledger', 1),
  ('edge-next', 'send_email', 'default', 'enqueued', NOW() - interval '59 minutes', NOW(), NULL, NULL);
UPDATE autumn_jobs SET status = 'running', claimed_by = 'edge-setup', claimed_at = NOW()
  WHERE id = 'edge-running';

SELECT 'BEFORE, edge case' AS variant;
SELECT candidate.id FROM autumn_jobs candidate
WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
  AND candidate.queue = ANY(ARRAY['default'])
  AND (candidate.concurrency_limit IS NULL OR (
    SELECT COUNT(*) FROM autumn_jobs running
    WHERE running.status = 'running' AND running.name = candidate.name
      AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
  ) < candidate.concurrency_limit)
  AND candidate.id IN ('edge-blocked', 'edge-next')
ORDER BY array_position(ARRAY['default'], candidate.queue), candidate.run_at ASC
LIMIT 1
FOR UPDATE SKIP LOCKED;

SELECT 'AFTER, edge case' AS variant;
SELECT candidate.id FROM autumn_jobs candidate
WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
  AND candidate.queue = 'default'
  AND (candidate.concurrency_limit IS NULL OR (
    SELECT COUNT(*) FROM autumn_jobs running
    WHERE running.status = 'running' AND running.name = candidate.name
      AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
  ) < candidate.concurrency_limit)
  AND candidate.id IN ('edge-blocked', 'edge-next')
ORDER BY candidate.run_at ASC
LIMIT 1
FOR UPDATE SKIP LOCKED;
ROLLBACK;
