### Breaking Changes

- **Breaking:** `autumn_web::graph::served()` takes the app's `&AppState` and
  returns an `Arc` ([migration guide](docs/migrations/next.md)). Each
  application now serves its own graph at `/actuator/graph`.

### Fixed

- **graph:** two applications in one process serve their own architecture
  graph (issue #2520). Before, every application served the graph of the
  first one.
- **graph:** a binary that links two versions of one crate keeps a node for
  each version (issue #2520). An id that the versions share ends in
  `@<version>`. Other ids do not change.
