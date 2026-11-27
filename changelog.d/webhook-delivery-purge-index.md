### Performance

- **webhooks:** `SqlOutboundWebhookStore::purge_deliveries_before` reads only
  the expired delivered logs through a new partial index,
  `idx_autumn_webhook_deliveries_delivered`, instead of the whole delivery log.
  An hourly purge of a 30-day log reads 98.6% fewer buffers (126,849 → 1,731
  at 1M logs). `ensure_schema` builds the index with `CREATE INDEX
  CONCURRENTLY` in the background on existing tables. Each delivered log adds
  one index entry (about 4% more WAL per delivery-log write).
