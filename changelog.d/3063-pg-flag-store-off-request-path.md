### Breaking Changes

- **Breaking:** before its first load, `PgFlagStore::get` on a Tokio runtime
  returns an error ([migration guide](docs/migrations/next.md)). It starts a
  load in the background. Call `PgFlagStore::refresh` first when you read
  flags before the app starts. Apps that use `with_flag_store` need no change.

### Fixed

- **feature_flags:** `PgFlagStore` reads flags from an in-memory snapshot
  (issue #3063). A flag read does not connect to the database on a Tokio
  worker. A stale snapshot refreshes on the blocking pool. Loads run one at a
  time, in order. Each store connection has a 5 s statement timeout, and a
  5 s connect timeout when the URL sets none. Failed refreshes back off up to
  30 s.
- **feature_flags:** a database failure does not turn flags off. A failed
  refresh keeps the last-known snapshot. A failed read in
  `FeatureFlagService` uses the last-known value of the flag. Both log a
  warning and count the error: `PgFlagStore::refresh_errors` and
  `FeatureFlagService::store_errors`.
  A successful write through the service updates the last-known value, so a
  `disable` holds during a later outage.

### Added

- **feature_flags:** `FeatureFlagService::with_default(key, bool)` sets the
  value of a flag that the store does not hold, or cannot read with no
  last-known value. Without it, the value is `false`.
- **feature_flags:** `AppBuilder::with_flag_service` (and
  `TestApp::with_flag_service`) registers a configured service, for example
  one with declared defaults.
- **feature_flags:** `FlagStore::preload` (default: no-op) and
  `PgFlagStore::refresh`. At startup, before user startup hooks, the app runs
  `preload` on the blocking pool and waits up to 5 s for it. `autumn build`,
  one-off tasks and `TestApp::build` wait until it ends.

### Changed

- **feature_flags:** `PgFlagStore::spawn_poll_listener` loads all flags at
  once and then on each interval. Its thread stops when the store drops.
- **feature_flags:** `PgFlagStore`'s `Debug` output no longer shows the
  database URL, which can hold a password.

### Documentation

- **feature_flags:** the guides said that replicas use `LISTEN/NOTIFY`. They
  poll. The guides now say so, and describe the store-failure behavior.
