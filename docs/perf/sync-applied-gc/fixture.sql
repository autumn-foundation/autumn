-- Production-shaped autumn_sync_applied fixture (seeded, deterministic).
-- :n rows spread over 31 days, i.e. a 30-day dedup retention window plus one
-- day of expired rows at steady state. 200 scopes, Zipf-ish skew, ~3% rows
-- carry a Resolved snapshot. applied_at follows insert order with +/-10 min
-- jitter (pushes from many devices interleave), so physical order tracks time.
SELECT setseed(0.42);
DROP TABLE IF EXISTS autumn_sync_applied;
CREATE TABLE autumn_sync_applied (
    scope TEXT NOT NULL,
    device_id TEXT NOT NULL,
    change_id TEXT NOT NULL,
    version BIGINT NOT NULL DEFAULT 0,
    applied_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    resolved_row JSONB,
    PRIMARY KEY (scope, device_id, change_id)
) WITH (autovacuum_enabled = false);
INSERT INTO autumn_sync_applied
SELECT 'tenant-' || (floor(200 * power(random(), 2.5)))::int,
       'dev-' || (i % 5000),
       'chg-' || i,
       i,
       timestamptz '2026-11-01 00:00:00+00'
         - interval '31 days'
         + (i::double precision / :n) * interval '31 days'
         + (random() - 0.5) * interval '20 minutes',
       CASE WHEN i % 33 = 0 THEN jsonb_build_object('title', 'row ' || i, 'body', repeat('x', 200)) END
FROM generate_series(1, :n) i;
\if :{?variant_ddl}
-- Built after the load (a BRIN built before it has no summaries until vacuum).
:variant_ddl
\endif
VACUUM (ANALYZE) autumn_sync_applied;
