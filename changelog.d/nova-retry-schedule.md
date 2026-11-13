### Added

- **backoff:** [no-plugin] `RetrySchedule`, a bounded full-jitter retry plan. It yields
  delays until `max_retries` is spent, and `next_delay(Some(remaining))`
  returns `None` when the next sleep would run past a request deadline.
