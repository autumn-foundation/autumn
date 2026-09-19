-- Simulates ONE `Mailer::send_list_mail` call to `weekly_digest` against the
-- CURRENT (pre-fix) `DbSuppressionStore::is_suppressed`: the suppression
-- loop in autumn/src/mail.rs issues one `SELECT COUNT(*) ... WHERE
-- subscriber = $1 AND list_id = $2` per recipient in `mail.to`, sequentially,
-- same as the real Rust loop does over `&mail.to`.
--
-- Run after `SELECT pg_stat_statements_reset();` to profile this workload in
-- isolation.

DO $$
DECLARE
    r RECORD;
    hit_count BIGINT;
    total_calls BIGINT := 0;
BEGIN
    FOR r IN SELECT subscriber FROM batch_recipients ORDER BY ord LOOP
        SELECT COUNT(*) INTO hit_count
        FROM mail_unsubscribes
        WHERE subscriber = r.subscriber
          AND list_id = 'weekly_digest';
        total_calls := total_calls + 1;
    END LOOP;
    RAISE NOTICE 'workload_before: % sequential suppression-check statements issued', total_calls;
END $$;
