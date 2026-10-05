### Fixed

- **scheduler:** the Postgres scheduler runs each fleet tick at most once
  (issue #3052). It used a session advisory lock and freed it when the leader
  finished, so a replica whose timer reached the same tick later ran it again.
  Each tick is now a row in `autumn_scheduler_ticks`, inserted with
  `ON CONFLICT DO NOTHING`. The row stays for `scheduler.lease_ttl_secs`. A
  leader that crashes mid-tick does not free its tick. The coordinator holds no
  connection while a tick runs, and it works behind a transaction-mode
  PgBouncer.

### Added

- **scheduler:** `scheduler::current_tick()` gives a running `#[scheduled]`
  task its tick key and, on the Postgres backend, a fencing token (the tick's
  `generation`).
- **scheduler:** a boot warning when `scheduler.backend = "in_process"` runs
  fleet tasks and a hint shows more than one replica: `jobs.backend` is
  `postgres` or `redis`, `AUTUMN_REPLICAS` is more than 1, or
  `KUBERNETES_SERVICE_HOST` is set.

### Deprecated

- **scheduler:** `PostgresAdvisorySchedulerCoordinator` is now an alias of
  `PostgresTickSchedulerCoordinator`. It no longer uses advisory locks.
