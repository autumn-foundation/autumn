### Added

- **cost:** per-request cost accounting and a live cost signal (issue #1720).
  `[cost] enabled = true` meters the CPU time, allocated bytes (with an
  `AllocationProbe`) and DB queries of each request, by tenant. The cost goes
  to the `Server-Timing` header (`cost-cpu`, `cost-db`, `cost-alloc`), the
  `autumn.cost` metrics source and `GET /actuator/cost`. `CostSignal` holds a
  carbon or price value, set from code or from the `autumn_cost_signal`
  runtime-config key with no redeploy. `#[job(deferrable)]` and
  `#[scheduled(..., deferrable)]` work waits while the signal is above
  `[cost] defer_threshold`, then runs; it is never dropped. See
  [the cost guide](docs/guide/cost.md).
