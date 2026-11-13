-- Index sets. Parameter :variant selects one.
--   shipped      : the older generator output, (tenant_id, recipient_id).
--   feed         : (tenant_id, recipient_id, id) in place of (tenant_id, recipient_id). Current generator output.
--   feed_created : `feed` plus the opt-in index for `sort=created_at`.
--   wide         : the #3616 candidate. `shipped` plus (tenant_id, recipient_id, created_at DESC, id DESC).
\set ON_ERROR_STOP on
DROP INDEX IF EXISTS idx_notifications_tenant_recipient, idx_notifications_feed,
  idx_notifications_unread, idx_notifications_created;
SELECT :'variant' IN ('shipped', 'wide') AS old_idx, :'variant' IN ('feed', 'feed_created') AS feed_idx,
       :'variant' IN ('feed_created', 'wide') AS created_idx \gset
\if :old_idx
CREATE INDEX idx_notifications_tenant_recipient ON notifications (tenant_id, recipient_id);
\endif
\if :feed_idx
CREATE INDEX idx_notifications_feed ON notifications (tenant_id, recipient_id, id);
\endif
\if :created_idx
CREATE INDEX idx_notifications_created ON notifications (tenant_id, recipient_id, created_at DESC, id DESC);
\endif
CREATE INDEX idx_notifications_unread ON notifications (tenant_id, recipient_id, id) WHERE read_at IS NULL;
VACUUM (ANALYZE) notifications;
