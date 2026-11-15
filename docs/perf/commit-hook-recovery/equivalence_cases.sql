-- Edge rows for the stale-recovery UPDATEs, on top of fixture.sql. now() is
-- frozen (equivalence.sh), so the cutoff NOW() - 60 s is a fixed instant.
-- Every status, crossed with claimed_at NULL, far past, 1 us before the
-- cutoff, exactly at it, 1 us after, and fresh; with and without last_error;
-- with a real and an empty payload.
INSERT INTO autumn_repository_commit_hooks
  (id, handler_key, hook_name, context, record, status, attempt, max_attempts,
   enqueued_at, started_at, finished_at, claimed_by, claimed_at, last_error, run_at)
SELECT 'edge-' || st || '-' || c.label || '-' || e.label || '-' || p.label,
       'app::PostRepository', 'after_update', p.payload, p.payload, st,
       CASE WHEN c.label = 'old' THEN 5 ELSE 2 END, 5,
       now() - interval '1 hour', now() - interval '10 minutes',
       CASE WHEN st IN ('completed', 'failed', 'after_hook_failed') THEN now() - interval '9 minutes' END,
       CASE WHEN c.at IS NULL THEN NULL ELSE 'replica-9' END, c.at, e.err, now() - interval '1 hour'
FROM unnest(ARRAY['enqueued', 'running', 'pending_after_hook', 'after_hook_succeeded',
                  'after_hook_failed', 'completed', 'failed']) st,
     (VALUES ('null', NULL::timestamptz), ('old', now() - interval '3 days'),
             ('before', now() - interval '60 seconds' - interval '1 microsecond'),
             ('at', now() - interval '60 seconds'),
             ('after', now() - interval '60 seconds' + interval '1 microsecond'),
             ('fresh', now() - interval '1 second')) c(label, at),
     (VALUES ('noerr', NULL::text), ('err', 'earlier failure')) e(label, err),
     (VALUES ('payload', '{"id":7,"title":"t"}'::jsonb), ('empty', '{}'::jsonb)) p(label, payload);
ANALYZE autumn_repository_commit_hooks;
