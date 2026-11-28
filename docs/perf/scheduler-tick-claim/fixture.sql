-- Steady state of a fleet's tick table at the default 300 s retention.
-- 30 tasks: task-1..10 every 1 s, task-11..20 every 10 s, task-21..30
-- every 60 s. Every tick of the last (retention + period) seconds holds a
-- row, as it would after the fleet has run for longer than that. Plus the
-- dead versions of ~700 pruned rows: the autovacuum threshold of a table this
-- size (50 + 0.2 x 3,380), so the table is as dirty as it gets between two
-- autovacuum runs. Deterministic: the row set depends only on the clock.
DROP TABLE IF EXISTS autumn_scheduler_ticks;
\i :sqldir/ddl.sql
CREATE OR REPLACE FUNCTION pg_temp.period(t int) RETURNS int
  LANGUAGE sql IMMUTABLE AS $$ SELECT CASE WHEN t <= 10 THEN 1 WHEN t <= 20 THEN 10 ELSE 60 END $$;
-- Expired rows, then their prune: dead tuples left for vacuum.
INSERT INTO autumn_scheduler_ticks (key_prefix, task_name, tick_key, owner, expires_at, claimed_at)
SELECT 'autumn:scheduler', 'task-' || t,
       'task-' || t || ':' || (floor(extract(epoch FROM now()) / pg_temp.period(t))::bigint - 400 - k),
       'replica-' || (k % 6),
       now() - make_interval(secs => k + 1), now() - make_interval(secs => 300 + k)
FROM generate_series(1, 10) t, generate_series(0, 69) k;
DELETE FROM autumn_scheduler_ticks WHERE expires_at < now();
-- Live rows: one per tick inside the hold window.
INSERT INTO autumn_scheduler_ticks (key_prefix, task_name, tick_key, owner, expires_at, claimed_at)
SELECT 'autumn:scheduler', 'task-' || t,
       'task-' || t || ':' || (b.bucket - k),
       'replica-' || ((t + k) % 6),
       now() + make_interval(secs => 300 + pg_temp.period(t) - k * pg_temp.period(t)),
       now() - make_interval(secs => k * pg_temp.period(t))
FROM generate_series(1, 30) t
CROSS JOIN LATERAL (SELECT floor(extract(epoch FROM now()) / pg_temp.period(t))::bigint AS bucket) b
CROSS JOIN LATERAL generate_series(1, (300 + pg_temp.period(t)) / pg_temp.period(t)) k;
ANALYZE autumn_scheduler_ticks;
SELECT count(*) AS live_rows FROM autumn_scheduler_ticks;
SELECT n_live_tup, n_dead_tup FROM pg_stat_user_tables WHERE relname = 'autumn_scheduler_ticks';
