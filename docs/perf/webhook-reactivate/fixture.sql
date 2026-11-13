-- Production-shaped fixture for the outbound webhook store.
--
-- 200 subscriptions: 194 active with no failures, 3 active with a flaky
-- receiver (1-4 failures in a row), 2 disabled, 1 failed. Topic lists are
-- JSON arrays, as the store writes them.
--
-- 1,000,000 delivery logs over 30 days, the retention most apps keep, with
-- the subscription skew of the workload (a few receivers take most events):
-- 97.5% delivered (2xx), 1% failed and retrying, 1% in the DLQ, 0.5% pending.
-- setseed makes every run write the same rows.
SELECT setseed(0.3742);

INSERT INTO autumn_webhook_subscriptions
    (id, target_url, event_topics, secret, status, consecutive_failures)
SELECT g::text,
       'https://hooks.example.com/r/' || g,
       '["order.created","order.updated","invoice.paid"]',
       'whsec_' || md5(g::text),
       CASE WHEN g IN (150, 151) THEN 'disabled' WHEN g = 152 THEN 'failed' ELSE 'active' END,
       CASE WHEN g IN (7, 40, 90) THEN 1 + g % 4 WHEN g = 152 THEN 50 ELSE 0 END
FROM generate_series(1, 200) g;

INSERT INTO autumn_webhook_deliveries
    (id, subscription_id, topic, payload, request_headers, response_status, response_body,
     elapsed_ms, attempt, max_attempts, is_dlq, last_error, logged_at)
SELECT 'd' || g,
       -- ~ zipf over 200 receivers
       (1 + floor(200 * power(random(), 3)))::int::text,
       (ARRAY['order.created','order.updated','invoice.paid'])[1 + g % 3],
       '{"id":"ord_' || g || '","event":"order.updated","note":"' || repeat('x', 600 + (g % 300)) || '"}',
       '{"content-type":"application/json","webhook-id":"d' || g || '"}',
       CASE WHEN k < 0.975 THEN 200 + (g % 2) * 4
            WHEN k < 0.995 THEN 500
            ELSE NULL END,
       CASE WHEN k < 0.975 THEN 'ok' WHEN k < 0.995 THEN 'boom' ELSE NULL END,
       20 + (g % 380),
       CASE WHEN k < 0.975 THEN 1 WHEN k < 0.985 THEN 2 WHEN k < 0.995 THEN 5 ELSE 1 END,
       5,
       k >= 0.985 AND k < 0.995,
       CASE WHEN k >= 0.975 AND k < 0.995 THEN 'server returned status: 500' ELSE NULL END,
       1762905600000 - (g::bigint * 2592000000 / 1000000)
FROM (SELECT g, random() AS k FROM generate_series(1, 1000000) g) s;
