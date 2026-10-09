### Fixed

- **jobs:** `in_flight` now counts each claim once (issue #3166). Before, a job
  that another replica enqueued had no admin record here. Stale recovery, then
  a lease loss or lost ack, balanced the claim twice. `/actuator/jobs` then
  showed too few running jobs. Recovery of a claim that another process
  started now leaves `in_flight` alone. This holds on `postgres`, `sqlite` and
  `redis`.
