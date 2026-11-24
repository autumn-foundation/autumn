-- Production-shaped history for the data-retention sweep (`autumn/src/data_retention.rs`).
-- psql -v n=<rows> -f fixture.sql. Seeded (setseed), so a size is the same table every run.
--
-- autumn_jobs: :n rows of finished job history spread over 90 days, newest
-- days denser (traffic grew 3x over the window). 90% completed, 7% failed (with
-- last_error), 3% discarded. 5% carry a unique_key; a fifth of those use the
-- 'ttl' window, which the sweep must keep. Plus 0.5% in flight (enqueued/running).
-- autumn_job_tracking: :n rows, updated_at over the same 90 days, same skew,
-- a ~300 byte JSON record each.
-- autumn_repository_commit_hooks: :n finished hooks over the same 90 days, 94%
-- completed (the ack empties context/record), 3% failed and 3% after_hook_failed
-- with a last_error. Handler keys skew 70/15/10/5.
-- autumn_experiment_assignments: :n sticky assignments over the same 90 days,
-- across 40 experiments: 30 archived (sweepable), 6 running, 2 concluded and 2
-- draft (kept), assignments skewed toward the low-numbered experiments.
-- Rows go in in time order, as an app writes them: the oldest rows sit at the
-- front of the heap.
-- Ids are random v4-shaped UUIDs (md5 of a seeded random), as Uuid::new_v4 makes them.
-- A 30-day retention window makes about half of each table stale.
-- 5% of rows then take an UPDATE (a retry rewrite) and ANALYZE runs with no
-- VACUUM, so the heap and the statistics both carry dead tuples.
SELECT setseed(0.42);
TRUNCATE autumn_jobs, autumn_job_tracking, autumn_repository_commit_hooks,
         autumn_experiment_assignments, autumn_experiments;

INSERT INTO autumn_jobs (id, name, queue, payload, status, attempt, enqueued_at, run_at,
                         started_at, finished_at, claimed_by, last_error, unique_key, unique_window)
SELECT md5(random()::text || g)::uuid::text,
       (ARRAY['send_email','sync_contacts','render_pdf','webhook_deliver','reindex'])[1 + (g % 5)],
       'default',
       jsonb_build_object('user_id', g % 50000, 'template', 'welcome', 'pad', repeat('x', 120)),
       CASE WHEN r < 0.90 THEN 'completed' WHEN r < 0.97 THEN 'failed' ELSE 'discarded' END,
       1 + (g % 3),
       ts, ts, ts + interval '1 second', ts + interval '2 seconds', 'worker-' || (g % 8),
       CASE WHEN r >= 0.90 THEN 'boom: upstream timed out after 30s ' || repeat('e', 80) END,
       CASE WHEN g % 20 = 0 THEN 'uk-' || g END,
       CASE WHEN g % 20 = 0 THEN CASE WHEN g % 100 = 0 THEN 'ttl' ELSE 'running' END END
FROM (
  SELECT g, random() AS r,
         -- sqrt skew: more rows near now
         now() - interval '90 days' * (1 - sqrt(random())) AS ts
  FROM generate_series(1, :n) g
) s
ORDER BY ts;

INSERT INTO autumn_jobs (id, name, queue, payload, status, enqueued_at, run_at)
SELECT md5('live' || g)::uuid::text, 'send_email', 'default', '{}'::jsonb,
       CASE WHEN g % 2 = 0 THEN 'enqueued' ELSE 'running' END, now(), now()
FROM generate_series(1, (:n / 200)) g;

INSERT INTO autumn_job_tracking (key, record, updated_at, expires_at)
SELECT 'job:tracking:' || md5(random()::text || g)::uuid::text,
       jsonb_build_object('job_id', g, 'state', 'completed', 'progress', 100,
                          'message', repeat('m', 200)),
       ts, ts + interval '1 day'
FROM (SELECT g, now() - interval '90 days' * (1 - sqrt(random())) AS ts
      FROM generate_series(1, :n) g) s
ORDER BY ts;

INSERT INTO autumn_repository_commit_hooks (id, handler_key, hook_name, status, enqueued_at, run_at,
                                            started_at, finished_at, claimed_by, last_error)
SELECT md5(random()::text || g)::uuid::text,
       CASE WHEN h < 0.70 THEN 'posts' WHEN h < 0.85 THEN 'comments' WHEN h < 0.95 THEN 'users' ELSE 'billing' END,
       'after_commit',
       CASE WHEN r < 0.94 THEN 'completed' WHEN r < 0.97 THEN 'failed' ELSE 'after_hook_failed' END,
       ts, ts, ts + interval '1 second', ts + interval '2 seconds', 'worker-' || (g % 8),
       CASE WHEN r >= 0.94 THEN 'hook failed: ' || repeat('e', 80) END
FROM (SELECT g, random() AS r, random() AS h,
             now() - interval '90 days' * (1 - sqrt(random())) AS ts
      FROM generate_series(1, :n) g) s
ORDER BY ts;

INSERT INTO autumn_experiments (name, state)
SELECT 'exp_' || e,
       (CASE WHEN e <= 30 THEN 'archived' WHEN e <= 36 THEN 'running'
             WHEN e <= 38 THEN 'concluded' ELSE 'draft' END)::autumn_experiment_state
FROM generate_series(1, 40) e;

INSERT INTO autumn_experiment_assignments (experiment, actor, variant, assigned_at)
SELECT 'exp_' || (1 + floor(40 * power(random(), 2)))::int, 'user:' || g,
       CASE WHEN g % 2 = 0 THEN 'control' ELSE 'treatment' END, ts
FROM (SELECT g, now() - interval '90 days' * (1 - sqrt(random())) AS ts
      FROM generate_series(1, :n) g) s
ORDER BY ts;

UPDATE autumn_jobs SET attempt = attempt + 1 WHERE (hashtext(id) % 20) = 0;
UPDATE autumn_job_tracking SET version = version + 1 WHERE (hashtext(key) % 20) = 0;
UPDATE autumn_repository_commit_hooks SET attempt = attempt + 1 WHERE (hashtext(id) % 20) = 0;
UPDATE autumn_experiment_assignments SET is_override = true WHERE id % 20 = 0;
ANALYZE autumn_jobs;
ANALYZE autumn_job_tracking;
ANALYZE autumn_repository_commit_hooks;
ANALYZE autumn_experiments;
ANALYZE autumn_experiment_assignments;
