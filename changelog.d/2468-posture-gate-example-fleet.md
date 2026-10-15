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

### Breaking Changes

- **Breaking:** `ScopedGroup` and `RouteInfo` gain a public `layer_guarded`
  field. A struct literal that names every field needs the new field
  ([migration guide](docs/migrations/next.md)).
