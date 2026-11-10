-- Trade-off check: the running counts are read for the whole running set, once
-- per claim that reaches a concurrency-limited row. Worst case for that: many
-- running jobs whose rows were just updated (not all-visible, so every index
-- entry costs a heap fetch) and an unsaturated limited job at the queue head.
-- :running unlimited running rows; head job 'mail.deliver' limit 10, 2 running.
TRUNCATE autumn_jobs;
-- running rows spread one per ~50 rows of finished history, so each sits on its own heap page
INSERT INTO autumn_jobs (id, name, status, run_at, queue) SELECT 'r' || g, CASE WHEN g % 50 = 0 THEN 'report.build' ELSE 'old.done' END, CASE WHEN g % 50 = 0 THEN 'enqueued' ELSE 'completed' END, now() - interval '1 hour', 'default' FROM generate_series(1, :running * 50) g;
INSERT INTO autumn_jobs (id, name, status, run_at, queue, concurrency_key, concurrency_limit)
SELECT 'm' || g, 'mail.deliver', CASE WHEN g <= 2 THEN 'running' ELSE 'enqueued' END,
       now() - interval '30 minutes' + interval '1 s' * g, 'default', 'smtp', 10 FROM generate_series(1, 1000) g;
INSERT INTO autumn_jobs (id, name, status, run_at, queue) SELECT 'u' || g, 'thumb.render', 'enqueued', now() - interval '10 minutes', 'default' FROM generate_series(1, 1000) g;
VACUUM ANALYZE autumn_jobs;
-- claim the unlimited rows the way workers do, so the running set is freshly written
UPDATE autumn_jobs SET status = 'running', claimed_by = 'w', claimed_at = now(), started_at = now() WHERE name = 'report.build';
