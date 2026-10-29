### Performance

- **outbox:** on SQLite, the relay's claim no longer reads every sent row that
  the purge has not deleted yet (issue #3203). The SQLite schema now has the
  partial index `idx_autumn_outbox_claim`, which Postgres has had since 0.9.0.
  `outbox::ensure_schema` adds the index to existing tables at the next boot.
  On a large table, the build holds the SQLite write lock one time.
