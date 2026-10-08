### Fixed

- **jobs:** `in_flight` counts each claim once (issue #3166). A job that
  another replica enqueued has no admin record in this process. Before, stale
  recovery here and then a lease loss or lost ack in the old worker balanced
  that claim twice, and hid other runs of the same job type from
  `/actuator/jobs`. Recovery of a claim that another process started now leaves
  `in_flight` alone. The fix holds on `postgres`, `sqlite` and `redis`.
