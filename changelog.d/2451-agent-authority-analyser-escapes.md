### Fixed

- **agent authority:** the `#[agent_operable]` analyser now refuses these
  shapes (issue #2451):
  - a future assigned after it is declared, or shadowed in a block
  - an opaque macro with no handle
  - a named future given to `timeout`
  - a fake `commit`, `sleep` or `cache`
  - a callee that is not a path
  - a diesel write to an aliased or unresolved table
  - an assignment made in one branch only

  A data-changing CTE now records every table it writes. A proof no longer
  hides a declared effect. Qualified job paths and function-item aliases no
  longer give false positives.
- **agent authority:** the manifest keeps the first route per `operation_id`,
  as the MCP server does. It marks a skipped route `not-exposed` with the
  caveat `duplicate-operation-id`. `--check` does not fail on it.

### Breaking Changes

- **Breaking:** the `#[agent_operable]` analyser now refuses handler shapes
  that compiled before ([migration guide](docs/migrations/next.md)). Examples:
  a bare `sleep(..)`, `session.x()` or `cache.x()` on an untyped value, an
  opaque macro, and a `diesel::` write to an unresolved table. Write the
  `tokio` path, type the parameter, or add `#[agent_effect(...)]`.
