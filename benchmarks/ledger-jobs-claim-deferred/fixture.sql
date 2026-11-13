-- Seeded, deterministic fixture. :hot = ready backlog of the deferred job name
-- (oldest run_at => sits at the head of the (queue, run_at) index).
-- 100k other ready jobs across 9 names, 1M finished (dead) rows, 200 running.
SELECT setseed(0.42);
TRUNCATE autumn_jobs;
ALTER TABLE autumn_jobs SET (autovacuum_enabled = false);
INSERT INTO autumn_jobs (id, name, status, run_at, enqueued_at, queue, payload)
SELECT 'hot-'||g, 'report.slow', 'enqueued', TIMESTAMPTZ '2026-01-01' + g * interval '1 second',
       TIMESTAMPTZ '2026-01-01' + g * interval '1 second', 'default', '{}'
FROM generate_series(1, :hot) g;
INSERT INTO autumn_jobs (id, name, status, run_at, enqueued_at, queue, payload)
SELECT 'oth-'||g, 'job.n'||(g % 9), 'enqueued', TIMESTAMPTZ '2026-06-01' + g * interval '1 second',
       TIMESTAMPTZ '2026-06-01' + g * interval '1 second', 'default', '{}'
FROM generate_series(1, 100000) g;
INSERT INTO autumn_jobs (id, name, status, run_at, enqueued_at, finished_at, queue, payload, attempt)
SELECT 'done-'||g, 'job.n'||(g % 9), CASE WHEN g % 50 = 0 THEN 'failed' ELSE 'succeeded' END,
       TIMESTAMPTZ '2025-01-01' + g * interval '20 seconds', TIMESTAMPTZ '2025-01-01' + g * interval '20 seconds',
       TIMESTAMPTZ '2025-01-01' + g * interval '20 seconds' + interval '3 seconds', 'default', '{"k":1}', 1
FROM generate_series(1, 1000000) g;
INSERT INTO autumn_jobs (id, name, status, run_at, claimed_at, claimed_by, queue, payload, concurrency_key, concurrency_limit)
SELECT 'run-'||g, 'report.slow', 'running', now(), now(), 'w1', 'default', '{}', NULL, NULL
FROM generate_series(1, 200) g;
VACUUM (ANALYZE) autumn_jobs;
