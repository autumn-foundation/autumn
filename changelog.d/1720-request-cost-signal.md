### Added

- **cost:** per-request cost records and a live cost signal (issue #1720).
  `[cost] enabled = true` measures the CPU time, allocated bytes (with an
  `AllocationProbe`) and DB queries of each request, by tenant. The cost goes
  to the `Server-Timing` header (`cost-cpu`, `cost-db`, `cost-alloc`), the
  `autumn.cost` metrics source and `GET /actuator/cost`. `CostSignal` holds a
  carbon or price value. Set it from code, or from the `autumn_cost_signal`
  runtime-config key with no redeploy. `#[job(deferrable)]` and
  `#[scheduled(..., deferrable)]` work waits while the signal is above
  `[cost] defer_threshold`. The work runs when the signal falls. The runtime
  never drops it. Only the `local` jobs backend defers jobs. See
  [the cost guide](docs/guide/cost.md).

### Breaking Changes

- **Breaking:** `AutumnConfig` gains a public `cost` field. A struct literal
  needs `..AutumnConfig::default()`
  ([migration guide](docs/migrations/next.md)).
