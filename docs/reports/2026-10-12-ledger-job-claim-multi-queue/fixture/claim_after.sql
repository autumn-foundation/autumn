-- Multi-queue claim (after). psql vars: worker, queues, deferred.
-- Mirrors pg_claim_sql() in autumn/src/job.rs.
UPDATE autumn_jobs
SET status = 'running', started_at = NOW(), claimed_by = :'worker', claimed_at = NOW(),
    pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
    unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  SELECT picked.id
  FROM unnest(:queues::text[]) WITH ORDINALITY AS q(queue, ord)
  CROSS JOIN LATERAL (
    SELECT candidate.id, candidate.run_at FROM autumn_jobs candidate
    WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
      AND candidate.queue = q.queue
      AND (candidate.concurrency_limit IS NULL OR (
        SELECT COUNT(*) FROM autumn_jobs running
        WHERE running.status = 'running' AND running.name = candidate.name
          AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
      ) < candidate.concurrency_limit)
      AND NOT (candidate.name = ANY(:deferred::text[]))
    ORDER BY candidate.run_at ASC
    LIMIT 1 FOR UPDATE SKIP LOCKED
  ) picked
  ORDER BY q.ord, picked.run_at
  LIMIT 1
)
RETURNING id, queue, run_at;
