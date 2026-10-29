### Fixed

- **billing:** two checkout POSTs for one user no longer create two provider
  sessions (issue #3091). While one checkout runs, a second POST for the same
  user gets `409`. The guard is per process. A retry after the first finishes
  works.
