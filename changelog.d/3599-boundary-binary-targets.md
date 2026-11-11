### Breaking Changes

- **Breaking:** `CrateGraph` has the public field `bins` and is
  `#[non_exhaustive]` (issue #3599). A struct literal does not compile.
  Start from `CrateGraph::default()`. A destructure must end with `..`
  ([migration guide](docs/migrations/next.md)).

### Fixed

- **graph:** `autumn graph check-boundaries` now puts a node of a renamed
  `[[bin]]` target in the boundary of its package (issue #3599). Before, a
  route under `marketing_server::…` in the package `crm-marketing` was in no
  boundary, so the graph rule did not check it. A pattern that matches the
  binary name itself still wins. A binary with the name of a library does
  not take the nodes of that library. The crate rule does not use binary
  names, because a dependent links the library only.
