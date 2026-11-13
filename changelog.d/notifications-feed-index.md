### Performance

- **notifications:** on Postgres, `autumn generate notifications` emits
  `idx_notifications_feed` on `(tenant_id, recipient_id, id)` in place of
  `idx_notifications_tenant_recipient`. This index is in the default feed
  order. The first page for a recipient with 1,092 rows reads 23 buffers,
  not 1,083. Inserts write about 30% more WAL (70% just after a checkpoint).
  Existing apps keep their old indexes. The
  [notifications guide](docs/guide/notifications.md#indexes) gives the
  migrations for the feed and unread indexes, and an opt-in index for
  `sort=created_at`.
