### Fixed

- **mail:** `DbSuppressionStore` now declares `unsubscribed_at` as
  `TimestamptzSqlite` when the runtime is SQLite, also when `postgres` is on
  (issue #2697). Tests run the generated SQLite DDL.
