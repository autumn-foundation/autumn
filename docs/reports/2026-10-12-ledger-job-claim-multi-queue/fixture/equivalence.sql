-- Result-equivalence: the old ORDER BY array_position(..), run_at pick vs the
-- per-queue lateral pick, compared as (queue, run_at) (ties on run_at are an
-- existing "any due job" semantic) over queue orders, deferred names, an
-- unknown queue, a duplicated queue, an empty list, and concurrency-limit and
-- NULL-key edges. SELECT-only: nothing is claimed, so every case sees the same
-- table state.
CREATE TEMP TABLE cases(qs text[], deferred text[]);
INSERT INTO cases
SELECT q, d FROM (VALUES
  (ARRAY['high','default','low']), (ARRAY['low','default','high']),
  (ARRAY['default','high']), (ARRAY['low','high']), (ARRAY['default','default','low']),
  (ARRAY['nosuchqueue','high']), (ARRAY['nosuchqueue','nosuch2']), (ARRAY[]::text[])
) a(q) CROSS JOIN (VALUES
  (ARRAY[]::text[]),
  (ARRAY[(SELECT name FROM autumn_jobs WHERE status='enqueued' GROUP BY name ORDER BY count(*) DESC LIMIT 1)]),
  (ARRAY(SELECT name FROM autumn_jobs WHERE status='enqueued' GROUP BY name ORDER BY count(*) DESC LIMIT 12))
) b(d);
-- Edge: make a concurrency-limited name saturated (its limit already running).
SELECT count(*) AS limited_ready FROM autumn_jobs WHERE status='enqueued' AND concurrency_limit IS NOT NULL;
WITH old AS (
  SELECT c.qs, c.deferred, (SELECT (candidate.queue, candidate.run_at)::text FROM autumn_jobs candidate
    WHERE candidate.status='enqueued' AND candidate.run_at <= NOW() AND candidate.queue = ANY(c.qs)
      AND (candidate.concurrency_limit IS NULL OR (SELECT COUNT(*) FROM autumn_jobs running
           WHERE running.status='running' AND running.name=candidate.name
             AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key) < candidate.concurrency_limit)
      AND NOT (candidate.name = ANY(c.deferred))
    ORDER BY array_position(c.qs, candidate.queue), candidate.run_at ASC LIMIT 1) AS pick
  FROM cases c
), new AS (
  SELECT c.qs, c.deferred, (SELECT (picked.queue, picked.run_at)::text FROM unnest(c.qs) WITH ORDINALITY q(queue, ord)
    CROSS JOIN LATERAL (SELECT candidate.queue, candidate.run_at FROM autumn_jobs candidate
      WHERE candidate.status='enqueued' AND candidate.run_at <= NOW() AND candidate.queue = q.queue
        AND (candidate.concurrency_limit IS NULL OR (SELECT COUNT(*) FROM autumn_jobs running
             WHERE running.status='running' AND running.name=candidate.name
               AND running.concurrency_key IS NOT DISTINCT FROM candidate.concurrency_key) < candidate.concurrency_limit)
        AND NOT (candidate.name = ANY(c.deferred))
      ORDER BY candidate.run_at ASC LIMIT 1) picked
    ORDER BY q.ord, picked.run_at LIMIT 1) AS pick
  FROM cases c
)
SELECT count(*) AS cases, count(*) FILTER (WHERE old.pick IS NOT DISTINCT FROM new.pick) AS identical,
       count(*) FILTER (WHERE old.pick IS NULL) AS both_empty
FROM old JOIN new USING (qs, deferred);
