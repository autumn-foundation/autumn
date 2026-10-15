### Fixed

- **idempotency:** the replay alias key is now locked before the session is
  saved (issue #2455). A request with the same unrotated cookie can arrive
  while the first response commits. It now gets `409 Conflict`. The handler
  does not run again.
