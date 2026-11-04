### Breaking Changes

- **Breaking:** `Route` has a new public field `group`, and `AutumnConfig`
  has a new field `route_groups` (issue #3544). A `Route { .. }` literal
  needs `group: None`. The route macros set it. A top-level `[route_groups]`
  table now belongs to the framework
  ([migration guide](docs/migrations/next.md)).

### Added

- **routing:** route groups (issue #3544). A route gets its group from
  `#[get("/x", group = "marketing")]` or `Route::in_group`, then
  `AppBuilder::routes_in("marketing", ...)`, then `Plugin::route_group()`
  (default `Plugin::name()`), then `app`.
- **roles:** `[roles.<name>] mount = ["marketing"]` serves only those
  groups. A role without `mount` serves every group. Probes, the actuator,
  assets and `/_autumn/*` stay on every role. A route in another group gets `404`. A
  `mount` entry that names no group stops the boot. `/actuator/info` shows
  the list as `role.mount`.
- **graph:** `autumn graph`, `/actuator/graph`, `autumn openapi export` and
  `/openapi.json` show only the routes of the role. The new
  `completeness.mounted_elsewhere` lists routes that another declared role
  mounts. The graph does not report these routes as drift. Pre-rendered pages
  and sitemap entries follow the mount too.
- **doctor:** the `route_group_mounts` row warns, and `--strict` fails, when
  no `[roles.<name>]` preset mounts a group in `[route_groups]` (`manifest`
  or `declared`).
- **routes:** `autumn routes --format json` shows the `group` of each route.
