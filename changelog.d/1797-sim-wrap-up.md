### Added

- **sim-testing:** `#[scheduled]` tasks run under `Sim` (issue #1797).
  `TestApp::tasks(tasks![..])` registers them, a plugin's tasks now reach a
  `TestApp` too, and `TestApp::build` starts them on the in-process scheduler.
  A tick fires when `Sim::advance` crosses its deadline, with no real sleep.
  Before this, `TestApp` dropped every task, although the simulation-testing
  guide said that ticks fire. `TestApp::jobs(jobs![..])`, which the guide also
  used, now exists as well.
- **sim-testing:** an opt-in liveness watchdog for `#[sim_test]`. Set
  `AUTUMN_SIM_LIVENESS_BUDGET_SECS` and a test whose tasks all park panics
  with its seed and replay line instead of hanging. Arm it only where sim tests
  run one at a time: the paused clock also advances while a test waits on
  another thread.
- **sim-testing:** the `sim-sweep` bin reads `AUTUMN_SIM_SEED_START`, and its
  replay command now reruns only the failing seed instead of every seed up to
  it.

### Changed

- **sim-testing:** `Sim::build` seeds the mounted app's entropy from
  `sim.seed`, so job ids, request ids and retry jitter replay from
  `AUTUMN_SIM_SEED` with no `with_entropy` call. Before, they came from the OS
  unless the test passed `with_entropy(SeededEntropy::new(sim.seed))`, which
  still works and now changes nothing. An explicit `with_entropy` still wins,
  and an app remounted by `Sim::restart` draws a new stream derived from the
  seed.
