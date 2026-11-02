### Added

- **plugin_conformance:** `ConformanceConfig::no_routes()`, the library form of
  `autumn plugin-check --no-routes`. A plugin that mounts no routes (a cache, a
  background runtime) can now pass `run_conformance` in `cargo test`: no
  attributed route skips `route-attribution`, and an attributed route fails
  it. A `prefix` together with `no_routes` fails `route-prefix`. The default
  stays fail-closed.
