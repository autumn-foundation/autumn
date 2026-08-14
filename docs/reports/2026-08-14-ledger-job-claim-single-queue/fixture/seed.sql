-- Deterministic, production-shaped fixture generator for autumn_jobs.
-- Usage: psql -v ready=50000 -v scheduled=2000 -v history=500000 -v running=200 -f seed.sql
-- Skew modeled on a real background-job corpus:
--   * job names: Zipfian-ish, 3 hot types dominate (~75% of volume), 12-name long tail
--   * queues: 'default' 85%, 'high' 10%, 'low' 5%
--   * status mix in history: finished 90%, failed 7%, running 3% (stale-recovery candidates)
--   * a couple of names carry a concurrency_limit (real feature usage, not universal)
--   * a slice of ready rows carry a unique_key (dedup feature usage)
SELECT setseed(0.4231);

TRUNCATE autumn_jobs;

WITH names(n, w) AS (
    VALUES
    ('send_email', 40), ('process_webhook', 20), ('sync_contact', 15),
    ('generate_pdf', 5), ('cleanup_expired', 5), ('resize_image', 3),
    ('index_search_doc', 3), ('charge_invoice', 2), ('send_digest', 2),
    ('reconcile_ledger', 1), ('rotate_credentials', 1), ('archive_thread', 1),
    ('warm_cache', 1), ('publish_webhook_retry', 1)
),
names_cum AS (
    SELECT n, w,
           SUM(w) OVER (ORDER BY n) - w AS lo,
           SUM(w) OVER (ORDER BY n)     AS hi,
           SUM(w) OVER ()               AS total
    FROM names
),
-- historical (finished/failed/running) rows: bulk of the table, matches real retention
hist AS (
    SELECT
        'h-' || gs::text AS id,
        (SELECT n FROM names_cum WHERE (gs % (SELECT total FROM names_cum LIMIT 1)) >= lo
                                    AND (gs % (SELECT total FROM names_cum LIMIT 1)) < hi LIMIT 1) AS name,
        CASE WHEN random() < 0.85 THEN 'default'
             WHEN random() < 0.67 THEN 'high'
             ELSE 'low' END AS queue,
        CASE WHEN random() < 0.90 THEN 'finished'
             WHEN random() < 0.767 THEN 'failed'
             ELSE 'running' END AS status,
        NOW() - (random() * interval '30 days') - (gs || ' seconds')::interval AS base_ts
    FROM generate_series(1, :history) AS gs
)
INSERT INTO autumn_jobs (id, name, queue, status, payload, attempt, max_attempts,
                          enqueued_at, started_at, finished_at, claimed_by, claimed_at,
                          run_at, concurrency_key, concurrency_limit, unique_key)
SELECT
    id, name, queue, status,
    jsonb_build_object('n', (random()*1000)::int, 'note', repeat('x', 40)),
    (1 + (random()*3)::int), 5,
    base_ts,
    CASE WHEN status <> 'enqueued' THEN base_ts + interval '2 seconds' END,
    CASE WHEN status = 'finished' OR status = 'failed' THEN base_ts + interval '4 seconds' END,
    CASE WHEN status <> 'enqueued' THEN 'worker-' || (1 + (random()*8)::int) END,
    CASE WHEN status = 'running' THEN NOW() - (random() * interval '10 minutes') END,
    base_ts,
    CASE WHEN name IN ('reconcile_ledger', 'rotate_credentials') THEN name END,
    CASE WHEN name IN ('reconcile_ledger', 'rotate_credentials') THEN 1 END,
    NULL
FROM hist;

-- ready-to-claim rows: run_at <= now, status = 'enqueued'
WITH names(n, w) AS (
    VALUES
    ('send_email', 40), ('process_webhook', 20), ('sync_contact', 15),
    ('generate_pdf', 5), ('cleanup_expired', 5), ('resize_image', 3),
    ('index_search_doc', 3), ('charge_invoice', 2), ('send_digest', 2),
    ('reconcile_ledger', 1), ('rotate_credentials', 1), ('archive_thread', 1),
    ('warm_cache', 1), ('publish_webhook_retry', 1)
),
names_cum AS (
    SELECT n, w,
           SUM(w) OVER (ORDER BY n) - w AS lo,
           SUM(w) OVER (ORDER BY n)     AS hi,
           SUM(w) OVER ()               AS total
    FROM names
),
ready AS (
    SELECT
        'r-' || gs::text AS id,
        (SELECT n FROM names_cum WHERE (gs % (SELECT total FROM names_cum LIMIT 1)) >= lo
                                    AND (gs % (SELECT total FROM names_cum LIMIT 1)) < hi LIMIT 1) AS name,
        CASE WHEN random() < 0.85 THEN 'default'
             WHEN random() < 0.67 THEN 'high'
             ELSE 'low' END AS queue,
        NOW() - (gs || ' milliseconds')::interval AS enq_ts
    FROM generate_series(1, :ready) AS gs
)
INSERT INTO autumn_jobs (id, name, queue, status, payload, attempt, max_attempts,
                          enqueued_at, run_at, concurrency_key, concurrency_limit, unique_key)
SELECT
    id, name, queue, 'enqueued',
    jsonb_build_object('n', (random()*1000)::int, 'note', repeat('x', 40)),
    1, 5,
    enq_ts, enq_ts,
    CASE WHEN name IN ('reconcile_ledger', 'rotate_credentials') THEN name END,
    CASE WHEN name IN ('reconcile_ledger', 'rotate_credentials') THEN 1 END,
    CASE WHEN random() < 0.05 THEN id END
FROM ready;

-- scheduled-future rows (run_at in the future): dashboard "scheduled" tab
WITH names(n, w) AS (
    VALUES
    ('send_email', 40), ('process_webhook', 20), ('sync_contact', 15),
    ('generate_pdf', 5), ('cleanup_expired', 5), ('resize_image', 3),
    ('index_search_doc', 3), ('charge_invoice', 2), ('send_digest', 2),
    ('reconcile_ledger', 1), ('rotate_credentials', 1), ('archive_thread', 1),
    ('warm_cache', 1), ('publish_webhook_retry', 1)
),
names_cum AS (
    SELECT n, w,
           SUM(w) OVER (ORDER BY n) - w AS lo,
           SUM(w) OVER (ORDER BY n)     AS hi,
           SUM(w) OVER ()               AS total
    FROM names
),
sched AS (
    SELECT
        's-' || gs::text AS id,
        (SELECT n FROM names_cum WHERE (gs % (SELECT total FROM names_cum LIMIT 1)) >= lo
                                    AND (gs % (SELECT total FROM names_cum LIMIT 1)) < hi LIMIT 1) AS name,
        'default' AS queue,
        NOW() + (random() * interval '6 hours') AS run_ts
    FROM generate_series(1, :scheduled) AS gs
)
INSERT INTO autumn_jobs (id, name, queue, status, payload, attempt, max_attempts, enqueued_at, run_at)
SELECT id, name, queue, 'enqueued', '{}'::jsonb, 1, 5, NOW(), run_ts
FROM sched;

VACUUM ANALYZE autumn_jobs;

SELECT status, count(*) FROM autumn_jobs GROUP BY status ORDER BY 2 DESC;
SELECT queue, status, count(*) FROM autumn_jobs GROUP BY queue, status ORDER BY 1,2;
