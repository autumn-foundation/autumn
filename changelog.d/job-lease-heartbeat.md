### Fixed

- **jobs:** a durable job that runs longer than the visibility timeout does
  not run again on a second worker at the same time (issue #3051). Postgres,
  Redis and `SQLite` workers renew each claim every third of the visibility
  timeout. If a renewal finds the claim gone, or no renewal succeeds for two
  thirds of the timeout, the worker stops the handler. It does not settle the
  job. Crash recovery does not change: the heartbeat stops with the process.
- **jobs:** Redis claim deadlines and the stale-claim check use the Redis
  server clock (`TIME`), not the worker clock. A worker with a skewed clock
  does not see a live claim as expired.

### Added

- **jobs:** `#[job(timeout = "30s")]` and `jobs.default_timeout_ms`
  (`AUTUMN_JOBS__DEFAULT_TIMEOUT_MS`, default `0` = no limit). A run that
  exceeds its timeout fails and retries, and the worker is free at once. See
  [Claim leases and timeouts](docs/guide/jobs.md#claim-leases-and-timeouts).
- **jobs:** `JobContext::lease_lost()`, `JobContext::is_cancelled()` and
  `JobContext::cancelled()` let work that a handler spawns find out that the
  worker stopped the run.

### Breaking Changes

- **Breaking:** `JobInfo` has a new public `timeout` field. A `JobInfo { … }`
  struct literal must add `timeout: None`
  ([migration guide](docs/migrations/next.md)).
