-- 50 claims by one worker with [jobs] queues = high,default,low (the pg_claim_sql path).
\set worker 'worker-bench'
\set queues '''{high,default,low}'''
\set deferred '''{}'''
\o /dev/null
UPDATE autumn_jobs
SET status = 'running', started_at = NOW(), claimed_by = :'worker', claimed_at = NOW(),
    pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
    unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  SELECT candidate.id FROM autumn_jobs candidate
  WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
    AND candidate.queue = ANY(:queues::text[])
    AND (candidate.concurrency_limit IS NULL OR (
      SELECT COUNT(*) FROM autumn_jobs running
      WHERE running.status = 'running' AND running.name = candidate.name
        AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
    ) < candidate.concurrency_limit)
    AND NOT (candidate.name = ANY(:deferred::text[]))
  ORDER BY array_position(:queues::text[], candidate.queue), candidate.run_at ASC
  LIMIT 1 FOR UPDATE SKIP LOCKED
)
RETURNING id, queue, run_at;

UPDATE autumn_jobs
SET status = 'running', started_at = NOW(), claimed_by = :'worker', claimed_at = NOW(),
    pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
    unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  SELECT candidate.id FROM autumn_jobs candidate
  WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
    AND candidate.queue = ANY(:queues::text[])
    AND (candidate.concurrency_limit IS NULL OR (
      SELECT COUNT(*) FROM autumn_jobs running
      WHERE running.status = 'running' AND running.name = candidate.name
        AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
    ) < candidate.concurrency_limit)
    AND NOT (candidate.name = ANY(:deferred::text[]))
  ORDER BY array_position(:queues::text[], candidate.queue), candidate.run_at ASC
  LIMIT 1 FOR UPDATE SKIP LOCKED
)
RETURNING id, queue, run_at;

UPDATE autumn_jobs
SET status = 'running', started_at = NOW(), claimed_by = :'worker', claimed_at = NOW(),
    pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
    unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  SELECT candidate.id FROM autumn_jobs candidate
  WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
    AND candidate.queue = ANY(:queues::text[])
    AND (candidate.concurrency_limit IS NULL OR (
      SELECT COUNT(*) FROM autumn_jobs running
      WHERE running.status = 'running' AND running.name = candidate.name
        AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
    ) < candidate.concurrency_limit)
    AND NOT (candidate.name = ANY(:deferred::text[]))
  ORDER BY array_position(:queues::text[], candidate.queue), candidate.run_at ASC
  LIMIT 1 FOR UPDATE SKIP LOCKED
)
RETURNING id, queue, run_at;

UPDATE autumn_jobs
SET status = 'running', started_at = NOW(), claimed_by = :'worker', claimed_at = NOW(),
    pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
    unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  SELECT candidate.id FROM autumn_jobs candidate
  WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
    AND candidate.queue = ANY(:queues::text[])
    AND (candidate.concurrency_limit IS NULL OR (
      SELECT COUNT(*) FROM autumn_jobs running
      WHERE running.status = 'running' AND running.name = candidate.name
        AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
    ) < candidate.concurrency_limit)
    AND NOT (candidate.name = ANY(:deferred::text[]))
  ORDER BY array_position(:queues::text[], candidate.queue), candidate.run_at ASC
  LIMIT 1 FOR UPDATE SKIP LOCKED
)
RETURNING id, queue, run_at;

UPDATE autumn_jobs
SET status = 'running', started_at = NOW(), claimed_by = :'worker', claimed_at = NOW(),
    pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
    unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  SELECT candidate.id FROM autumn_jobs candidate
  WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
    AND candidate.queue = ANY(:queues::text[])
    AND (candidate.concurrency_limit IS NULL OR (
      SELECT COUNT(*) FROM autumn_jobs running
      WHERE running.status = 'running' AND running.name = candidate.name
        AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
    ) < candidate.concurrency_limit)
    AND NOT (candidate.name = ANY(:deferred::text[]))
  ORDER BY array_position(:queues::text[], candidate.queue), candidate.run_at ASC
  LIMIT 1 FOR UPDATE SKIP LOCKED
)
RETURNING id, queue, run_at;

UPDATE autumn_jobs
SET status = 'running', started_at = NOW(), claimed_by = :'worker', claimed_at = NOW(),
    pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
    unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  SELECT candidate.id FROM autumn_jobs candidate
  WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
    AND candidate.queue = ANY(:queues::text[])
    AND (candidate.concurrency_limit IS NULL OR (
      SELECT COUNT(*) FROM autumn_jobs running
      WHERE running.status = 'running' AND running.name = candidate.name
        AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
    ) < candidate.concurrency_limit)
    AND NOT (candidate.name = ANY(:deferred::text[]))
  ORDER BY array_position(:queues::text[], candidate.queue), candidate.run_at ASC
  LIMIT 1 FOR UPDATE SKIP LOCKED
)
RETURNING id, queue, run_at;

UPDATE autumn_jobs
SET status = 'running', started_at = NOW(), claimed_by = :'worker', claimed_at = NOW(),
    pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
    unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  SELECT candidate.id FROM autumn_jobs candidate
  WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
    AND candidate.queue = ANY(:queues::text[])
    AND (candidate.concurrency_limit IS NULL OR (
      SELECT COUNT(*) FROM autumn_jobs running
      WHERE running.status = 'running' AND running.name = candidate.name
        AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
    ) < candidate.concurrency_limit)
    AND NOT (candidate.name = ANY(:deferred::text[]))
  ORDER BY array_position(:queues::text[], candidate.queue), candidate.run_at ASC
  LIMIT 1 FOR UPDATE SKIP LOCKED
)
RETURNING id, queue, run_at;

UPDATE autumn_jobs
SET status = 'running', started_at = NOW(), claimed_by = :'worker', claimed_at = NOW(),
    pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
    unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  SELECT candidate.id FROM autumn_jobs candidate
  WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
    AND candidate.queue = ANY(:queues::text[])
    AND (candidate.concurrency_limit IS NULL OR (
      SELECT COUNT(*) FROM autumn_jobs running
      WHERE running.status = 'running' AND running.name = candidate.name
        AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
    ) < candidate.concurrency_limit)
    AND NOT (candidate.name = ANY(:deferred::text[]))
  ORDER BY array_position(:queues::text[], candidate.queue), candidate.run_at ASC
  LIMIT 1 FOR UPDATE SKIP LOCKED
)
RETURNING id, queue, run_at;

UPDATE autumn_jobs
SET status = 'running', started_at = NOW(), claimed_by = :'worker', claimed_at = NOW(),
    pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
    unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  SELECT candidate.id FROM autumn_jobs candidate
  WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
    AND candidate.queue = ANY(:queues::text[])
    AND (candidate.concurrency_limit IS NULL OR (
      SELECT COUNT(*) FROM autumn_jobs running
      WHERE running.status = 'running' AND running.name = candidate.name
        AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
    ) < candidate.concurrency_limit)
    AND NOT (candidate.name = ANY(:deferred::text[]))
  ORDER BY array_position(:queues::text[], candidate.queue), candidate.run_at ASC
  LIMIT 1 FOR UPDATE SKIP LOCKED
)
RETURNING id, queue, run_at;

UPDATE autumn_jobs
SET status = 'running', started_at = NOW(), claimed_by = :'worker', claimed_at = NOW(),
    pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
    unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  SELECT candidate.id FROM autumn_jobs candidate
  WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
    AND candidate.queue = ANY(:queues::text[])
    AND (candidate.concurrency_limit IS NULL OR (
      SELECT COUNT(*) FROM autumn_jobs running
      WHERE running.status = 'running' AND running.name = candidate.name
        AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
    ) < candidate.concurrency_limit)
    AND NOT (candidate.name = ANY(:deferred::text[]))
  ORDER BY array_position(:queues::text[], candidate.queue), candidate.run_at ASC
  LIMIT 1 FOR UPDATE SKIP LOCKED
)
RETURNING id, queue, run_at;

UPDATE autumn_jobs
SET status = 'running', started_at = NOW(), claimed_by = :'worker', claimed_at = NOW(),
    pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
    unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  SELECT candidate.id FROM autumn_jobs candidate
  WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
    AND candidate.queue = ANY(:queues::text[])
    AND (candidate.concurrency_limit IS NULL OR (
      SELECT COUNT(*) FROM autumn_jobs running
      WHERE running.status = 'running' AND running.name = candidate.name
        AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
    ) < candidate.concurrency_limit)
    AND NOT (candidate.name = ANY(:deferred::text[]))
  ORDER BY array_position(:queues::text[], candidate.queue), candidate.run_at ASC
  LIMIT 1 FOR UPDATE SKIP LOCKED
)
RETURNING id, queue, run_at;

UPDATE autumn_jobs
SET status = 'running', started_at = NOW(), claimed_by = :'worker', claimed_at = NOW(),
    pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
    unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  SELECT candidate.id FROM autumn_jobs candidate
  WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
    AND candidate.queue = ANY(:queues::text[])
    AND (candidate.concurrency_limit IS NULL OR (
      SELECT COUNT(*) FROM autumn_jobs running
      WHERE running.status = 'running' AND running.name = candidate.name
        AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
    ) < candidate.concurrency_limit)
    AND NOT (candidate.name = ANY(:deferred::text[]))
  ORDER BY array_position(:queues::text[], candidate.queue), candidate.run_at ASC
  LIMIT 1 FOR UPDATE SKIP LOCKED
)
RETURNING id, queue, run_at;

UPDATE autumn_jobs
SET status = 'running', started_at = NOW(), claimed_by = :'worker', claimed_at = NOW(),
    pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
    unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  SELECT candidate.id FROM autumn_jobs candidate
  WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
    AND candidate.queue = ANY(:queues::text[])
    AND (candidate.concurrency_limit IS NULL OR (
      SELECT COUNT(*) FROM autumn_jobs running
      WHERE running.status = 'running' AND running.name = candidate.name
        AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
    ) < candidate.concurrency_limit)
    AND NOT (candidate.name = ANY(:deferred::text[]))
  ORDER BY array_position(:queues::text[], candidate.queue), candidate.run_at ASC
  LIMIT 1 FOR UPDATE SKIP LOCKED
)
RETURNING id, queue, run_at;

UPDATE autumn_jobs
SET status = 'running', started_at = NOW(), claimed_by = :'worker', claimed_at = NOW(),
    pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
    unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  SELECT candidate.id FROM autumn_jobs candidate
  WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
    AND candidate.queue = ANY(:queues::text[])
    AND (candidate.concurrency_limit IS NULL OR (
      SELECT COUNT(*) FROM autumn_jobs running
      WHERE running.status = 'running' AND running.name = candidate.name
        AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
    ) < candidate.concurrency_limit)
    AND NOT (candidate.name = ANY(:deferred::text[]))
  ORDER BY array_position(:queues::text[], candidate.queue), candidate.run_at ASC
  LIMIT 1 FOR UPDATE SKIP LOCKED
)
RETURNING id, queue, run_at;

UPDATE autumn_jobs
SET status = 'running', started_at = NOW(), claimed_by = :'worker', claimed_at = NOW(),
    pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
    unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  SELECT candidate.id FROM autumn_jobs candidate
  WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
    AND candidate.queue = ANY(:queues::text[])
    AND (candidate.concurrency_limit IS NULL OR (
      SELECT COUNT(*) FROM autumn_jobs running
      WHERE running.status = 'running' AND running.name = candidate.name
        AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
    ) < candidate.concurrency_limit)
    AND NOT (candidate.name = ANY(:deferred::text[]))
  ORDER BY array_position(:queues::text[], candidate.queue), candidate.run_at ASC
  LIMIT 1 FOR UPDATE SKIP LOCKED
)
RETURNING id, queue, run_at;

UPDATE autumn_jobs
SET status = 'running', started_at = NOW(), claimed_by = :'worker', claimed_at = NOW(),
    pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
    unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  SELECT candidate.id FROM autumn_jobs candidate
  WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
    AND candidate.queue = ANY(:queues::text[])
    AND (candidate.concurrency_limit IS NULL OR (
      SELECT COUNT(*) FROM autumn_jobs running
      WHERE running.status = 'running' AND running.name = candidate.name
        AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
    ) < candidate.concurrency_limit)
    AND NOT (candidate.name = ANY(:deferred::text[]))
  ORDER BY array_position(:queues::text[], candidate.queue), candidate.run_at ASC
  LIMIT 1 FOR UPDATE SKIP LOCKED
)
RETURNING id, queue, run_at;

UPDATE autumn_jobs
SET status = 'running', started_at = NOW(), claimed_by = :'worker', claimed_at = NOW(),
    pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
    unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  SELECT candidate.id FROM autumn_jobs candidate
  WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
    AND candidate.queue = ANY(:queues::text[])
    AND (candidate.concurrency_limit IS NULL OR (
      SELECT COUNT(*) FROM autumn_jobs running
      WHERE running.status = 'running' AND running.name = candidate.name
        AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
    ) < candidate.concurrency_limit)
    AND NOT (candidate.name = ANY(:deferred::text[]))
  ORDER BY array_position(:queues::text[], candidate.queue), candidate.run_at ASC
  LIMIT 1 FOR UPDATE SKIP LOCKED
)
RETURNING id, queue, run_at;

UPDATE autumn_jobs
SET status = 'running', started_at = NOW(), claimed_by = :'worker', claimed_at = NOW(),
    pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
    unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  SELECT candidate.id FROM autumn_jobs candidate
  WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
    AND candidate.queue = ANY(:queues::text[])
    AND (candidate.concurrency_limit IS NULL OR (
      SELECT COUNT(*) FROM autumn_jobs running
      WHERE running.status = 'running' AND running.name = candidate.name
        AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
    ) < candidate.concurrency_limit)
    AND NOT (candidate.name = ANY(:deferred::text[]))
  ORDER BY array_position(:queues::text[], candidate.queue), candidate.run_at ASC
  LIMIT 1 FOR UPDATE SKIP LOCKED
)
RETURNING id, queue, run_at;

UPDATE autumn_jobs
SET status = 'running', started_at = NOW(), claimed_by = :'worker', claimed_at = NOW(),
    pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
    unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  SELECT candidate.id FROM autumn_jobs candidate
  WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
    AND candidate.queue = ANY(:queues::text[])
    AND (candidate.concurrency_limit IS NULL OR (
      SELECT COUNT(*) FROM autumn_jobs running
      WHERE running.status = 'running' AND running.name = candidate.name
        AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
    ) < candidate.concurrency_limit)
    AND NOT (candidate.name = ANY(:deferred::text[]))
  ORDER BY array_position(:queues::text[], candidate.queue), candidate.run_at ASC
  LIMIT 1 FOR UPDATE SKIP LOCKED
)
RETURNING id, queue, run_at;

UPDATE autumn_jobs
SET status = 'running', started_at = NOW(), claimed_by = :'worker', claimed_at = NOW(),
    pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
    unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  SELECT candidate.id FROM autumn_jobs candidate
  WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
    AND candidate.queue = ANY(:queues::text[])
    AND (candidate.concurrency_limit IS NULL OR (
      SELECT COUNT(*) FROM autumn_jobs running
      WHERE running.status = 'running' AND running.name = candidate.name
        AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
    ) < candidate.concurrency_limit)
    AND NOT (candidate.name = ANY(:deferred::text[]))
  ORDER BY array_position(:queues::text[], candidate.queue), candidate.run_at ASC
  LIMIT 1 FOR UPDATE SKIP LOCKED
)
RETURNING id, queue, run_at;

UPDATE autumn_jobs
SET status = 'running', started_at = NOW(), claimed_by = :'worker', claimed_at = NOW(),
    pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
    unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  SELECT candidate.id FROM autumn_jobs candidate
  WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
    AND candidate.queue = ANY(:queues::text[])
    AND (candidate.concurrency_limit IS NULL OR (
      SELECT COUNT(*) FROM autumn_jobs running
      WHERE running.status = 'running' AND running.name = candidate.name
        AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
    ) < candidate.concurrency_limit)
    AND NOT (candidate.name = ANY(:deferred::text[]))
  ORDER BY array_position(:queues::text[], candidate.queue), candidate.run_at ASC
  LIMIT 1 FOR UPDATE SKIP LOCKED
)
RETURNING id, queue, run_at;

UPDATE autumn_jobs
SET status = 'running', started_at = NOW(), claimed_by = :'worker', claimed_at = NOW(),
    pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
    unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  SELECT candidate.id FROM autumn_jobs candidate
  WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
    AND candidate.queue = ANY(:queues::text[])
    AND (candidate.concurrency_limit IS NULL OR (
      SELECT COUNT(*) FROM autumn_jobs running
      WHERE running.status = 'running' AND running.name = candidate.name
        AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
    ) < candidate.concurrency_limit)
    AND NOT (candidate.name = ANY(:deferred::text[]))
  ORDER BY array_position(:queues::text[], candidate.queue), candidate.run_at ASC
  LIMIT 1 FOR UPDATE SKIP LOCKED
)
RETURNING id, queue, run_at;

UPDATE autumn_jobs
SET status = 'running', started_at = NOW(), claimed_by = :'worker', claimed_at = NOW(),
    pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
    unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  SELECT candidate.id FROM autumn_jobs candidate
  WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
    AND candidate.queue = ANY(:queues::text[])
    AND (candidate.concurrency_limit IS NULL OR (
      SELECT COUNT(*) FROM autumn_jobs running
      WHERE running.status = 'running' AND running.name = candidate.name
        AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
    ) < candidate.concurrency_limit)
    AND NOT (candidate.name = ANY(:deferred::text[]))
  ORDER BY array_position(:queues::text[], candidate.queue), candidate.run_at ASC
  LIMIT 1 FOR UPDATE SKIP LOCKED
)
RETURNING id, queue, run_at;

UPDATE autumn_jobs
SET status = 'running', started_at = NOW(), claimed_by = :'worker', claimed_at = NOW(),
    pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
    unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  SELECT candidate.id FROM autumn_jobs candidate
  WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
    AND candidate.queue = ANY(:queues::text[])
    AND (candidate.concurrency_limit IS NULL OR (
      SELECT COUNT(*) FROM autumn_jobs running
      WHERE running.status = 'running' AND running.name = candidate.name
        AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
    ) < candidate.concurrency_limit)
    AND NOT (candidate.name = ANY(:deferred::text[]))
  ORDER BY array_position(:queues::text[], candidate.queue), candidate.run_at ASC
  LIMIT 1 FOR UPDATE SKIP LOCKED
)
RETURNING id, queue, run_at;

UPDATE autumn_jobs
SET status = 'running', started_at = NOW(), claimed_by = :'worker', claimed_at = NOW(),
    pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
    unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  SELECT candidate.id FROM autumn_jobs candidate
  WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
    AND candidate.queue = ANY(:queues::text[])
    AND (candidate.concurrency_limit IS NULL OR (
      SELECT COUNT(*) FROM autumn_jobs running
      WHERE running.status = 'running' AND running.name = candidate.name
        AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
    ) < candidate.concurrency_limit)
    AND NOT (candidate.name = ANY(:deferred::text[]))
  ORDER BY array_position(:queues::text[], candidate.queue), candidate.run_at ASC
  LIMIT 1 FOR UPDATE SKIP LOCKED
)
RETURNING id, queue, run_at;

UPDATE autumn_jobs
SET status = 'running', started_at = NOW(), claimed_by = :'worker', claimed_at = NOW(),
    pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
    unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  SELECT candidate.id FROM autumn_jobs candidate
  WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
    AND candidate.queue = ANY(:queues::text[])
    AND (candidate.concurrency_limit IS NULL OR (
      SELECT COUNT(*) FROM autumn_jobs running
      WHERE running.status = 'running' AND running.name = candidate.name
        AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
    ) < candidate.concurrency_limit)
    AND NOT (candidate.name = ANY(:deferred::text[]))
  ORDER BY array_position(:queues::text[], candidate.queue), candidate.run_at ASC
  LIMIT 1 FOR UPDATE SKIP LOCKED
)
RETURNING id, queue, run_at;

UPDATE autumn_jobs
SET status = 'running', started_at = NOW(), claimed_by = :'worker', claimed_at = NOW(),
    pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
    unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  SELECT candidate.id FROM autumn_jobs candidate
  WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
    AND candidate.queue = ANY(:queues::text[])
    AND (candidate.concurrency_limit IS NULL OR (
      SELECT COUNT(*) FROM autumn_jobs running
      WHERE running.status = 'running' AND running.name = candidate.name
        AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
    ) < candidate.concurrency_limit)
    AND NOT (candidate.name = ANY(:deferred::text[]))
  ORDER BY array_position(:queues::text[], candidate.queue), candidate.run_at ASC
  LIMIT 1 FOR UPDATE SKIP LOCKED
)
RETURNING id, queue, run_at;

UPDATE autumn_jobs
SET status = 'running', started_at = NOW(), claimed_by = :'worker', claimed_at = NOW(),
    pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
    unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  SELECT candidate.id FROM autumn_jobs candidate
  WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
    AND candidate.queue = ANY(:queues::text[])
    AND (candidate.concurrency_limit IS NULL OR (
      SELECT COUNT(*) FROM autumn_jobs running
      WHERE running.status = 'running' AND running.name = candidate.name
        AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
    ) < candidate.concurrency_limit)
    AND NOT (candidate.name = ANY(:deferred::text[]))
  ORDER BY array_position(:queues::text[], candidate.queue), candidate.run_at ASC
  LIMIT 1 FOR UPDATE SKIP LOCKED
)
RETURNING id, queue, run_at;

UPDATE autumn_jobs
SET status = 'running', started_at = NOW(), claimed_by = :'worker', claimed_at = NOW(),
    pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
    unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  SELECT candidate.id FROM autumn_jobs candidate
  WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
    AND candidate.queue = ANY(:queues::text[])
    AND (candidate.concurrency_limit IS NULL OR (
      SELECT COUNT(*) FROM autumn_jobs running
      WHERE running.status = 'running' AND running.name = candidate.name
        AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
    ) < candidate.concurrency_limit)
    AND NOT (candidate.name = ANY(:deferred::text[]))
  ORDER BY array_position(:queues::text[], candidate.queue), candidate.run_at ASC
  LIMIT 1 FOR UPDATE SKIP LOCKED
)
RETURNING id, queue, run_at;

UPDATE autumn_jobs
SET status = 'running', started_at = NOW(), claimed_by = :'worker', claimed_at = NOW(),
    pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
    unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  SELECT candidate.id FROM autumn_jobs candidate
  WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
    AND candidate.queue = ANY(:queues::text[])
    AND (candidate.concurrency_limit IS NULL OR (
      SELECT COUNT(*) FROM autumn_jobs running
      WHERE running.status = 'running' AND running.name = candidate.name
        AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
    ) < candidate.concurrency_limit)
    AND NOT (candidate.name = ANY(:deferred::text[]))
  ORDER BY array_position(:queues::text[], candidate.queue), candidate.run_at ASC
  LIMIT 1 FOR UPDATE SKIP LOCKED
)
RETURNING id, queue, run_at;

UPDATE autumn_jobs
SET status = 'running', started_at = NOW(), claimed_by = :'worker', claimed_at = NOW(),
    pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
    unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  SELECT candidate.id FROM autumn_jobs candidate
  WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
    AND candidate.queue = ANY(:queues::text[])
    AND (candidate.concurrency_limit IS NULL OR (
      SELECT COUNT(*) FROM autumn_jobs running
      WHERE running.status = 'running' AND running.name = candidate.name
        AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
    ) < candidate.concurrency_limit)
    AND NOT (candidate.name = ANY(:deferred::text[]))
  ORDER BY array_position(:queues::text[], candidate.queue), candidate.run_at ASC
  LIMIT 1 FOR UPDATE SKIP LOCKED
)
RETURNING id, queue, run_at;

UPDATE autumn_jobs
SET status = 'running', started_at = NOW(), claimed_by = :'worker', claimed_at = NOW(),
    pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
    unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  SELECT candidate.id FROM autumn_jobs candidate
  WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
    AND candidate.queue = ANY(:queues::text[])
    AND (candidate.concurrency_limit IS NULL OR (
      SELECT COUNT(*) FROM autumn_jobs running
      WHERE running.status = 'running' AND running.name = candidate.name
        AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
    ) < candidate.concurrency_limit)
    AND NOT (candidate.name = ANY(:deferred::text[]))
  ORDER BY array_position(:queues::text[], candidate.queue), candidate.run_at ASC
  LIMIT 1 FOR UPDATE SKIP LOCKED
)
RETURNING id, queue, run_at;

UPDATE autumn_jobs
SET status = 'running', started_at = NOW(), claimed_by = :'worker', claimed_at = NOW(),
    pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
    unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  SELECT candidate.id FROM autumn_jobs candidate
  WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
    AND candidate.queue = ANY(:queues::text[])
    AND (candidate.concurrency_limit IS NULL OR (
      SELECT COUNT(*) FROM autumn_jobs running
      WHERE running.status = 'running' AND running.name = candidate.name
        AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
    ) < candidate.concurrency_limit)
    AND NOT (candidate.name = ANY(:deferred::text[]))
  ORDER BY array_position(:queues::text[], candidate.queue), candidate.run_at ASC
  LIMIT 1 FOR UPDATE SKIP LOCKED
)
RETURNING id, queue, run_at;

UPDATE autumn_jobs
SET status = 'running', started_at = NOW(), claimed_by = :'worker', claimed_at = NOW(),
    pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
    unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  SELECT candidate.id FROM autumn_jobs candidate
  WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
    AND candidate.queue = ANY(:queues::text[])
    AND (candidate.concurrency_limit IS NULL OR (
      SELECT COUNT(*) FROM autumn_jobs running
      WHERE running.status = 'running' AND running.name = candidate.name
        AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
    ) < candidate.concurrency_limit)
    AND NOT (candidate.name = ANY(:deferred::text[]))
  ORDER BY array_position(:queues::text[], candidate.queue), candidate.run_at ASC
  LIMIT 1 FOR UPDATE SKIP LOCKED
)
RETURNING id, queue, run_at;

UPDATE autumn_jobs
SET status = 'running', started_at = NOW(), claimed_by = :'worker', claimed_at = NOW(),
    pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
    unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  SELECT candidate.id FROM autumn_jobs candidate
  WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
    AND candidate.queue = ANY(:queues::text[])
    AND (candidate.concurrency_limit IS NULL OR (
      SELECT COUNT(*) FROM autumn_jobs running
      WHERE running.status = 'running' AND running.name = candidate.name
        AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
    ) < candidate.concurrency_limit)
    AND NOT (candidate.name = ANY(:deferred::text[]))
  ORDER BY array_position(:queues::text[], candidate.queue), candidate.run_at ASC
  LIMIT 1 FOR UPDATE SKIP LOCKED
)
RETURNING id, queue, run_at;

UPDATE autumn_jobs
SET status = 'running', started_at = NOW(), claimed_by = :'worker', claimed_at = NOW(),
    pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
    unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  SELECT candidate.id FROM autumn_jobs candidate
  WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
    AND candidate.queue = ANY(:queues::text[])
    AND (candidate.concurrency_limit IS NULL OR (
      SELECT COUNT(*) FROM autumn_jobs running
      WHERE running.status = 'running' AND running.name = candidate.name
        AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
    ) < candidate.concurrency_limit)
    AND NOT (candidate.name = ANY(:deferred::text[]))
  ORDER BY array_position(:queues::text[], candidate.queue), candidate.run_at ASC
  LIMIT 1 FOR UPDATE SKIP LOCKED
)
RETURNING id, queue, run_at;

UPDATE autumn_jobs
SET status = 'running', started_at = NOW(), claimed_by = :'worker', claimed_at = NOW(),
    pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
    unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  SELECT candidate.id FROM autumn_jobs candidate
  WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
    AND candidate.queue = ANY(:queues::text[])
    AND (candidate.concurrency_limit IS NULL OR (
      SELECT COUNT(*) FROM autumn_jobs running
      WHERE running.status = 'running' AND running.name = candidate.name
        AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
    ) < candidate.concurrency_limit)
    AND NOT (candidate.name = ANY(:deferred::text[]))
  ORDER BY array_position(:queues::text[], candidate.queue), candidate.run_at ASC
  LIMIT 1 FOR UPDATE SKIP LOCKED
)
RETURNING id, queue, run_at;

UPDATE autumn_jobs
SET status = 'running', started_at = NOW(), claimed_by = :'worker', claimed_at = NOW(),
    pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
    unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  SELECT candidate.id FROM autumn_jobs candidate
  WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
    AND candidate.queue = ANY(:queues::text[])
    AND (candidate.concurrency_limit IS NULL OR (
      SELECT COUNT(*) FROM autumn_jobs running
      WHERE running.status = 'running' AND running.name = candidate.name
        AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
    ) < candidate.concurrency_limit)
    AND NOT (candidate.name = ANY(:deferred::text[]))
  ORDER BY array_position(:queues::text[], candidate.queue), candidate.run_at ASC
  LIMIT 1 FOR UPDATE SKIP LOCKED
)
RETURNING id, queue, run_at;

UPDATE autumn_jobs
SET status = 'running', started_at = NOW(), claimed_by = :'worker', claimed_at = NOW(),
    pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
    unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  SELECT candidate.id FROM autumn_jobs candidate
  WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
    AND candidate.queue = ANY(:queues::text[])
    AND (candidate.concurrency_limit IS NULL OR (
      SELECT COUNT(*) FROM autumn_jobs running
      WHERE running.status = 'running' AND running.name = candidate.name
        AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
    ) < candidate.concurrency_limit)
    AND NOT (candidate.name = ANY(:deferred::text[]))
  ORDER BY array_position(:queues::text[], candidate.queue), candidate.run_at ASC
  LIMIT 1 FOR UPDATE SKIP LOCKED
)
RETURNING id, queue, run_at;

UPDATE autumn_jobs
SET status = 'running', started_at = NOW(), claimed_by = :'worker', claimed_at = NOW(),
    pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
    unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  SELECT candidate.id FROM autumn_jobs candidate
  WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
    AND candidate.queue = ANY(:queues::text[])
    AND (candidate.concurrency_limit IS NULL OR (
      SELECT COUNT(*) FROM autumn_jobs running
      WHERE running.status = 'running' AND running.name = candidate.name
        AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
    ) < candidate.concurrency_limit)
    AND NOT (candidate.name = ANY(:deferred::text[]))
  ORDER BY array_position(:queues::text[], candidate.queue), candidate.run_at ASC
  LIMIT 1 FOR UPDATE SKIP LOCKED
)
RETURNING id, queue, run_at;

UPDATE autumn_jobs
SET status = 'running', started_at = NOW(), claimed_by = :'worker', claimed_at = NOW(),
    pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
    unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  SELECT candidate.id FROM autumn_jobs candidate
  WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
    AND candidate.queue = ANY(:queues::text[])
    AND (candidate.concurrency_limit IS NULL OR (
      SELECT COUNT(*) FROM autumn_jobs running
      WHERE running.status = 'running' AND running.name = candidate.name
        AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
    ) < candidate.concurrency_limit)
    AND NOT (candidate.name = ANY(:deferred::text[]))
  ORDER BY array_position(:queues::text[], candidate.queue), candidate.run_at ASC
  LIMIT 1 FOR UPDATE SKIP LOCKED
)
RETURNING id, queue, run_at;

UPDATE autumn_jobs
SET status = 'running', started_at = NOW(), claimed_by = :'worker', claimed_at = NOW(),
    pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
    unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  SELECT candidate.id FROM autumn_jobs candidate
  WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
    AND candidate.queue = ANY(:queues::text[])
    AND (candidate.concurrency_limit IS NULL OR (
      SELECT COUNT(*) FROM autumn_jobs running
      WHERE running.status = 'running' AND running.name = candidate.name
        AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
    ) < candidate.concurrency_limit)
    AND NOT (candidate.name = ANY(:deferred::text[]))
  ORDER BY array_position(:queues::text[], candidate.queue), candidate.run_at ASC
  LIMIT 1 FOR UPDATE SKIP LOCKED
)
RETURNING id, queue, run_at;

UPDATE autumn_jobs
SET status = 'running', started_at = NOW(), claimed_by = :'worker', claimed_at = NOW(),
    pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
    unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  SELECT candidate.id FROM autumn_jobs candidate
  WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
    AND candidate.queue = ANY(:queues::text[])
    AND (candidate.concurrency_limit IS NULL OR (
      SELECT COUNT(*) FROM autumn_jobs running
      WHERE running.status = 'running' AND running.name = candidate.name
        AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
    ) < candidate.concurrency_limit)
    AND NOT (candidate.name = ANY(:deferred::text[]))
  ORDER BY array_position(:queues::text[], candidate.queue), candidate.run_at ASC
  LIMIT 1 FOR UPDATE SKIP LOCKED
)
RETURNING id, queue, run_at;

UPDATE autumn_jobs
SET status = 'running', started_at = NOW(), claimed_by = :'worker', claimed_at = NOW(),
    pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
    unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  SELECT candidate.id FROM autumn_jobs candidate
  WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
    AND candidate.queue = ANY(:queues::text[])
    AND (candidate.concurrency_limit IS NULL OR (
      SELECT COUNT(*) FROM autumn_jobs running
      WHERE running.status = 'running' AND running.name = candidate.name
        AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
    ) < candidate.concurrency_limit)
    AND NOT (candidate.name = ANY(:deferred::text[]))
  ORDER BY array_position(:queues::text[], candidate.queue), candidate.run_at ASC
  LIMIT 1 FOR UPDATE SKIP LOCKED
)
RETURNING id, queue, run_at;

UPDATE autumn_jobs
SET status = 'running', started_at = NOW(), claimed_by = :'worker', claimed_at = NOW(),
    pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
    unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  SELECT candidate.id FROM autumn_jobs candidate
  WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
    AND candidate.queue = ANY(:queues::text[])
    AND (candidate.concurrency_limit IS NULL OR (
      SELECT COUNT(*) FROM autumn_jobs running
      WHERE running.status = 'running' AND running.name = candidate.name
        AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
    ) < candidate.concurrency_limit)
    AND NOT (candidate.name = ANY(:deferred::text[]))
  ORDER BY array_position(:queues::text[], candidate.queue), candidate.run_at ASC
  LIMIT 1 FOR UPDATE SKIP LOCKED
)
RETURNING id, queue, run_at;

UPDATE autumn_jobs
SET status = 'running', started_at = NOW(), claimed_by = :'worker', claimed_at = NOW(),
    pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
    unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  SELECT candidate.id FROM autumn_jobs candidate
  WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
    AND candidate.queue = ANY(:queues::text[])
    AND (candidate.concurrency_limit IS NULL OR (
      SELECT COUNT(*) FROM autumn_jobs running
      WHERE running.status = 'running' AND running.name = candidate.name
        AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
    ) < candidate.concurrency_limit)
    AND NOT (candidate.name = ANY(:deferred::text[]))
  ORDER BY array_position(:queues::text[], candidate.queue), candidate.run_at ASC
  LIMIT 1 FOR UPDATE SKIP LOCKED
)
RETURNING id, queue, run_at;

UPDATE autumn_jobs
SET status = 'running', started_at = NOW(), claimed_by = :'worker', claimed_at = NOW(),
    pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
    unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  SELECT candidate.id FROM autumn_jobs candidate
  WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
    AND candidate.queue = ANY(:queues::text[])
    AND (candidate.concurrency_limit IS NULL OR (
      SELECT COUNT(*) FROM autumn_jobs running
      WHERE running.status = 'running' AND running.name = candidate.name
        AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
    ) < candidate.concurrency_limit)
    AND NOT (candidate.name = ANY(:deferred::text[]))
  ORDER BY array_position(:queues::text[], candidate.queue), candidate.run_at ASC
  LIMIT 1 FOR UPDATE SKIP LOCKED
)
RETURNING id, queue, run_at;

UPDATE autumn_jobs
SET status = 'running', started_at = NOW(), claimed_by = :'worker', claimed_at = NOW(),
    pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
    unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  SELECT candidate.id FROM autumn_jobs candidate
  WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
    AND candidate.queue = ANY(:queues::text[])
    AND (candidate.concurrency_limit IS NULL OR (
      SELECT COUNT(*) FROM autumn_jobs running
      WHERE running.status = 'running' AND running.name = candidate.name
        AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
    ) < candidate.concurrency_limit)
    AND NOT (candidate.name = ANY(:deferred::text[]))
  ORDER BY array_position(:queues::text[], candidate.queue), candidate.run_at ASC
  LIMIT 1 FOR UPDATE SKIP LOCKED
)
RETURNING id, queue, run_at;

UPDATE autumn_jobs
SET status = 'running', started_at = NOW(), claimed_by = :'worker', claimed_at = NOW(),
    pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
    unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  SELECT candidate.id FROM autumn_jobs candidate
  WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
    AND candidate.queue = ANY(:queues::text[])
    AND (candidate.concurrency_limit IS NULL OR (
      SELECT COUNT(*) FROM autumn_jobs running
      WHERE running.status = 'running' AND running.name = candidate.name
        AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
    ) < candidate.concurrency_limit)
    AND NOT (candidate.name = ANY(:deferred::text[]))
  ORDER BY array_position(:queues::text[], candidate.queue), candidate.run_at ASC
  LIMIT 1 FOR UPDATE SKIP LOCKED
)
RETURNING id, queue, run_at;

UPDATE autumn_jobs
SET status = 'running', started_at = NOW(), claimed_by = :'worker', claimed_at = NOW(),
    pending_unique_key = CASE WHEN unique_window = 'pending' THEN unique_key ELSE NULL END,
    unique_key = CASE WHEN unique_window = 'pending' THEN NULL ELSE unique_key END
WHERE id = (
  SELECT candidate.id FROM autumn_jobs candidate
  WHERE candidate.status = 'enqueued' AND candidate.run_at <= NOW()
    AND candidate.queue = ANY(:queues::text[])
    AND (candidate.concurrency_limit IS NULL OR (
      SELECT COUNT(*) FROM autumn_jobs running
      WHERE running.status = 'running' AND running.name = candidate.name
        AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key
    ) < candidate.concurrency_limit)
    AND NOT (candidate.name = ANY(:deferred::text[]))
  ORDER BY array_position(:queues::text[], candidate.queue), candidate.run_at ASC
  LIMIT 1 FOR UPDATE SKIP LOCKED
)
RETURNING id, queue, run_at;

\o
