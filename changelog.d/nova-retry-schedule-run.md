### Added

- **backoff:** [no-plugin] `RetrySchedule::run`, an async retry loop. It calls your
  operation, sleeps the next jittered delay on a retryable error, and returns the
  last error when retries run out or the next sleep would pass the deadline
  (the task's current `Deadline` by default).
