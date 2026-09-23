### Fixed

- **jobs:** a `SQLite` app could fail its first enqueue with "ON CONFLICT
  clause does not match any PRIMARY KEY or UNIQUE constraint" on a correct
  database. A pooled connection opened before the job queue created its
  schema holds a cached schema that predates the partial unique index the
  enqueue upsert targets, and `SQLite` does not reload it. The queue now drops
  every idle connection once, when it creates its schema, so the pool serves
  only connections that can resolve the target.
