### Changed

- **outbox:** the relay's claim no longer reads every sent row that the purge
  has not deleted yet. A new partial index `idx_autumn_outbox_claim`
  (`autumn_outbox (seq) WHERE dispatched_at IS NULL AND dead_at IS NULL`)
  serves the `ORDER BY seq` of the claim: 126,813 buffers per three claims
  fall to 7,499 with 950,000 sent rows in retention, and the cost no longer
  grows with them. New installs get the index from the schema. On an
  existing install, `outbox::ensure_schema` builds it in a background task with
  `CREATE INDEX CONCURRENTLY` at boot, and only logs a warning when the role
  cannot create indexes or the pool has a single connection. The index adds
  about 66 bytes of WAL to each
  enqueued message. [no-plugin]
