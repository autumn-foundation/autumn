### Fixed

- `DbSuppressionStore`'s `mail_unsubscribes` table declaration is now forked
  per backend like the other DDL-emitting stores: `unsubscribed_at` is
  `Timestamptz` on Postgres and `TimestamptzSqlite` on SQLite, matching the
  `TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP` DDL `autumn generate mailer
  --list-unsubscribe` emits for SQLite apps. Previously the single
  Postgres-only declaration compiled only because no query touched the
  column; any select of it broke the `sqlite` build (issue #2697).
