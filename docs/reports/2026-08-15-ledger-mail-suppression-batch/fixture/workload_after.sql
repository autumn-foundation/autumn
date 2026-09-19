-- Simulates ONE `Mailer::send_list_mail` call to `weekly_digest` against the
-- NEW (post-fix) `DbSuppressionStore::is_suppressed_many`: one batched
-- `SELECT subscriber ... WHERE list_id = $1 AND subscriber = ANY($2)` query
-- PER CHUNK of up to `CHUNK_SIZE` recipients — mirroring
-- `DbSuppressionStore::is_suppressed_many`'s `subscribers.chunks(CHUNK_SIZE)`
-- loop in autumn/src/mail.rs exactly, so batches above `CHUNK_SIZE` issue the
-- same number of statements the shipped code issues, not one statement
-- covering the whole recipient list regardless of size (an earlier version
-- of this harness did exactly that and was caught in review: at the
-- CHUNK_SIZE the code shipped with at the time, a 20,000-recipient batch was
-- 4 chunked statements in production, not the 1 this harness measured).
--
-- Run after `SELECT pg_stat_statements_reset();` to profile this workload in
-- isolation.

DO $$
DECLARE
    CHUNK_SIZE CONSTANT INTEGER := 50000;
    recipients TEXT[];
    chunk TEXT[];
    chunk_start INTEGER;
    total INTEGER;
    hit_count INTEGER;
    total_hits INTEGER := 0;
    statement_count INTEGER := 0;
BEGIN
    SELECT array_agg(subscriber) INTO recipients FROM batch_recipients;
    total := array_length(recipients, 1);

    FOR chunk_start IN 1..total BY CHUNK_SIZE LOOP
        chunk := recipients[chunk_start : LEAST(chunk_start + CHUNK_SIZE - 1, total)];

        SELECT COUNT(*) INTO hit_count
        FROM (
            SELECT subscriber
            FROM mail_unsubscribes
            WHERE list_id = 'weekly_digest'
              AND subscriber = ANY(chunk)
        ) suppressed;

        total_hits := total_hits + hit_count;
        statement_count := statement_count + 1;
    END LOOP;

    RAISE NOTICE 'workload_after: % chunked suppression-check statement(s) issued, % of % recipients suppressed',
        statement_count, total_hits, total;
END $$;
