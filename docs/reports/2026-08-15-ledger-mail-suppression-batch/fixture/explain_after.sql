-- Single-statement EXPLAIN for the NEW `DbSuppressionStore::is_suppressed_many`
-- query (autumn/src/mail.rs, after this change): one chunk of up to 50,000
-- recipients — mirroring `subscribers.chunks(CHUNK_SIZE)` in the shipped
-- code exactly, so this plan represents ONE of the statements production
-- actually issues (batches at or under 50,000 issue exactly one such
-- statement; larger batches issue `ceil(batch / 50000)` of them — see
-- workload_after.sql, which reproduces the full chunk loop for the
-- statement-count and total-buffers numbers).

SELECT array_agg(subscriber) AS recipients
FROM (SELECT subscriber FROM batch_recipients ORDER BY ord LIMIT 50000) chunk
\gset

\echo '=== AFTER: batched lookup, one chunk (up to 50,000 recipients) ==='
EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS)
SELECT subscriber
FROM mail_unsubscribes
WHERE list_id = 'weekly_digest'
  AND subscriber = ANY(:'recipients'::text[]);
