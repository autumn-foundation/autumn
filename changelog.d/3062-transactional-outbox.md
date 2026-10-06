### Added

- **outbox:** a transactional outbox and inbox (issue #3062). `Outbox::write`
  puts a message in the `autumn_outbox` table on the connection of your
  transaction, so it commits or rolls back with your data. A relay sends it
  after commit, in order per aggregate, with retries, jitter and dead letters.
  `Inbox::seen` drops a copy for an at-least-once consumer. Postgres and
  SQLite. Turn it on with `outbox.enabled = true`
  (`AUTUMN_OUTBOX__ENABLED`); register handlers with
  `AppBuilder::outbox_handler`. See `docs/guide/outbox.md`.
- **outbox:** framework writers on the same table: `Outbox::publish` and
  `Events::publish_in_tx` (durable event listeners), `Outbox::enqueue_job`
  (Redis and SQLite job backends), `Outbox::deliver_mail`, and
  `Outbox::dispatch_webhook` / `WebhookOutboundManager::dispatch_in_tx`.
- **mail:** with `outbox.enabled = true`, `deliver_later` writes to the outbox
  (`OutboxMailQueue`) when the app sets no `MailDeliveryQueue`, so a queued
  mail survives a restart.
- **webhooks:** each outbound delivery sends the Standard Webhooks headers
  `webhook-id`, `webhook-timestamp` and `webhook-signature`. `webhook-id` is
  the same on every retry, so a receiver can drop a copy, and the signature
  covers it.
- **webhooks:** `SqlOutboundWebhookStore` and `OutboundWebhookPlugin::sql()`, a
  durable subscription and delivery-log store on the app database
  (`WEBHOOK_SCHEMA_SQL`).
- **testing:** `TestApp::with_outbox` and `TestApp::outbox_handler`.
  `Sim::run_to_idle` drains the outbox.

### Fixed

- **webhooks:** a second delivery job for a log that already got a `2xx`
  response no longer sends the webhook again. `log_delivery` ignores a write
  to a `2xx` log and a repeated or stale (older attempt) failure
  (`webhook_outbound::log_delivery_ignores`), so a late duplicate job cannot
  overwrite a success or count one failure twice.
