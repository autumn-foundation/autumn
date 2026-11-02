### Added

- **plugin_conformance:** `ConformanceConfig::no_routes()`, the library
  counterpart of `autumn plugin-check --no-routes`. A plugin that mounts no
  routes (a cache, a background runtime) can now pass `run_conformance` in
  `cargo test`. No route under a name of the plugin (the config name, or the
  contract's `plugin` or `registered_as`) skips `route-attribution`, and such
  a route fails it. A `prefix` together with `no_routes` fails `route-prefix`.
  The default stays fail-closed.
