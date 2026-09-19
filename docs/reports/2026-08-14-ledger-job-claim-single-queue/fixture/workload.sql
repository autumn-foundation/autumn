-- Representative workload: mimics one worker polling the default queue plus
-- occasional admin-dashboard page loads and enqueues, on a mid-size backlog
-- (50k ready-in-default rows across a 552k-row table, production-shaped).
\timing off

-- 200 claims (worker poll loop) -- the ORIGINAL production query.
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
        AND candidate.queue = ANY(ARRAY['default'])
        AND (candidate.concurrency_limit IS NULL OR (
          SELECT COUNT(*) FROM autumn_jobs running
          WHERE running.status = 'running'
            AND running.name = candidate.name
            AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
        ) < candidate.concurrency_limit)
      ORDER BY array_position(ARRAY['default'], candidate.queue), candidate.run_at ASC
      LIMIT 1
      FOR UPDATE SKIP LOCKED
    );
  END LOOP;
END $$;

-- 5 admin dashboard page loads (pg_enqueued_and_scheduled_pages, literal SQL).
DO $$
DECLARE
  i INT;
  r RECORD;
BEGIN
  FOR i IN 1..5 LOOP
    PERFORM (SELECT COALESCE(SUM(CASE WHEN run_at IS NULL OR run_at <= NOW() THEN 1 ELSE 0 END), 0)
             FROM autumn_jobs WHERE status = 'enqueued');
    PERFORM id FROM autumn_jobs
      WHERE status = 'enqueued' AND (run_at IS NULL OR run_at <= NOW())
      ORDER BY enqueued_at DESC NULLS LAST LIMIT 20 OFFSET 0;
    PERFORM id FROM autumn_jobs
      WHERE status = 'enqueued' AND run_at > NOW()
      ORDER BY run_at ASC LIMIT 20 OFFSET 0;
  END LOOP;
END $$;

-- 50 enqueues (JobClient::enqueue insert path, roughly).
DO $$
DECLARE
  i INT;
BEGIN
  FOR i IN 1..50 LOOP
    INSERT INTO autumn_jobs (id, name, queue, status, payload, attempt, max_attempts, enqueued_at, run_at)
    VALUES ('wl-' || gen_random_uuid()::text, 'send_email', 'default', 'enqueued', '{}'::jsonb, 1, 5, NOW(), NOW());
  END LOOP;
END $$;
