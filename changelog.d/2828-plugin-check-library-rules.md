### Added

- **plugins:** `plugin_conformance::check_no_routes`,
  `plugin_conformance::check_prefix_with_no_routes` and
  `From<&str> for RouteSource` (issue #2828).

### Changed

- **cli:** `autumn plugin-check` runs the route checks of
  `autumn_web::plugin_conformance` (issue #2828). The two no longer give
  different results for the same routes. The JSON report has the same keys.
  Some text changes: a collision names each source as `(source: <source>)`,
  and an undeclared sensitive route shows `undeclared sensitive surface`
  and a hint line.
- **plugins:** the `sensitive-surfaces` hint names
  `ConformanceConfig::sensitive_route` and
  `autumn plugin-check --sensitive-route`.

### Fixed

- **cli:** `autumn plugin-check --no-routes` fails when a route carries any
  name of the plugin: `--plugin-name`, the contract's crate name or its
  registered name. Before, it checked only one name and could skip.
