### Breaking Changes

- **Breaking:** `graph::NodeKind` has a new variant `BackgroundTask`,
  `GraphNode` has a new field `background`, and `Completeness` has a new field
  `background_tasks` (issue #3545). An exhaustive `match` on `NodeKind` and a
  `GraphNode { .. }` literal need an update
  ([migration guide](docs/migrations/next.md)).

### Added

- **background:** role-aware background tasks for plugins and apps (issue
  #3545). `AppBuilder::background_task(name, capability, |state, shutdown| ..)`
  starts a loop only on the roles that run its capability. The graceful drain
  waits for it. `AppBuilder::background(BackgroundTask)` adds `queues` (the
  role `pin` applies) and an `explicit` switch. The plugin switch wins. The
  boot then logs a warning.
- **roles:** a new capability, `maintenance`, for housekeeping loops. It runs
  on `combined` and `worker`. A preset can turn it off with
  `maintenance = false`.
- **roles:** plugin capabilities. `PluginCapability::new("harvest.worker",
  Capability::JobWorkers)` follows its parent. A named role sets it with
  `[roles.<name>] capabilities = { "harvest.worker" = false }`. A role cannot
  turn on a plugin capability that its base does not run. An unknown key stops
  the boot of that role. `Role::runs_plugin` reads it.
- **observability:** `/actuator/tasks` has `background_tasks`, and
  `autumn graph` has `background_task` nodes. Each lists the roles of the
  config that run the task. `autumn jobs manifest` includes task queues, so
  the `autumn doctor` coverage check sees them.
