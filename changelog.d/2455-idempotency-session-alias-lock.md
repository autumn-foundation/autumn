### Fixed

- **idempotency:** lock the session replay alias before the session is saved
  (issue #2455). A request with the same unrotated cookie, sent while the
  first response commits, now gets `409` and does not run the handler again.
