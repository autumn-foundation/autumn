-- Production-shaped autumn_repository_commit_hooks. psql -v hist=<rows>.
-- Retention is "forever" by default, so finished history is the bulk of the
-- table. History spans 30 days in insert order: 94% completed (ack empties
-- context/record), 3% failed and 3% after_hook_failed (both keep a
-- last_error; failed keeps its payload). Handler keys skew 70/15/10/5.
-- In flight: 8 running (2 claimed past the 60 s stale window), 6
-- pending_after_hook (2 stale), 4 after_hook_succeeded (1 stale), 20
-- enqueued retries backed off an hour.
SELECT setseed(0.3742);
INSERT INTO autumn_repository_commit_hooks
  (id, handler_key, hook_name, context, record, status, attempt, max_attempts,
   initial_backoff_ms, enqueued_at, started_at, finished_at, claimed_by,
   claimed_at, last_error, run_at)
SELECT md5('h' || g)::uuid::text,
       CASE WHEN k <= 70 THEN 'app::PostRepository' WHEN k <= 85 THEN 'app::CommentRepository'
            WHEN k <= 95 THEN 'app::UserRepository' ELSE 'app::VoteRepository' END,
       (ARRAY['after_create','after_update','after_delete'])[1 + g % 3],
       CASE WHEN s = 'failed' THEN '{"request_id":"01JD0000000000000000000000","actor":"user:42","tenant":"t-1"}'::jsonb ELSE '{}'::jsonb END,
       CASE WHEN s = 'failed' THEN '{"id":12345,"title":"A post title that is about sixty characters long, give or take","score":17,"author_id":42}'::jsonb ELSE '{}'::jsonb END,
       s,
       CASE WHEN s = 'failed' THEN 5 ELSE 1 END, 5, 1000,
       t, t + interval '20 ms', t + interval '45 ms', NULL, NULL,
       CASE WHEN s <> 'completed' THEN 'handler returned error: upstream channel closed while broadcasting fragment (attempt ' || (g % 5 + 1) || ')' END,
       t
FROM (
  SELECT g, (random() * 99)::int + 1 AS k,
         CASE WHEN r < 0.94 THEN 'completed' WHEN r < 0.97 THEN 'failed' ELSE 'after_hook_failed' END AS s,
         now() - interval '30 days' + (g::float8 / :hist) * interval '30 days' - interval '2 hours' AS t
  FROM (SELECT g, random() AS r FROM generate_series(1, :hist) g) x
) y;

INSERT INTO autumn_repository_commit_hooks
  (id, handler_key, hook_name, context, record, status, attempt, enqueued_at, started_at, claimed_by, claimed_at, run_at)
SELECT 'inflight-' || st || '-' || i, 'app::PostRepository', 'after_update',
       '{"request_id":"01JD0000000000000000000000","actor":"user:42","tenant":"t-1"}'::jsonb,
       '{"id":12345,"title":"A post title that is about sixty characters long, give or take","score":17}'::jsonb,
       st, 1, now() - age, CASE WHEN st = 'running' THEN now() - age END,
       CASE WHEN st = 'enqueued' THEN NULL ELSE 'replica-' || (i % 3) END,
       CASE WHEN st = 'enqueued' THEN NULL ELSE now() - age END,
       CASE WHEN st = 'enqueued' THEN now() + interval '1 hour' ELSE now() - age END
FROM (VALUES ('running', 8, 2), ('pending_after_hook', 6, 2), ('after_hook_succeeded', 4, 1), ('enqueued', 20, 0)) v(st, n, stale),
     generate_series(1, n) i,
     LATERAL (SELECT CASE WHEN i <= stale THEN interval '5 minutes' ELSE interval '2 seconds' END AS age) a;

VACUUM (ANALYZE) autumn_repository_commit_hooks;
