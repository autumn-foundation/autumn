### Fixed

- **db:** `DatabaseFleet` operations are cancellation-safe (issue #3229).
  A dropped `delete`, `restore`, `close`, first `open` or `provision` no
  longer strands a key on a drain, leaves `on_open` hooks untracked, or loses
  a creation. A retried `provision` reports a creation the first call did not.
