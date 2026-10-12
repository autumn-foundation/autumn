### Added

- **mcp:** MCP Tasks extension (`io.modelcontextprotocol/tasks`). A tool
  tagged `#[api_doc(mcp, task)]` (or `Route::mcp_task()`) returns a task
  handle to a client that declares the extension. The client polls
  `tasks/get`, and can call `tasks/cancel` and `tasks/update`. A task runs
  outside the `POST /mcp` request, so only the route's own timeout applies.
  `AppBuilder::mcp_tasks(McpTaskConfig)` sets the TTL, poll interval, store
  size, per-client quota and stored-result byte budget. Autumn keeps tasks in
  memory, per process. Only a caller with the same credentials can read a
  task. See `docs/guide/mcp.md` §6.

- **mcp:** MCP protocol revision `2026-07-28`. The endpoint is dual-era: a
  request with `io.modelcontextprotocol/protocolVersion` in `params._meta`
  is served with the modern rules (`server/discover`, per-request client
  capabilities, `resultType` and `serverInfo` on results, the
  `MCP-Protocol-Version`, `Mcp-Method` and `Mcp-Name` header checks, and the
  `-32020`, `-32021` and `-32022` errors). Legacy clients keep the
  `initialize` flow. See `docs/guide/mcp.md` §3.

### Breaking Changes

- **Breaking:** `ApiDoc` gains a public `mcp_task` field. A struct literal
  needs `..ApiDoc::default()`
  ([migration guide](docs/migrations/next.md)).
