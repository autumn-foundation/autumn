-- Deterministic, production-shaped `mail_unsubscribes` fixture plus a fixed
-- recipient batch for the `send_list_mail` suppression-check workload.
--
-- Usage:
--   psql -d ledger_bench -v total=800000 -v batch=2000 -f seed.sql
--
-- `:total`  — total historical unsubscribe rows across all lists (skewed:
--             three popular lists carry the bulk, a long tail of 37 more
--             carry the rest — mirrors a multi-year SaaS with one flagship
--             weekly newsletter, a product-update list, a security-alert
--             list, and many smaller per-feature/per-team lists).
-- `:batch`  — size of the recipient list for ONE simulated
--             `Mailer::send_list_mail` call against `weekly_digest`, the
--             flagship list. 30% of the batch are addresses that really did
--             unsubscribe from `weekly_digest` (drawn from the real
--             corpus below); 70% are active subscribers who never
--             unsubscribed (fresh addresses guaranteed absent from the
--             table) — a realistic suppressed fraction for a
--             multi-year-old high-volume list.

SELECT setseed(0.7213);

TRUNCATE mail_unsubscribes RESTART IDENTITY;

INSERT INTO mail_unsubscribes (subscriber, list_id, unsubscribed_at)
SELECT
    'user' || gs || '@example.com',
    CASE
        WHEN r < 0.40 THEN 'weekly_digest'
        WHEN r < 0.60 THEN 'product_updates'
        WHEN r < 0.75 THEN 'security_alerts'
        -- Long tail: 37 smaller lists sharing the remaining 25%, roughly
        -- evenly (list_04..list_40), giving idx_scan-worthy but modest
        -- cardinality per tail list.
        ELSE 'list_' || lpad((4 + width_bucket(r, 0.75, 1.0, 37) - 1)::text, 2, '0')
    END,
    NOW() - (random() * INTERVAL '730 days')
FROM (
    SELECT gs, random() AS r
    FROM generate_series(1, :total) AS gs
) s
ON CONFLICT (subscriber, list_id) DO NOTHING;

-- Fixed recipient batch for the workload scripts. Table shape matches what
-- `send_list_mail` holds in memory before checking suppression: one
-- canonical subscriber address per recipient.
DROP TABLE IF EXISTS batch_recipients;
CREATE TABLE batch_recipients (
    ord        BIGINT PRIMARY KEY,
    subscriber TEXT NOT NULL
);

-- 30% real unsubscribes from weekly_digest specifically (deterministic:
-- lowest ids in that list, not random re-sampling, so this is stable
-- across reruns of just this script against the same :total).
INSERT INTO batch_recipients (ord, subscriber)
SELECT row_number() OVER (ORDER BY id) - 1, subscriber
FROM (
    SELECT id, subscriber
    FROM mail_unsubscribes
    WHERE list_id = 'weekly_digest'
    ORDER BY id
    LIMIT GREATEST((:batch * 3) / 10, 1)
) suppressed;

-- 70% fresh addresses in a disjoint namespace, guaranteed never present in
-- mail_unsubscribes (never unsubscribed from anything).
INSERT INTO batch_recipients (ord, subscriber)
SELECT
    (SELECT COALESCE(MAX(ord), -1) FROM batch_recipients) + row_number() OVER (ORDER BY gs),
    'active' || gs || '@example.com'
FROM generate_series(1, :batch - (SELECT COUNT(*) FROM batch_recipients)) AS gs;

VACUUM ANALYZE mail_unsubscribes;
ANALYZE batch_recipients;
