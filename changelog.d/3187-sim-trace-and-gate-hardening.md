### Fixed

- **sim:** `sim::trace::capture` now keeps captures apart when they run
  together on one thread, for example with `tokio::join!`. Each trace has only
  its own events.
- **sim:** `sim::runtime()` now returns `SimRuntime`. Its `block_on` installs
  the gate before the first poll, so a runtime built on one thread and run on
  another still gates the first query. `SimRuntime` derefs to `Runtime`.
