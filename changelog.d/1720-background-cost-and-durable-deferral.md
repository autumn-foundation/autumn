### Added

- **cost:** deferrable jobs wait on every jobs backend (issue #1720). On
  `postgres` and `sqlite`, workers do not claim a `#[job(deferrable)]` job
  while the cost signal is high. On `redis`, a worker parks it until the next
  signal check. The job stays enqueued and uses no attempt. The boot warning
  for these backends is removed.
- **cost:** with `[cost] enabled`, each job run and scheduled tick is metered
  (CPU time, allocated bytes, DB queries). A `local` job goes to the tenant
  that enqueued it. Deferrable runs are classed `shifted` or `in_window`, with
  a shifted ratio. See `jobs`, `tasks` in `GET /actuator/cost`, the
  `autumn_cost_work_*_total` and `autumn_cost_deferrable_*_total` metrics, and
  [the cost guide](docs/guide/cost.md).
- **cost:** `CostSignal::set_recheck` sets the time between two signal checks.
