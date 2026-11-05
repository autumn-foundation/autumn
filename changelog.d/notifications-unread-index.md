### Changed

- **notifications:** [no-plugin] `autumn generate notifications` also creates a partial
  index `idx_notifications_unread (tenant_id, recipient_id) WHERE read_at IS NULL`.
  The unread badge count of a recipient with 168k notifications (2.5k unread)
  drops from 39,161 to 6 buffers. Existing apps can add the same index in a
  migration; the framework store's queries are unchanged.
