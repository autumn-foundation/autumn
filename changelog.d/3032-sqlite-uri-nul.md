### Fixed

- **db:** a `%00` in a SQLite URI name or value now ends that name or value,
  as SQLite does. `file:a?cache=shared%00x` is now shared-cache (issue #3032).
