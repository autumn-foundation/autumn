-- Production-shaped autumn_jobs for the unique-job enqueue path. Seeded
-- (setseed) and deterministic.
-- :hist finished rows over the last 7 days (97% completed, 3% failed):
--   25% digest.send   #[job(unique, window = running)], key user:<zipf 20,000>,
--                     key kept on the finished row (the running window keeps it)
--   15% sync.pull     #[job(unique, window = pending)], key cleared at claim
--                     (unique_key NULL, pending_unique_key kept)
--   10% report.build  #[job(unique, ttl = 1h)], key acct:<zipf 2,000>, kept
--   50% eight names with no uniqueness
-- In flight: 3,000 digest.send (keys user:1..3000), 500 sync.pull
-- (acct:1..500), 300 report.build enqueued in the last hour (acct:1..300),
-- 40 running jobs, 20,000 plain ready rows.
SELECT setseed(0.42);
INSERT INTO autumn_jobs (id, name, payload, status, attempt, enqueued_at, run_at,
                         started_at, finished_at, queue, unique_key, unique_window,
                         pending_unique_key)
SELECT 'h' || g, j.name,
       jsonb_build_object('n', g, 'pad', repeat('x', 60 + (r2 * 120)::int)),
       CASE WHEN r3 < 0.97 THEN 'completed' ELSE 'failed' END,
       1 + (r3 > 0.95)::int, t, t, t, t + interval '200 ms', 'default',
       j.unique_key, j.unique_window, j.pending_unique_key
FROM (
  SELECT g, random() AS r1, random() AS r2, random() AS r3, random() AS rk,
         now() - interval '7 days' * (1 - g::float8 / :hist) AS t
  FROM generate_series(1, :hist) g
) s
CROSS JOIN LATERAL (
  SELECT CASE WHEN r1 < 0.25 THEN 'digest.send'
              WHEN r1 < 0.40 THEN 'sync.pull'
              WHEN r1 < 0.50 THEN 'report.build'
              ELSE (ARRAY['mail.deliver','webhook.send','thumb.render','index.update',
                          'billing.charge','cleanup.tmp','audit.export','sitemap.build'])
                   [1 + floor(power(rk, 2) * 8)::int] END AS name,
         CASE WHEN r1 < 0.25 THEN 'user:' || (1 + floor(power(rk, 3) * 20000)::int)
              WHEN r1 >= 0.40 AND r1 < 0.50 THEN 'acct:' || (1 + floor(power(rk, 3) * 2000)::int)
         END AS unique_key,
         CASE WHEN r1 < 0.25 THEN 'running' WHEN r1 < 0.40 THEN 'pending'
              WHEN r1 < 0.50 THEN 'ttl' END AS unique_window,
         CASE WHEN r1 >= 0.25 AND r1 < 0.40 THEN 'acct:' || (1 + floor(power(rk, 3) * 2000)::int)
         END AS pending_unique_key
) j;

-- In-flight unique holds.
INSERT INTO autumn_jobs (id, name, status, run_at, enqueued_at, queue, unique_key, unique_window)
SELECT 'ud' || g, 'digest.send', 'enqueued', now() + interval '1 minute', now() - interval '5 minutes',
       'default', 'user:' || g, 'running'
FROM generate_series(1, 3000) g;
INSERT INTO autumn_jobs (id, name, status, run_at, enqueued_at, queue, unique_key, unique_window)
SELECT 'us' || g, 'sync.pull', 'enqueued', now() + interval '1 minute', now() - interval '5 minutes',
       'default', 'acct:' || g, 'pending'
FROM generate_series(1, 500) g;
INSERT INTO autumn_jobs (id, name, status, run_at, enqueued_at, queue, unique_key, unique_window)
SELECT 'ur' || g, 'report.build', 'enqueued', now() + interval '1 minute',
       now() - interval '50 minutes' + interval '10 s' * g, 'default', 'acct:' || g, 'ttl'
FROM generate_series(1, 300) g;
INSERT INTO autumn_jobs (id, name, status, run_at, started_at, claimed_by, claimed_at, queue)
SELECT 'run' || g, 'thumb.render', 'running', now() - interval '1 minute', now(),
       'worker-y', now(), 'default'
FROM generate_series(1, 40) g;
INSERT INTO autumn_jobs (id, name, payload, status, run_at, enqueued_at, queue)
SELECT 'r' || g, 'webhook.send', jsonb_build_object('n', g), 'enqueued',
       now() + interval '1 hour' + interval '50 ms' * g, now() - interval '1 minute', 'default'
FROM generate_series(1, 20000) g;

VACUUM ANALYZE autumn_jobs;
