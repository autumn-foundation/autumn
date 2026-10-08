### Added

- **fault injection:** an opt-in `[fault_injection]` section for staging
  (issue #3071). It adds latency or errors to routes, database checkouts,
  Redis session operations and outbound HTTP calls. Each fault has a rate and
  a list of paths. It is refused in `prod` unless `allow_in_production =
  true`. A burn-rate stop condition disarms it. Each arm or disarm writes an
  audit event. See `docs/guide/fault-injection.md`.

### Fixed

- **scheduler:** a cron task that waits in the cost gate longer than its
  window no longer claims that occurrence (issue #3071). Before, the claim
  could succeed after the tick row expired, so the occurrence ran twice. The
  tick-election protocol model found this bug.

### Breaking Changes

- **Breaking:** `AutumnConfig` gains a public `fault_injection` field. A
  struct literal needs `..AutumnConfig::default()`
  ([migration guide](docs/migrations/next.md)).
