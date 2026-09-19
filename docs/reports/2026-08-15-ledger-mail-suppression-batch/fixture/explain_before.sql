-- Single-call EXPLAIN for the CURRENT `DbSuppressionStore::is_suppressed`
-- query (autumn/src/mail.rs, before this change) — the query
-- `send_list_mail` issues once PER RECIPIENT in its suppression loop.
--
-- Two representative cases: a recipient who really did unsubscribe (hit)
-- and one who never did (miss, the common case — 70% of the batch).

SELECT
    (SELECT subscriber FROM batch_recipients WHERE ord = 0) AS hit_subscriber,
    (SELECT subscriber FROM batch_recipients ORDER BY ord DESC LIMIT 1) AS miss_subscriber
\gset

\echo '=== BEFORE: single-recipient lookup, HIT (real unsubscribe) ==='
EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS)
SELECT COUNT(*)
FROM mail_unsubscribes
WHERE subscriber = :'hit_subscriber'
  AND list_id = 'weekly_digest';

\echo '=== BEFORE: single-recipient lookup, MISS (never unsubscribed) ==='
EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS)
SELECT COUNT(*)
FROM mail_unsubscribes
WHERE subscriber = :'miss_subscriber'
  AND list_id = 'weekly_digest';
