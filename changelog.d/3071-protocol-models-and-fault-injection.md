### Added

- **fault injection:** an opt-in `[fault_injection]` section for staging
  (issue #3071). It adds latency or errors to routes, database checkouts,
  Redis session operations and outbound HTTP calls, at a rate, on matched
  paths. It is refused in `prod` unless `allow_in_production = true`. A
  burn-rate stop condition disarms it. Each toggle writes an audit event.
  See `docs/guide/fault-injection.md`.

### Testing

- **verification:** Stateright models of job claims, scheduler tick election
  and lease locks (issue #3071), in the `autumn-protocol-models` crate. Each
  seeded bug, for example a settle with no owner fence, must give a
  counterexample. The `Protocol models` CI job runs them.
