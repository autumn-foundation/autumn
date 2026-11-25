-- Outbox and inbox history inside the retention window: :hist messages
-- already sent (7-day `retention_ms` at the relay's steady rate), written by
-- the framework writers, so each has its own id as the aggregate. Topics,
-- payload sizes and the inbox share follow gen_scripts.py. Ids are :idfn
-- (gen_random_uuid()::text: every message before this change made a v4).
-- Run with -v hist=<n> -v idfn=<sql expr> -v now=<epoch ms>.
INSERT INTO autumn_outbox
  (id, aggregate, topic, payload, created_at, available_at, attempts, dispatched_at)
SELECT id, id, topic,
       '{"type":"' || topic || '","n":' || g || ',"data":{"note":"' || repeat('x', (120 + (g * 31) % 160)::int) || '"}}',
       :now - 604800000 + g * (604800000 / :hist),
       :now - 604800000 + g * (604800000 / :hist),
       1,
       :now - 604800000 + g * (604800000 / :hist) + 40
  FROM (SELECT g, :idfn AS id,
               (ARRAY['autumn.event','autumn.event','autumn.event','autumn.event',
                      'autumn.event','autumn.event','autumn.event','autumn.event',
                      'autumn.job','autumn.job','autumn.job','autumn.job','autumn.job','autumn.job',
                      'autumn.mail','autumn.mail','autumn.mail',
                      'autumn.webhook','autumn.webhook','autumn.webhook'])[1 + (g * 7) % 20] AS topic
          FROM generate_series(1::bigint, :hist) g) s
 ORDER BY g;
INSERT INTO autumn_inbox (consumer, message_id, seen_at)
SELECT 'listeners', id, dispatched_at - 10 FROM autumn_outbox WHERE topic = 'autumn.event' ORDER BY seq;
VACUUM (ANALYZE) autumn_outbox;
VACUUM (ANALYZE) autumn_inbox;
