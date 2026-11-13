-- Skewed notifications fixture: 2,000,000 rows, 50,000 recipients.
-- Recipient 1 holds about 49,000 rows, recipient 58 about 1,100, recipient 6373 about 40.
-- Schema = the table `autumn generate notifications` emits (Postgres). Indexes come from indexes.sql.
DROP TABLE IF EXISTS notifications;
SELECT setseed(0.42);
CREATE TABLE notifications (
  id BIGSERIAL PRIMARY KEY, tenant_id TEXT NOT NULL DEFAULT '', recipient_id BIGINT NOT NULL,
  kind TEXT NOT NULL, payload TEXT NOT NULL, read_at TIMESTAMPTZ NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT NOW());
INSERT INTO notifications (tenant_id, recipient_id, kind, payload, read_at, created_at)
SELECT CASE WHEN random() < 0.9 THEN '' ELSE 't' || (random() * 9)::int END,
       1 + floor(50000 * power(random(), 3))::bigint, 'kind' || (random() * 5)::int,
       repeat('x', 40 + (random() * 80)::int),
       CASE WHEN random() < 0.15 THEN NULL ELSE now() - make_interval(secs => random() * 100 * 86400) END,
       now() - make_interval(secs => random() * 365 * 86400)
FROM generate_series(1, 2000000);
