### Added

- **routes:** `AppBuilder::scoped_guarded` declares that a layer, such as
  `RequireApiToken`, guards a route group (issue #2468). `autumn routes audit`
  lists those routes as `gated` with `provenance: "declared"`.
- **examples:** the security posture gate now also covers `flock`, `invoice`
  and `todo-app`.
