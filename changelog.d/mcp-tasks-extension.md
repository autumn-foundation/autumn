### Added

- **mcp:** MCP Tasks extension (`io.modelcontextprotocol/tasks`). A tool
  tagged `#[api_doc(mcp, task)]` (or `Route::mcp_task()`) returns a task
  handle to a client that declares the extension. The client polls
  `tasks/get`, and can call `tasks/cancel` and `tasks/update`. A task runs
  outside the `POST /mcp` request, so only the route's own timeout applies.
  `AppBuilder::mcp_tasks(McpTaskConfig)` sets the TTL, poll interval and store
  size. Tasks are in memory, per process, and bound to the caller's
  credentials. See `docs/guide/mcp.md` §6.
