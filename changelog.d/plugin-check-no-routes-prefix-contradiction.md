### Fixed

- **plugin-check:** `autumn plugin-check --no-routes --prefix <p>` now fails
  `route-prefix` instead of skipping it. `--no-routes` and `--prefix` are a
  contradiction — a plugin cannot both mount no routes and have all of them
  live under a prefix — and the CLI now reports it the same way the library's
  `ConformanceConfig::no_routes` already does (issue #2828).
