### Added

- **routes:** `AppBuilder::scoped_guarded` declares that a layer, such as
  `RequireApiToken`, guards a route group (issue #2468). `autumn routes audit`
  lists these routes as `gated`, with `provenance: "declared"`. The route dump
  JSON adds `layer_guarded`. A plugin cannot use it for its own routes.
- **routes:** `autumn routes posture diff` reports `guard_declared` (widening)
  when a proven guard becomes a declared one, and `guard_proven` (narrowing) in
  the other direction.
- **examples:** the security posture gate now also covers `flock`, `invoice`
  and `todo-app`.

### Changed

- **routes:** the security manifest is now schema v6. Each route entry can
  carry `provenance: "declared"`. An older `autumn` CLI would read such a guard
  as a proven one, so it now refuses a v6 manifest and asks you to upgrade. A
  v3 to v5 manifest still reads.
- **routes:** a posture acknowledgment ceiling now records whether a route's
  guard is proven or declared. A marker approved for a proven guard does not
  approve the same route once a layer only declares the guard.

### Breaking Changes

- **Breaking:** `ScopedGroup` and `RouteInfo` gain a public `layer_guarded`
  field. A struct literal that names every field needs the new field
  ([migration guide](docs/migrations/next.md)).
