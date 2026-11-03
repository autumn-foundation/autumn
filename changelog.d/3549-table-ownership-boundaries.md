### Added

- **graph:** `autumn graph check-boundaries` checks table-ownership boundaries
  declared in `boundaries.toml` (issue #3549). It fails when a route, job or
  repository in one boundary reaches a table that another boundary owns,
  through the Diesel DSL, a repository or a raw-SQL `JOIN`. A crate rule
  reads `cargo metadata` and fails when a crate depends on another
  boundary's schema crate. A baseline file holds today's violations: new ones
  fail, fixed ones must leave the baseline. `autumn doctor --strict` runs the
  same check. A plugin declares its tables in
  `[package.metadata.autumn.boundary]`. See `docs/guide/table-ownership.md`.
