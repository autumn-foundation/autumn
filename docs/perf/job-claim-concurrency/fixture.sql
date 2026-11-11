-- Production-shaped autumn_jobs. Seeded (setseed) and deterministic.
-- :hist  finished rows (97% completed, 3% failed) over the last 7 days
-- :sat   ready backlog of one concurrency-limited name whose group is saturated
--        (limit 2, both slots running). Its rows are the oldest ready rows.
-- Plus: 10 unlimited names x 2,000 ready rows (newer), 20,000 future rows,
-- 30 running unlimited jobs, 2 running saturated jobs.
SELECT setseed(0.42);
INSERT INTO autumn_jobs (id, name, payload, status, attempt, enqueued_at, run_at,
                         started_at, finished_at, queue, last_error)
SELECT 'h' || g,
       (ARRAY['mail.deliver','webhook.send','report.build','thumb.render','sync.pull',
              'index.update','billing.charge','digest.send','cleanup.tmp','audit.export'])
         [1 + floor(power(random(), 2) * 10)::int],
       jsonb_build_object('n', g, 'pad', repeat('x', 60 + (random() * 120)::int)),
       CASE WHEN random() < 0.97 THEN 'completed' ELSE 'failed' END,
       1 + (random() < 0.05)::int,
       now() - interval '7 days' * (g::float8 / :hist),
       now() - interval '7 days' * (g::float8 / :hist),
       now() - interval '7 days' * (g::float8 / :hist),
       now() - interval '7 days' * (g::float8 / :hist) + interval '200 ms',
       'default', NULL
FROM generate_series(1, :hist) g;

-- Saturated concurrency group: oldest ready rows, all blocked.
INSERT INTO autumn_jobs (id, name, payload, status, run_at, enqueued_at, queue,
                         concurrency_key, concurrency_limit)
SELECT 's' || g, 'mail.deliver', jsonb_build_object('to', 'u' || g || '@example.test'),
       'enqueued', now() - interval '2 hours' + interval '1 ms' * g,
       now() - interval '2 hours', 'default', 'smtp', 2
FROM generate_series(1, :sat) g;
INSERT INTO autumn_jobs (id, name, status, run_at, started_at, claimed_by, claimed_at,
                         queue, concurrency_key, concurrency_limit)
SELECT 'sr' || g, 'mail.deliver', 'running', now() - interval '3 hours', now(),
       'worker-x', now(), 'default', 'smtp', 2
FROM generate_series(1, 2) g;

-- Unlimited ready work, newer than the saturated backlog.
INSERT INTO autumn_jobs (id, name, payload, status, run_at, enqueued_at, queue)
SELECT 'r' || g,
       (ARRAY['webhook.send','report.build','thumb.render','sync.pull','index.update',
              'billing.charge','digest.send','cleanup.tmp','audit.export','sitemap.build'])
         [1 + (g % 10)],
       jsonb_build_object('n', g), 'enqueued',
       now() - interval '30 minutes' + interval '50 ms' * g, now() - interval '30 minutes',
       'default'
FROM generate_series(1, 20000) g;

-- Scheduled for later.
INSERT INTO autumn_jobs (id, name, status, run_at, queue)
SELECT 'f' || g, 'digest.send', 'enqueued', now() + interval '1 second' * g, 'default'
FROM generate_series(1, 20000) g;

INSERT INTO autumn_jobs (id, name, status, run_at, started_at, claimed_by, claimed_at, queue)
SELECT 'run' || g, 'report.build', 'running', now() - interval '1 hour', now(),
       'worker-y', now(), 'default'
FROM generate_series(1, 30) g;

VACUUM ANALYZE autumn_jobs;
