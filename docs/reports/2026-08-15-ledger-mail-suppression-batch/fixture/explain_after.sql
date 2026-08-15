-- Single-call EXPLAIN for the NEW `DbSuppressionStore::is_suppressed_many`
-- query (autumn/src/mail.rs, after this change) — ONE statement for the
-- WHOLE recipient batch instead of one per recipient.

SELECT array_agg(subscriber) AS recipients FROM batch_recipients \gset

\echo '=== AFTER: batched lookup, whole recipient batch in one statement ==='
EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS)
SELECT subscriber
FROM mail_unsubscribes
WHERE list_id = 'weekly_digest'
  AND subscriber = ANY(:'recipients'::text[]);
