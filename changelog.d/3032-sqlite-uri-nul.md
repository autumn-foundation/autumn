### Fixed

- **db:** SQLite now treats `%00` in a URI as the end of that name or value.
  `file:a?cache=shared%00x` is now found as shared-cache, so the shared-cache
  boot warning shows (issue #3032).
