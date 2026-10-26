-- Deterministic outbox backlog (no random(); every column is a function of g).
-- {HISTORY} rows already dispatched but inside the retention window (older,
-- lower seq), then 50,000 pending rows across 5,000 aggregates (squared skew:
-- agg-0 is the hottest). Of the pending rows 20% are in retry backoff
-- (available_at = now + 10 min, attempts 3), and 5% of the ready remainder are
-- leased by another relay (locked_until = now + 5 min). {NOW} is epoch ms.
INSERT INTO autumn_outbox
  (id, aggregate, topic, payload, created_at, available_at, attempts,
   claim_token, locked_until, dispatched_at)
SELECT 'm' || g,
       'agg-' || (floor(5000 * ((g * 7919) % 1000 / 1000.0) * ((g * 104729) % 1000 / 1000.0)))::int,
       (ARRAY['order.placed','mail.deliver','webhook.dispatch','job.enqueue'])[1 + (g % 4)],
       repeat('x', 200),
       {NOW} - 4000000 + g,
       CASE WHEN g > {HISTORY} AND g % 5 = 0 THEN {NOW} + 600000 ELSE {NOW} - 4000000 + g END,
       CASE WHEN g > {HISTORY} AND g % 5 = 0 THEN 3 ELSE 0 END,
       CASE WHEN g > {HISTORY} AND g % 5 <> 0 AND g % 20 = 1 THEN 'other' END,
       CASE WHEN g > {HISTORY} AND g % 5 <> 0 AND g % 20 = 1 THEN {NOW} + 300000 END,
       CASE WHEN g <= {HISTORY} THEN {NOW} - 200000000 + g END
FROM generate_series(1::bigint, {HISTORY} + 50000) g;
ANALYZE autumn_outbox;
