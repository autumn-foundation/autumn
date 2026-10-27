-- Result-equivalence of claim_baseline.sql vs claim_after.sql (+ after_index.sql).
-- Two schemas (eqa = baseline, eqb = rewritten + new index) hold identical edge-case data.
-- For each deferred set the queue is drained claim-by-claim with each variant and the
-- sequences of claimed (run_at, name) are compared, plus the claimed id sets (equal-run_at
-- ties may legitimately pick different ids, so ids are compared as sets per run_at).
-- Run: psql -v ON_ERROR_STOP=1 -f equivalence.sql   (cwd = this directory)
\set base `sed -e "s/RETURNING id AS jid;/RETURNING id;/" claim_baseline.sql`
\set after `sed -e "s/RETURNING id AS jid;/RETURNING id;/" claim_after.sql`
DROP TABLE IF EXISTS eq_cases, eq_sql, eq_out; DROP SCHEMA IF EXISTS eqa CASCADE; DROP SCHEMA IF EXISTS eqb CASCADE;
CREATE SCHEMA eqa; CREATE SCHEMA eqb;
SET search_path = eqa;
\i ../../autumn/migrations/20260513000000_create_job_queue/up.sql
\i ../../autumn/migrations/20260519000000_add_trace_context_to_jobs/up.sql
\i ../../autumn/migrations/20260610000000_add_job_uniqueness_concurrency/up.sql
\i ../../autumn/migrations/20260611000000_add_pending_unique_key_to_jobs/up.sql
\i ../../autumn/migrations/20260628000000_add_queue_to_jobs/up.sql
SET search_path = eqb;
\i ../../autumn/migrations/20260513000000_create_job_queue/up.sql
\i ../../autumn/migrations/20260519000000_add_trace_context_to_jobs/up.sql
\i ../../autumn/migrations/20260610000000_add_job_uniqueness_concurrency/up.sql
\i ../../autumn/migrations/20260611000000_add_pending_unique_key_to_jobs/up.sql
\i ../../autumn/migrations/20260628000000_add_queue_to_jobs/up.sql
\i after_index.sql
RESET search_path;

-- Edge data: NULL / non-NULL concurrency keys with limits, equal-run_at ties, a future row,
-- a row 1us in the past (boundary), a second queue, names sorting before/after/around the
-- deferred ones, a name that is a prefix of another, and a running row holding a slot.
CREATE TEMP TABLE seed AS
SELECT 'a'||g AS id, (ARRAY['alpha','report.slow','report.slow2','zeta','job.n3','lim'])[1+g%6] AS name,
       TIMESTAMPTZ '2026-01-01' + (g/2) * interval '1 second' AS run_at,
       CASE WHEN g%11=0 THEN 'other' ELSE 'default' END AS queue,
       CASE WHEN g%6=5 THEN (ARRAY['k1','k2',NULL])[1+g%3] END AS concurrency_key,
       CASE WHEN g%6=5 THEN 2 END AS concurrency_limit
FROM generate_series(1,300) g;
INSERT INTO seed VALUES ('future1','alpha',now()+interval '1 hour','default',NULL,NULL),
                        ('edge1','zeta',now()-interval '1 microsecond','default',NULL,NULL);
INSERT INTO eqa.autumn_jobs (id,name,status,run_at,queue,concurrency_key,concurrency_limit)
  SELECT id,name,'enqueued',run_at,queue,concurrency_key,concurrency_limit FROM seed;
INSERT INTO eqa.autumn_jobs (id,name,status,run_at,queue,claimed_at) VALUES ('run1','lim','running',now(),'default',now());
INSERT INTO eqb.autumn_jobs SELECT * FROM eqa.autumn_jobs;
ANALYZE eqa.autumn_jobs; ANALYZE eqb.autumn_jobs;

CREATE TABLE eq_cases(d text[]);
INSERT INTO eq_cases VALUES ('{}'), ('{report.slow}'), ('{report.slow,job.n3,nonexistent}'),
  ('{alpha,zeta,lim,report.slow,report.slow2,job.n3}'), ('{report}');
CREATE TABLE eq_sql(variant text, sql text);
INSERT INTO eq_sql VALUES ('a', :'base'), ('b', :'after');
CREATE TABLE eq_out(d text[], variant text, i int, id text, run_at timestamptz, name text);

DO $$
DECLARE c record; v text; sql text; r text; i int; sch text;
BEGIN
  FOR c IN SELECT d FROM eq_cases LOOP
    FOREACH v IN ARRAY ARRAY['a','b'] LOOP
      -- fresh copy of the seed state per (case, variant)
      EXECUTE format('SET LOCAL search_path = eq%s', v);
      UPDATE autumn_jobs SET status = 'enqueued', claimed_by = NULL WHERE id <> 'run1';
      SELECT e.sql INTO sql FROM public.eq_sql e WHERE e.variant = v;
      sql := replace(sql, '{report.slow}', c.d::text);
      i := 0;
      LOOP
        EXECUTE sql INTO r;
        EXIT WHEN r IS NULL;
        i := i + 1;
        INSERT INTO public.eq_out SELECT c.d, v, i, id, run_at, name FROM autumn_jobs WHERE id = r;
      END LOOP;
    END LOOP;
  END LOOP;
END $$;

-- same_run_at_sequence: the i-th claim has the same run_at in both variants.
-- same_tie_groups: within each run_at, the same ids were claimed (order inside a tie is
-- unspecified by ORDER BY run_at in the original too).
SELECT d AS deferred,
       (SELECT count(*) FROM eq_out o WHERE o.d = c.d AND variant='a') AS claimed_baseline,
       (SELECT count(*) FROM eq_out o WHERE o.d = c.d AND variant='b') AS claimed_after,
       NOT EXISTS (SELECT 1 FROM eq_out a JOIN eq_out b ON a.d=b.d AND a.i=b.i AND a.variant='a' AND b.variant='b'
                   WHERE a.d=c.d AND a.run_at IS DISTINCT FROM b.run_at) AS same_run_at_sequence,
       NOT EXISTS (
         SELECT 1 FROM (SELECT run_at, array_agg(id ORDER BY id) ids FROM eq_out WHERE d=c.d AND variant='a' GROUP BY run_at) x
         FULL JOIN (SELECT run_at, array_agg(id ORDER BY id) ids FROM eq_out WHERE d=c.d AND variant='b' GROUP BY run_at) y USING (run_at)
         WHERE x.ids IS DISTINCT FROM y.ids) AS same_tie_groups
FROM eq_cases c;
