### Breaking Changes

- **Breaking:** `http_client::ClientError` is `#[non_exhaustive]` and gains a
  `SimNetwork` variant for faults from the simulated network (issue #2967). A
  `match` needs a `_` arm ([migration guide](docs/migrations/next.md)).
- **Breaking:** `sim::SimClock` and `sim::SimApp` are no longer public. No
  public API returned them (issue #2967,
  [migration guide](docs/migrations/next.md)).

### Added

- **sim-testing:** `sim::SimNet`, a simulated network for outbound
  `http_client::Client` calls (issue #2967). It serves in-process hosts, adds
  seeded latency and drops, partitions hosts on demand, and records each
  attempt. No call reaches the real network. Install it with `Sim::net`.
- **sim-testing:** an interleaving shuffler. `Sim::interleave` polls futures in
  a seeded order, and `Sim::spawn` spawns a task with seeded yields, so a
  sweep explores task-poll interleavings too (issue #2967).
- **sim-testing:** `sim::crash_at` drops an operation at any await, and returns
  a `CrashOutcome`. `CrashPoint::await_index` is now the seeded index to pass
  it (issue #2967).
- **sim-testing:** a `sim_ops` cargo-fuzz target. It drives the same `Op`
  vocabulary (`sim::scenario`) as the `sim-sweep` bin (issue #2967).
- **time:** `ambient_now`, `ambient_monotonic`, `ambient_instant`,
  `ambient_system_time` and `AmbientClock`. They read the running `Sim`'s
  virtual clock on the current thread, and the system clock otherwise.
  Framework code with no clock in scope now reads time through them, so a
  `Sim` controls it (issue #2967).
- **sim-testing:** `Sim::try_run_to_idle` returns a `SimStall` when the drain
  does not settle (issue #2967).

### Changed

- **sim-testing:** `Sim::run_to_idle` panics with the seed when work still
  runs at the end of the drain, for example a job that enqueues itself again.
  Before, it stopped silently (issue #2967,
  [migration guide](docs/migrations/next.md)).
- **sim-testing:** inside a `Sim`, framework code with no clock in scope
  (about 55 modules, and `#[repository]` soft-delete stamps) reads the sim's
  virtual clock. Outside a `Sim`, nothing changes. The `SignedWebhook`
  timestamp check and the outbound webhook `t=` timestamp read the app clock
  (issue #2967, [migration guide](docs/migrations/next.md)).
- **ci:** the single-threaded `sim_` step arms the liveness watchdog
  (`AUTUMN_SIM_LIVENESS_BUDGET_SECS`) (issue #2967).
