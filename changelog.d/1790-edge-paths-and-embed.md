### Added

- **edge:** Typed path helpers work inside a capsule (issue #1790). The helper
  of an `#[edge]` route uses the new `autumn_edge::paths` encoders and
  compiles for `wasm32-wasip1`. Collect helpers with `autumn_edge::paths![]`.
  `autumn_edge::paths::PathExt` adds `with_query`. The encoders give the same
  bytes as `autumn_web::paths`.
- **edge:** `autumn build --embed` builds the edge capsule too. Before, it
  stopped with an error when the app had `#[edge]` routes. The capsule build
  gets the `embed-assets` feature, the same as the native build.
