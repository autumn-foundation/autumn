### Breaking Changes

- **Breaking:** the Postgres scheduler needs a new table ([migration guide](docs/migrations/next.md)).
  `scheduler.backend = "postgres"` claims ticks in `autumn_scheduler_ticks`
  (issue #3052). The runtime creates it on first use. If the app's database
  role cannot run `CREATE TABLE`, apply `scheduler::PG_TICK_TABLE_DDL` before
  you deploy.

### Fixed

- **scheduler:** the Postgres scheduler runs each fleet tick at most once
  (issue #3052). It freed its session advisory lock when the leader finished.
  A replica whose timer reached the same tick later ran it again. Now each
  tick is a row in `autumn_scheduler_ticks`, inserted with
  `ON CONFLICT DO NOTHING`. The row stays for `scheduler.lease_ttl_secs`, and a
  fixed-delay row stays for its delay plus `scheduler.lease_ttl_secs`. A leader that crashes
  mid-tick does not free its tick. The coordinator holds no connection while a
  tick runs, so it works behind a transaction-mode PgBouncer.
- **scheduler:** the `sqlite` backend keeps a fixed-delay tick claimed for its
  delay plus `scheduler.lease_ttl_secs`, not only for the TTL.
- **acme:** on a distributed scheduler backend, certificate renewal and
  tenant-domain issuance hold their leader key for the whole order (up to two
  hours if the leader crashes), then free it. Before, a second replica could
  order the same certificate.

### Added

- **scheduler:** `scheduler::current_tick()` gives a running `#[scheduled]`
  task its tick key. On the Postgres backend it also gives a fencing token
  (the tick's `generation`).
- **scheduler:** `SchedulerCoordinator::try_acquire_for_period`. It has a
  default, so a custom coordinator needs no change.
- **scheduler:** `SchedulerLease::release_and_free` frees a constant key that
  works as a mutex.
- **scheduler:** a boot warning when `scheduler.backend = "in_process"` runs
  fleet tasks and a hint shows more than one replica: `jobs.backend` is
  `postgres` or `redis`, `AUTUMN_REPLICAS` is more than 1, or
  `KUBERNETES_SERVICE_HOST` is set.

### Deprecated

- **scheduler:** `PostgresAdvisorySchedulerCoordinator` is now an alias of
  `PostgresTickSchedulerCoordinator`. It does not use advisory locks.
