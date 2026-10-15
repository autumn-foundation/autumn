### Fixed

- **experiments:** `PgExperimentStore` no longer connects to Postgres on the
  Tokio worker (issue #3262). `get` reads an in-memory snapshot that a
  background refresh updates. A failed refresh keeps the last-known configs.
  `AppBuilder::with_experiment_store` preloads the snapshot at startup.
  `PgConfigStore` keeps its cache: only admin routes read it.

### Added

- **experiments:** `Experiments::assign_async` and
  `ExperimentService::assign_async` run an assignment on the blocking pool.
  Use them in async handlers. `ExperimentStore::preload` is a new method with a
  default that does nothing.
