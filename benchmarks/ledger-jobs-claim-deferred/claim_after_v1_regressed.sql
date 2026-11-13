UPDATE autumn_jobs SET status = 'running', started_at = NOW(), claimed_by = 'w1', claimed_at = NOW(),
  pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
  unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  WITH RECURSIVE names(n, skip) AS (
    (SELECT name, name = ANY('{report.slow}'::text[]) FROM autumn_jobs
      WHERE status = 'enqueued' AND queue = 'default' ORDER BY name LIMIT 1)
    UNION ALL
    SELECT nx.name, nx.name = ANY('{report.slow}'::text[]) FROM names
      CROSS JOIN LATERAL (SELECT name FROM autumn_jobs
        WHERE status = 'enqueued' AND queue = 'default' AND name > names.n ORDER BY name LIMIT 1) nx
  )
  SELECT picked.id FROM names
  CROSS JOIN LATERAL (
    SELECT candidate.id, candidate.run_at FROM autumn_jobs candidate
    WHERE NOT names.skip AND candidate.status = 'enqueued' AND candidate.run_at <= NOW()
      AND candidate.queue = 'default' AND candidate.name = names.n
      AND (candidate.concurrency_limit IS NULL OR (SELECT COUNT(*) FROM autumn_jobs running
        WHERE running.status = 'running' AND running.name = candidate.name
          AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key) < candidate.concurrency_limit)
    ORDER BY candidate.run_at ASC LIMIT 1 FOR UPDATE SKIP LOCKED) picked
  ORDER BY picked.run_at LIMIT 1)
RETURNING id AS jid;
