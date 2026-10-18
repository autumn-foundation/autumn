-- Seeded, deterministic production-shaped autumn_jobs fixture.
-- Usage: psql -v ready=50000 -f fixture.sql   (ready = rows 'enqueued' and due)
-- Shape: 10 job names (Zipf-ish skew), 4 queues, history of succeeded/dead rows
-- = 9x the ready backlog, ~30% of due rows carry a concurrency limit (keyed by
-- tenant, 5% NULL key), ~0.5% running, 2% future-scheduled, real dead tuples
-- from a post-seed UPDATE, ANALYZE'd with no VACUUM.
\set ON_ERROR_STOP on
SELECT setseed(0.42);
DROP TABLE IF EXISTS autumn_jobs;
\i migrations_jobs.sql
INSERT INTO autumn_jobs (id, name, queue, status, run_at, enqueued_at, finished_at,
                         claimed_by, claimed_at, concurrency_key, concurrency_limit)
SELECT 'job-' || g,
       'job_' || LEAST(9, floor(-ln(1 - random()) * 2.5)::int),
       (ARRAY['default','default','default','mail','billing','ingest'])[1 + (g % 6)],
       CASE WHEN g <= :ready THEN 'enqueued'
            WHEN g <= :ready * 1.02 THEN 'enqueued'          -- future-scheduled
            WHEN g <= :ready * 1.025 THEN 'running'
            ELSE (ARRAY['succeeded','succeeded','succeeded','dead'])[1 + (g % 4)] END,
       CASE WHEN g <= :ready THEN NOW() - (random() * interval '2 hours')
            WHEN g <= :ready * 1.02 THEN NOW() + (random() * interval '1 day')
            ELSE NOW() - (random() * interval '30 days') END,
       NOW() - interval '31 days' + (g * interval '1 second' / 10),
       CASE WHEN g > :ready * 1.025 THEN NOW() - (random() * interval '30 days') END,
       CASE WHEN g BETWEEN :ready * 1.02 + 1 AND :ready * 1.025 THEN 'worker-' || (g % 8) END,
       CASE WHEN g BETWEEN :ready * 1.02 + 1 AND :ready * 1.025 THEN NOW() - (random() * interval '20 seconds') END,
       CASE WHEN random() < 0.05 THEN NULL ELSE 'tenant-' || floor(random() * 200)::int END,
       CASE WHEN random() < 0.30 THEN 5 END
FROM generate_series(1, (:ready * 10)::bigint) g;
-- running rows share name/key space with the blocked candidates
UPDATE autumn_jobs SET finished_at = finished_at + interval '1 second' WHERE id LIKE 'job-%7';
ANALYZE autumn_jobs;
