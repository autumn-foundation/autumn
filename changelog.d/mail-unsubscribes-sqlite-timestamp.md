### Fixed

- **mail:** `DbSuppressionStore` now reads `unsubscribed_at` as
  `TimestamptzSqlite` when the runtime is SQLite. This holds when the
  `postgres` feature is also on (issue #2697).
