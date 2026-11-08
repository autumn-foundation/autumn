### Changed

- **notifications:** `autumn generate notifications` adds a partial index over
  unread rows on Postgres (`idx_notifications_unread`, on
  `(tenant_id, recipient_id, id) WHERE read_at IS NULL`). For a recipient with
  200,000 read notifications, the unread badge and the unread feed page go from
  thousands of buffers per call to a few dozen. Existing apps can add the same
  index in a migration; see `docs/perf/notifications-unread/`. [no-plugin]
