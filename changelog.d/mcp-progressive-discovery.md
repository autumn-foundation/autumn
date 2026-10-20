### Added

- **mcp:** progressive tool discovery. `#[api_doc(mcp, core)]` (or
  `Route::mcp_core()`) marks a core tool. A core tool carries
  `_meta["anthropic/alwaysLoad"]`, so a client that defers tools loads it
  first. `AppBuilder::mcp_instructions` sets the server `instructions` on
  `initialize` and `server/discover`. `AppBuilder::mcp_catalog(McpCatalog::Progressive)`
  makes `tools/list` send the core tools and two meta-tools. `search_tools`
  finds a tool. `call_tool` runs it through the normal dispatch path. See
  `docs/guide/mcp.md` §3.

### Breaking Changes

- **Breaking:** `ApiDoc` gains a public `mcp_core` field. A struct literal
  needs `..ApiDoc::default()`
  ([migration guide](docs/migrations/next.md)).
