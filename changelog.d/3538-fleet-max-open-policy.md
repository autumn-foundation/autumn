### Breaking Changes

- **Breaking:** `DatabaseFleetConfig` has a new field `max_open_policy`,
  `FleetStats` has the new fields `peak_open`, `over_cap_opens_total` and
  `refused_total`, and `FleetError` has a new variant `AtCapacity` (issue
  #3538). A `DatabaseFleetConfig` literal with no `..` spread, and an
  exhaustive `match` on `FleetError`, need an update
  ([migration guide](docs/migrations/next.md)).

### Added

- **db:** `[database.fleet] max_open_policy` (issue #3538). `"soft"` is the
  default and does not change. `"wait"` parks an open until a slot frees, for
  at most `connect_timeout_secs`, then answers `503`. `"refuse"` answers `503`
  with `Retry-After` at once when no idle database can close. With either
  policy, the open and closing databases never pass `max_open`. A burst then
  cannot fail with "too many open files".
  `FleetStats` and `/actuator/health` (`db:fleet`) add `peak_open`,
  `over_cap_opens_total` and `refused_total`. The fleet guide gives the
  descriptor count and the `ulimit -n` rule.
