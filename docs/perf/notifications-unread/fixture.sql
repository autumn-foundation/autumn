-- Production-shaped notifications fixture. Parameter :hist = rows for the heavy recipient.
-- Schema = the DDL `autumn generate notifications` emits (Postgres), incl. its shipped index.
DROP TABLE IF EXISTS notifications;
CREATE TABLE notifications (
  id BIGSERIAL PRIMARY KEY, tenant_id TEXT NOT NULL DEFAULT '', recipient_id BIGINT NOT NULL,
  kind TEXT NOT NULL, payload TEXT NOT NULL, read_at TIMESTAMPTZ,
  created_at TIMESTAMPTZ NOT NULL DEFAULT NOW());
CREATE INDEX idx_notifications_tenant_recipient ON notifications (tenant_id, recipient_id);
SELECT setseed(0.42);
-- 20,000 ordinary recipients, 25 rows each, ~92% read (typical: user opens the bell regularly)
INSERT INTO notifications (recipient_id, kind, payload, read_at, created_at)
SELECT (g % 20000) + 1000, (ARRAY['comment.created','mention','follow','like'])[1 + (g % 4)],
       repeat('x', 120), CASE WHEN random() < 0.92 THEN now() END, now() - (g || ' seconds')::interval
FROM generate_series(1, 500000) g;
-- Heavy recipient (id 1): :hist rows, 99.9% read, 20 unread, interleaved by id
INSERT INTO notifications (recipient_id, kind, payload, read_at, created_at)
SELECT 1, 'comment.created', repeat('x', 120),
       CASE WHEN g % (:hist / 20) = 0 THEN NULL ELSE now() END, now() - (g || ' seconds')::interval
FROM generate_series(1, :hist) g;
VACUUM (ANALYZE) notifications;
