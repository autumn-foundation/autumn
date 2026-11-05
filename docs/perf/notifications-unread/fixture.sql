-- Seeded, deterministic fixture. Scaffold schema from autumn-cli generate notifications.
DROP TABLE IF EXISTS notifications;
CREATE TABLE notifications (
 id BIGSERIAL PRIMARY KEY, tenant_id TEXT NOT NULL DEFAULT '', recipient_id BIGINT NOT NULL,
 kind TEXT NOT NULL, payload TEXT NOT NULL, read_at TIMESTAMPTZ NULL, created_at TIMESTAMPTZ NOT NULL DEFAULT NOW());
CREATE INDEX idx_notifications_tenant_recipient ON notifications (tenant_id, recipient_id);
SELECT setseed(0.42);
-- 2,000,000 rows; recipient skew: id = 1 + floor(20000 * r^4) (few heavy recipients, long tail).
-- ~1.5% unread, concentrated in recent rows (older rows are read).
INSERT INTO notifications (tenant_id, recipient_id, kind, payload, read_at, created_at)
SELECT '', 1 + floor(20000 * power(random(),4))::bigint,
       (ARRAY['like','comment','follow','mention'])[1+floor(random()*4)::int],
       repeat('x', 80), 
       CASE WHEN random() < 0.015 THEN NULL ELSE now() - interval '1 day' END,
       now() - (g || ' seconds')::interval
FROM generate_series(1, 2000000) g;
VACUUM ANALYZE notifications;
SELECT recipient_id, count(*) total, count(*) FILTER (WHERE read_at IS NULL) unread
FROM notifications GROUP BY 1 ORDER BY 2 DESC LIMIT 1;
