BEGIN;
EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS)
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
  ),
  heads AS (
    SELECT names.n, h.head, row_number() OVER (ORDER BY h.head, names.n) AS i FROM names
    CROSS JOIN LATERAL (SELECT run_at AS head FROM autumn_jobs
      WHERE status = 'enqueued' AND queue = 'default' AND name = names.n AND run_at <= NOW()
      ORDER BY run_at LIMIT 1) h
    WHERE NOT names.skip
  ),
  walk(i, best_at, best_id) AS (
    SELECT 0::bigint, NULL::timestamptz, NULL::text WHERE EXISTS (SELECT 1 FROM heads)
    UNION ALL
    SELECT heads.i, COALESCE(p.run_at, walk.best_at), COALESCE(p.id, walk.best_id)
    FROM walk
    JOIN heads ON heads.i = walk.i + 1 AND (walk.best_at IS NULL OR heads.head < walk.best_at)
    LEFT JOIN LATERAL (
      SELECT candidate.id, candidate.run_at FROM autumn_jobs candidate
      WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
        AND (walk.best_at IS NULL OR candidate.run_at < walk.best_at)
        AND candidate.queue = 'default' AND candidate.name = heads.n
        AND (candidate.concurrency_limit IS NULL OR (SELECT COUNT(*) FROM autumn_jobs running
          WHERE running.status = 'running' AND running.name = candidate.name
            AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key) < candidate.concurrency_limit)
      ORDER BY candidate.run_at ASC LIMIT 1 FOR UPDATE SKIP LOCKED) p ON true
  )
  SELECT best_id FROM walk WHERE best_id IS NOT NULL ORDER BY i DESC LIMIT 1)
RETURNING id;
ROLLBACK;
