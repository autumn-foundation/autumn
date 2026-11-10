### Fixed

- **graph:** `autumn graph check-boundaries` now puts a node of a renamed
  `[[bin]]` target in the boundary of its package (issue #3599). Before, a
  route under `marketing_server::…` in the package `crm-marketing` was in no
  boundary, so the graph rule did not check it. A package matches by its
  name, its library name and the names of its binaries.

### Breaking Changes

- **Breaking:** `CrateGraph` has the public field `bins` and is
  `#[non_exhaustive]`. Build it from `CrateGraph::default()`
  ([migration guide](docs/migrations/next.md)).
