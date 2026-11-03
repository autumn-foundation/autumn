### Breaking Changes

- **Breaking:** `state.role()` returns `&Role`, not `ProcessRole`, and
  `AutumnConfig::role` is a `RoleName` (issue #3543). `serves_http()` and
  `runs_workers()` still work on the result. Use `state.role().base()` for the
  `ProcessRole`. A top-level `[roles]` table now belongs to the framework
  ([migration guide](docs/migrations/next.md)).

### Added

- **roles:** named role presets, `[roles.<name>]` (issue #3543). A preset is
  a built-in `base` (`combined`, `web` or `worker`) plus `pin`, `workers` and
  capability overrides. Select it with `AUTUMN_ROLE=<name>`,
  `role = "<name>"` or `autumn serve --role <name>`. An override can only turn
  a capability off. The durable-backend and file-backed SQLite checks read the
  base. Built-in names are reserved. An undeclared name stops the boot.
- **roles:** `state.role().name()` gives the role name in handlers,
  `on_startup` hooks and plugins. `/actuator/info` and `/actuator/jobs` show
  the name and the base. A preset adds a `role` label to the framework
  metrics and prefixes the default scheduler `replica_id` with its name.
- **doctor:** `autumn doctor` derives `[jobs.fleet] tiers` from `[roles.*]`
  when `tiers` is not set, so `--strict` checks queue coverage with no second
  declaration.
