\set QUIET on
SELECT COUNT(*) FROM notifications WHERE tenant_id = '' AND recipient_id = :rid AND read_at IS NULL \gset
SELECT id FROM notifications WHERE tenant_id = '' AND recipient_id = :rid AND read_at IS NULL ORDER BY created_at DESC, id DESC LIMIT 20 \gset
