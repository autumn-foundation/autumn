### Added

- **mcp:** MCP Tasks extension (`io.modelcontextprotocol/tasks`). A tool
  tagged `#[api_doc(mcp, task)]` (or `Route::mcp_task()`) returns a task
  handle to a client that declares the extension. The client polls
  `tasks/get`, and can call `tasks/cancel` and `tasks/update`. A task runs
  outside the `POST /mcp` request, so only the route's own timeout applies.
  `AppBuilder::mcp_tasks(McpTaskConfig)` sets the TTL, poll interval, store
  size and per-caller quota. Autumn keeps tasks in memory, per process. Only a
  caller with the same credentials can read a task. See `docs/guide/mcp.md`
  §6.

### Breaking Changes

- **Breaking:** `ApiDoc` gains a public `mcp_task` field. A struct literal
  needs `..ApiDoc::default()`
  ([migration guide](docs/migrations/next.md)).
