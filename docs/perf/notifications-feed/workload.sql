-- The reads the store issues for one feed view, as diesel emits them.
-- Columns: label|sql. %1$s is the recipient id. run.sh runs each 20 times per recipient.
unread count|SELECT COUNT(*) FROM notifications WHERE tenant_id = '' AND recipient_id = %1$s AND read_at IS NULL
total count|SELECT COUNT(*) FROM notifications WHERE tenant_id = '' AND recipient_id = %1$s
page id desc|SELECT id, recipient_id, kind, payload, read_at, created_at FROM notifications WHERE tenant_id = '' AND recipient_id = %1$s ORDER BY id DESC LIMIT 20 OFFSET 0
page 5 id desc|SELECT id, recipient_id, kind, payload, read_at, created_at FROM notifications WHERE tenant_id = '' AND recipient_id = %1$s ORDER BY id DESC LIMIT 20 OFFSET 80
unread page|SELECT id, recipient_id, kind, payload, read_at, created_at FROM notifications WHERE tenant_id = '' AND recipient_id = %1$s AND read_at IS NULL ORDER BY id DESC LIMIT 20 OFFSET 0
kind page|SELECT id, recipient_id, kind, payload, read_at, created_at FROM notifications WHERE tenant_id = '' AND recipient_id = %1$s AND kind = 'kind3' ORDER BY id DESC LIMIT 20 OFFSET 0
page created_at desc|SELECT id, recipient_id, kind, payload, read_at, created_at FROM notifications WHERE tenant_id = '' AND recipient_id = %1$s ORDER BY created_at DESC, id DESC LIMIT 20 OFFSET 0
