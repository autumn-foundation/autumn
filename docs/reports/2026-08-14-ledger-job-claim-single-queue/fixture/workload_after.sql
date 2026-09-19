\timing off
DO $$
DECLARE
  i INT;
BEGIN
  FOR i IN 1..50 LOOP
    UPDATE autumn_jobs
    SET status = 'running', started_at = NOW(), claimed_by = 'worker-bench', claimed_at = NOW(),
        pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
        unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
    WHERE id = (
      SELECT candidate.id FROM autumn_jobs candidate
      WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
        AND candidate.queue = 'default'
        AND (candidate.concurrency_limit IS NULL OR (
          SELECT COUNT(*) FROM autumn_jobs running
          WHERE running.status = 'running'
            AND running.name = candidate.name
            AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
        ) < candidate.concurrency_limit)
      ORDER BY candidate.run_at ASC
      LIMIT 1
      FOR UPDATE SKIP LOCKED
    );
  END LOOP;
END $$;
