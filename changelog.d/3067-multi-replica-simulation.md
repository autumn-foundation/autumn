### Added

- **sim:** several replicas on one sim clock (issue #3067).
  `Sim::mount_replica(name, app)` mounts named apps next to each other, each
  with its own state, job runtime and scheduled tasks. `kill_replica` stops a
  replica's tasks as a crash does, and `restart_replica` mounts it again.
  With `Sim::net`, each replica is a `SimNet` host under its name.
- **sim:** a clock per replica. `Replica::clock_ahead`, `clock_behind`,
  `clock_drift_ppm` and `seeded_clock` set the offset and drift.
  `Sim::step_replica_clock` jumps one clock, as an NTP step does.
- **sim:** database faults per replica. `Sim::db_link(name)` and
  `SqliteSubstrate::replica_pool` give each replica its own session on the
  shared database. The link drops the session, fails a write part way
  (mid-query error), or applies a write and then returns an error (commit
  ambiguity).
- **sim:** `Sim::run_for` moves time one event at a time. It does not move
  time while a query runs. So a seed replays the same `SQLite` work across
  replicas. `sim::runtime()` builds the runtime it needs, for a sim outside
  `#[sim_test]`.
- **sim:** `sim::trace::capture` records the framework's `tracing` events with
  sim time, and `Trace::diff` compares two runs of one seed.
- **sim-sweep:** with `--features sqlite`, the seed sweep also runs the jobs,
  scheduler and lock scenarios across two or three replicas under seeded
  faults, and runs the first seeds twice to compare their traces. Select
  scenarios with `AUTUMN_SIM_SCENARIOS`.

### Changed

- **sim:** `#[sim_test]` runs on `sim::runtime()`: one blocking thread, and
  each query of a sim-substrate connection runs only while all tasks wait. A
  seed then replays the same order of database work. A blocking task that
  waits for a second blocking task now never ends in a sim test.
- **jobs, scheduler, lock:** the background loops and the job claim
  heartbeat use `biased;` in `tokio::select!`, with shutdown first, so a sim
  replays their branch order.
- **lock:** the `SQLite` `LockGuard` stops its renewal task by a signal, not
  by an abort, so a release does not drop a renewal query in flight.

### Fixed

- **jobs:** the job claim heartbeat no longer drops a renewal in flight when
  it gives up or stops. On a current-thread runtime, diesel-async panicked on
  that drop with `SQLite`, before the heartbeat stopped the run. The run then
  went on without its claim, and a peer ran the job again. The #3067 sweep
  found this.
- **jobs:** `SQLite` stale-claim recovery draws its per-row jitter from the
  app's entropy, not from `SQLite`'s `random()`, so a sim replays it.

### Testing

- **sim:** two-replica regression tests for the cron double-fire (#3052) and
  the long-job double-run (#3051).

### Documentation

- **scheduler:** the `SQLite` lease coordinator reaps leases with each
  process's own clock. Keep the clock difference between processes below
  `scheduler.lease_ttl_secs`. Processes on one host use one clock.
