### Added

- **plugins:** `plugin_conformance::run_route_checks` runs the route checks
  of `run_conformance`. It does not run `experimental-surface`.
  `RouteSource::from_wire` reads the route-dump source string
  (issue #2828).

### Changed

- **cli:** `autumn plugin-check` runs `plugin_conformance::run_route_checks`
  (issue #2828). The CLI and the library now give the same result for the
  same routes. One difference stays: `--no-routes` skips only when the
  contract dump gives the registered name. The JSON report has the same
  keys. The CLI now shows the library text:
  - A collision names each source as `(source: <source>)`. Collisions sort
    by path, then by method.
  - An undeclared sensitive route shows `undeclared sensitive surface` and
    a hint line.
  - The library `no_routes` messages say `no_routes`.
  - A split-name failure lists `--plugin-name` first, then the crate name,
    then the registered name.
- **plugins:** `run_conformance` with a contract uses the contract's
  `registered_as` when no route carries the config name and a route carries
  `registered_as`. Routes under more than one name of the plugin fail
  `route-attribution`.
- **plugins:** the `sensitive-surfaces` hint names
  `ConformanceConfig::sensitive_route` and
  `autumn plugin-check --sensitive-route`.

### Fixed

- **cli:** `autumn plugin-check --no-routes` fails when a route carries any
  name of the plugin: `--plugin-name`, the contract's crate name or its
  registered name. Before, it checked one name. It could skip, and without
  a registered name it could pass.
- **plugins:** with `no_routes` and no registered name, the failure message
  no longer says that another plugin's route is this plugin's own.
