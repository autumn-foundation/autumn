### Breaking Changes

- **Breaking:** **jobs:** `JobTrackingStore::reset_for_retry` takes a
  `RetryToken`, not a `DateTime<Utc>` (issue #2581,
  [migration guide](docs/migrations/next.md)). `JobTrackingStore` has a new
  required method, `get_with_token`. The token is a write counter. It changes
  on every write, also under a frozen clock or inside one millisecond. A
  retry no longer overwrites a job that a worker has already started. The
  Postgres store needs the new `autumn_job_tracking.version` column: the
  framework migration adds it.
