### Fixed

- **db:** `DatabaseFleet` operations are cancellation-safe (issue #3229).
  A dropped `delete`, `restore`, `close`, first `open` or `provision` now
  finishes its work. It no longer strands a key on a drain. It no longer
  leaves `on_open` hooks untracked. It no longer loses a creation: a retried
  `provision` returns `Ok` when the first call never reported the creation.
