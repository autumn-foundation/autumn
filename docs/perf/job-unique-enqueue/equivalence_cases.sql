-- Edge cases of the shipped enqueue INSERT's dedup guard (:'insert', with
-- the store's typed $n binds). Each case seeds rows, runs one enqueue in a
-- transaction pinned to one now(), prints whether it inserted, and rolls
-- back. Run once on each schema; the outputs must be identical.
-- diesel-async prepares a raw sql_query anew on every call: always a custom
-- plan. One shared PREPARE here would turn generic after five executions.
SET plan_cache_mode = force_custom_plan;
\set types 'text, text, text, integer, bigint, text, text, bigint, text, integer, timestamptz, text, bigint'
PREPARE ins(:types) AS :insert;
CREATE FUNCTION pg_temp.enqueue(p_case text, p_name text, p_key text, p_win text, p_ttl bigint)
RETURNS TABLE (case_name text, inserted bigint, dedup_scan text) LANGUAGE plpgsql AS $$
DECLARE n bigint; line text; scan text := '(no dedup scan)';
  stmt text := format('EXECUTE ins(%L, %L, %L, 5, 250, %L, %L, %L, NULL, NULL, NULL, %L, NULL)',
                      'eq-' || p_case, p_name, '{}', p_key, p_win, p_ttl, 'default');
BEGIN
  -- The InitPlan's scan line: Seq Scan, or the index that serves the check.
  FOR line IN EXECUTE 'EXPLAIN (COSTS OFF) ' || stmt LOOP
    IF line ~ 'on autumn_jobs dup' THEN scan := regexp_replace(line, '^\s*->\s*', ''); END IF;
  END LOOP;
  EXECUTE stmt;
  SELECT count(*) INTO n FROM autumn_jobs WHERE id = 'eq-' || p_case;
  RETURN QUERY SELECT p_case, n, scan;
END $$;

\pset footer off
-- TTL window, 1 hour.
BEGIN;
INSERT INTO autumn_jobs (id, name, status, enqueued_at, finished_at, unique_key, unique_window) VALUES
  ('a1', 'r', 'completed', now() - interval '30 minutes', now() - interval '29 minutes', 'k-done-in', 'ttl'),
  ('a2', 'r', 'completed', now() - interval '2 hours', now() - interval '119 minutes', 'k-done-out', 'ttl'),
  ('a3', 'r', 'completed', now() - interval '1 hour', now(), 'k-boundary', 'ttl'),
  ('a4', 'r', 'completed', now() - interval '1 hour' + interval '1 microsecond', now(), 'k-boundary-in', 'ttl'),
  ('a5', 'other', 'completed', now() - interval '1 minute', now(), 'k-other-name', 'ttl'),
  ('a6', 'r', 'completed', now() - interval '1 minute', now(), NULL, 'pending'),
  ('a7', 'r', 'running', now() - interval '3 hours', NULL, 'k-inflight-old', 'ttl'),
  ('a8', 'r', 'failed', now() - interval '5 minutes', now(), 'k-failed-in', 'ttl'),
  ('a9', 'r', 'completed', now() - interval '5 minutes', now(), 'k-was-running-window', 'running'),
  ('a10', 'r', 'enqueued', now() - interval '5 minutes', NULL, 'k-inflight-in', 'ttl'),
  ('a11', 'r', 'discarded', now() - interval '5 minutes', now(), 'K-Case', 'ttl'),
  ('a12', 'r', 'completed', now() - interval '5 minutes', now(), 'k-done-only', 'running');
UPDATE autumn_jobs SET pending_unique_key = 'k-pending-only' WHERE id = 'a6';
SELECT * FROM pg_temp.enqueue('ttl: finished in window', 'r', 'k-done-in', 'ttl', 3600000)
UNION ALL SELECT * FROM pg_temp.enqueue('ttl: finished before window', 'r', 'k-done-out', 'ttl', 3600000)
UNION ALL SELECT * FROM pg_temp.enqueue('ttl: enqueued_at = now() - ttl', 'r', 'k-boundary', 'ttl', 3600000)
UNION ALL SELECT * FROM pg_temp.enqueue('ttl: 1us inside the boundary', 'r', 'k-boundary-in', 'ttl', 3600000)
UNION ALL SELECT * FROM pg_temp.enqueue('ttl: same key, other name', 'r', 'k-other-name', 'ttl', 3600000)
UNION ALL SELECT * FROM pg_temp.enqueue('ttl: key only in pending_unique_key', 'r', 'k-pending-only', 'ttl', 3600000)
UNION ALL SELECT * FROM pg_temp.enqueue('ttl: in flight, enqueued before window', 'r', 'k-inflight-old', 'ttl', 3600000)
UNION ALL SELECT * FROM pg_temp.enqueue('ttl: failed in window', 'r', 'k-failed-in', 'ttl', 3600000)
UNION ALL SELECT * FROM pg_temp.enqueue('ttl: running-window row in window', 'r', 'k-was-running-window', 'ttl', 3600000)
UNION ALL SELECT * FROM pg_temp.enqueue('ttl: in flight in window', 'r', 'k-inflight-in', 'ttl', 3600000)
UNION ALL SELECT * FROM pg_temp.enqueue('ttl: key differs in case', 'r', 'k-case', 'ttl', 3600000)
UNION ALL SELECT * FROM pg_temp.enqueue('ttl: empty key', 'r', '', 'ttl', 3600000)
UNION ALL SELECT * FROM pg_temp.enqueue('ttl: ttl 0', 'r', 'k-done-in', 'ttl', 0)
UNION ALL SELECT * FROM pg_temp.enqueue('running: finished only', 'r', 'k-done-only', 'running', NULL)
UNION ALL SELECT * FROM pg_temp.enqueue('running: in flight', 'r', 'k-inflight-in', 'running', NULL)
UNION ALL SELECT * FROM pg_temp.enqueue('plain: no key', 'r', NULL, NULL, NULL);
SELECT 'rows after: ' || count(*) || ', inserted ids: ' || string_agg(id, ' ' ORDER BY id)
FROM autumn_jobs WHERE id LIKE 'eq-%';
ROLLBACK;
