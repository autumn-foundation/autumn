### Fixed

- **feature_flags:** `PgFlagStore` reads flags from an in-memory snapshot
  (issue #3063). A flag read does not connect to the database on a Tokio
  worker. A stale snapshot refreshes on the blocking pool, one refresh at a
  time.
- **feature_flags:** a database failure does not turn flags off. A failed
  refresh keeps the last-known snapshot. A failed read in
  `FeatureFlagService` uses the last-known value of the flag. Both log a
  warning and count the error: `PgFlagStore::refresh_errors` and
  `FeatureFlagService::store_errors`.

### Added

- **feature_flags:** `FeatureFlagService::with_default(key, bool)` sets the
  value of a flag that the store does not hold, or cannot read with no
  last-known value. Without it, the value is `false`.
- **feature_flags:** `FlagStore::preload` (default: no-op) and
  `PgFlagStore::refresh`. `with_flag_store` calls `preload` once at startup on
  the blocking pool.

### Changed

- **feature_flags:** before its first load, `PgFlagStore::get` on a Tokio
  runtime returns an error and starts a load in the background. Call
  `PgFlagStore::refresh` first when you read flags before the app starts.
- **feature_flags:** `PgFlagStore::spawn_poll_listener` loads all flags at
  once and then on each interval. Its thread stops when the store drops.

### Documentation

- **feature_flags:** the guides said that replicas use `LISTEN/NOTIFY`. They
  poll. The guides now say so, and describe the store-failure behavior.
