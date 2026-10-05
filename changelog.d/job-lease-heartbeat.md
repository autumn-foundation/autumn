### Fixed

- **jobs:** a durable job that runs longer than the visibility timeout no
  longer runs a second time, at the same time, on another worker (issue
  #3051). Postgres, Redis and `SQLite` workers now renew each claim every third
  of the visibility timeout. If a renewal finds the claim gone, the worker
  stops the handler and does not settle the job. Crash recovery is unchanged:
  the heartbeat stops with the process.
- **jobs:** Redis claim deadlines and the stale-claim check now use the Redis
  server clock (`TIME`), not the worker clock. A worker with a skewed clock
  no longer sees a live claim as expired.

### Added

- **jobs:** `#[job(timeout = "30s")]` and `jobs.default_timeout_ms`
  (`AUTUMN_JOBS__DEFAULT_TIMEOUT_MS`, default `0` = no limit). A run that
  exceeds its timeout fails and retries, and frees the worker at once. See
  [Claim leases and timeouts](docs/guide/jobs.md#claim-leases-and-timeouts).
- **jobs:** `JobContext::lease_lost()`, `JobContext::is_cancelled()` and
  `JobContext::cancelled()` tell work that a handler spawned when the worker
  stopped the run.

### Breaking Changes

- **Breaking:** `JobInfo` has a new public `timeout` field. A `JobInfo { … }`
  struct literal must add `timeout: None`
  ([migration guide](docs/migrations/next.md)).
