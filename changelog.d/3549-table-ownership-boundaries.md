### Added

- **graph:** `autumn graph check-boundaries` checks the table-ownership
  boundaries in `boundaries.toml` (issue #3549).
  - The graph rule fails when a route, job or repository in one boundary
    reaches a table that another boundary owns. It finds the Diesel DSL, a
    repository and a raw-SQL `JOIN`.
  - The crate rule reads `cargo metadata`. It fails when a crate in one
    boundary depends on the schema crate of another boundary.
  - A baseline file holds the violations of today. A new violation fails. A
    fixed violation must leave the baseline.
  - `autumn doctor` runs the check on a committed graph. `--strict` fails on a
    violation.
  - A plugin declares its tables in `[package.metadata.autumn.boundary]`.
  - See `docs/guide/table-ownership.md`.
