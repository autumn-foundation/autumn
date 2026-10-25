### Added

- **webmcp:** Autumn gives routes as tools to the agent in the browser of
  the user (issue #2619).
  Enable the `webmcp` feature, tag a route `#[api_doc(webmcp)]` (or
  `Route::webmcp()`), and call `AppBuilder::mount_webmcp()`. Autumn serves
  the tool list at `/_autumn/webmcp/tools.json` with the same derivation and
  `inputSchema` as `/mcp`. The page script `js/autumn-webmcp.js` registers
  each tool with `document.modelContext.registerTool()`. A call is a
  same-origin `fetch` with the user's session cookie and CSRF token, so the
  full route pipeline runs. `#[api_doc(mcp)]` does not imply it.
  `expose_all_as_webmcp()` makes each eligible `GET` route a tool.
- **webmcp:** form tools. `webmcp::form_tool(name, description)` makes a
  `<form>` a tool. `webmcp::form_tool_confirm` makes a tool that asks the
  user first. The script makes the schema from the fields, and submits
  the form through htmx. An htmx swap unregisters the form tools of the
  swapped content.
- **webmcp:** a `DELETE` tool, or a tool behind a grant that is not
  `reversible`, asks the user first (`requestUserInteraction()`, else
  `confirm()`). When the user refuses, the script sends no request.
- **agent-authority:** the manifest reports WebMCP exposure apart from `/mcp`
  exposure: `"exposure": "webmcp-tool"`, `route.webmcp_tool`, and
  `ungoverned_webmcp_tools`. `autumn agents manifest --check` fails on an
  ungoverned WebMCP write.

### Breaking Changes

- **Breaking:** `ApiDoc` gains public `webmcp_tool` and `webmcp_exclude`
  fields. A struct literal needs `..ApiDoc::default()`
  ([migration guide](docs/migrations/next.md)).
- **Breaking:** `agent_authority::manifest::RouteSummary`, `RouteRef`,
  `UnregisteredAuthority` and `AgentAuthorityManifest` gain WebMCP fields. A
  hand-built literal must add them
  ([migration guide](docs/migrations/next.md)).
