### Added

- **plugins:** `plugin_conformance::run_route_checks`, the route checks of
  `run_conformance` without `experimental-surface`, and
  `RouteSource::from_wire` (issue #2828).

### Changed

- **cli:** `autumn plugin-check` runs `plugin_conformance::run_route_checks`
  (issue #2828). The CLI and the library now give the same result for the
  same routes. One difference stays: `--no-routes` skips only when the
  contract dump gives the registered name. The JSON report has the same
  keys. The CLI now shows the library text. A collision names each source
  as `(source: <source>)`. An undeclared sensitive route shows
  `undeclared sensitive surface` and a hint line. The `--no-routes`
  messages say `no_routes`.
- **plugins:** `run_conformance` with a contract runs the route checks
  under the contract's `registered_as` when only that name carries routes.
  Routes under more than one name of the plugin fail `route-attribution`.
- **plugins:** the `sensitive-surfaces` hint names
  `ConformanceConfig::sensitive_route` and
  `autumn plugin-check --sensitive-route`.

### Fixed

- **cli:** `autumn plugin-check --no-routes` fails when a route carries any
  name of the plugin: `--plugin-name`, the contract's crate name or its
  registered name. Before, it checked one name. It could skip, and without
  a registered name it could pass.
