### Added

- **cost:** deferrable jobs wait on every jobs backend (issue #1720). On
  `postgres` and `sqlite`, workers do not claim a `#[job(deferrable)]` job
  while the cost signal is high. On `redis`, a worker parks it until the signal
  falls. The job stays enqueued and uses no attempt. Autumn no longer
  logs the boot warning for these backends.
- **cost:** with `[cost] enabled`, Autumn meters each job run and scheduled
  tick (CPU time, allocated bytes, DB queries). The cost of a `local` job goes
  to the tenant that enqueued it. Autumn classifies each deferrable run as
  `shifted` or `in_window`, and shows the shifted ratio. See `jobs` and
  `tasks` in `GET /actuator/cost`, the `autumn_cost_work_*_total` and
  `autumn_cost_deferrable_*_total` metrics, and
  [the cost guide](docs/guide/cost.md).
- **cost:** `CostSignal::set_recheck` sets the time between two signal checks.
