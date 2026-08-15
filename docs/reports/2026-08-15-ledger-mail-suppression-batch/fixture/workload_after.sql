-- Simulates ONE `Mailer::send_list_mail` call to `weekly_digest` against the
-- NEW (post-fix) `DbSuppressionStore::is_suppressed_many`: ONE batched
-- `SELECT subscriber ... WHERE list_id = $1 AND subscriber = ANY($2)` query
-- covers the entire recipient batch.
--
-- Run after `SELECT pg_stat_statements_reset();` to profile this workload in
-- isolation.

DO $$
DECLARE
    recipients TEXT[];
    hit_count INTEGER;
BEGIN
    SELECT array_agg(subscriber) INTO recipients FROM batch_recipients;

    SELECT COUNT(*) INTO hit_count
    FROM (
        SELECT subscriber
        FROM mail_unsubscribes
        WHERE list_id = 'weekly_digest'
          AND subscriber = ANY(recipients)
    ) suppressed;

    RAISE NOTICE 'workload_after: 1 batched suppression-check statement issued, % of % recipients suppressed',
        hit_count, array_length(recipients, 1);
END $$;
