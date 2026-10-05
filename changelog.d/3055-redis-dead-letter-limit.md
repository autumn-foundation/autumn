### Added

- **jobs:** `jobs.redis.dead_letter_limit` sets the maximum length of the Redis
  dead-letter list (issue #3055). `0` keeps all dead letters. Env:
  `AUTUMN_JOBS__REDIS__DEAD_LETTER_LIMIT`.
- **jobs:** a Redis dead-letter trim logs a `warn` event and increments
  `autumn_jobs_dead_letter_trimmed_total` on `/actuator/prometheus`.

### Changed

- **jobs:** the Redis dead-letter list keeps 10 000 entries by default. Before,
  it kept 1 000 and removed older entries without a log or a metric. Each
  entry uses Redis memory for a list entry and a per-id record, so the new
  default can use up to 10 times more memory.
