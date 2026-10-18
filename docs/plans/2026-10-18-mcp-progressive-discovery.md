# MCP Progressive Discovery

> **Status: executed.** Spec: MCP revision `2026-07-28`
> ([tools](https://modelcontextprotocol.io/specification/2026-07-28/server/tools),
> [client best practices](https://modelcontextprotocol.io/docs/2026-07-28/develop/clients/client-best-practices)).
> Prior work: #1117 (MCP tools), the MCP Tasks plan
> (`2026-10-11-mcp-tasks.md`).

## Problem

`tools/list` sends every tool in one response. Each tool costs context
tokens before the agent does any work. Tool choice gets worse past 30 to 50
tools. A large app, or a plugin such as Harvest that makes four or five tools
for each workflow, goes past that limit.

## Gap audit

| Item | State before this plan | Gap |
| --- | --- | --- |
| Server guidance | `initialize` and `server/discover` have no `instructions`. | The client cannot tell the model when to search. |
| Core tools | No marker. | A client that defers tools also defers the important ones. |
| Catalog size | `tools/list` sends all tools. | No way to send a small entry set. |
| Search | None. | A client without its own tool search sees all tools or none. |
| Ordering | Registration order, stable for a build. | None. |
| Pagination | None. | Out of scope (see "Decisions"). |

## Facts (white hat)

- Claude Code, the Claude API tool search, the OpenAI Responses API tool
  search and VS Code virtual tools defer tools on the client.
- Claude Code reads `_meta["anthropic/alwaysLoad"]` on a tool. It reads the
  server `instructions` to decide when to search.
- `DiscoverResult` (2026-07-28) and `InitializeResult` (2025) both have an
  optional `instructions` string.
- 2026-07-28: `tools/list` "MUST NOT vary per-connection or as a side effect
  of other requests". It may vary with authorization.
- A change to the tool list during a session invalidates the prompt cache.
  ChatGPT and the OpenAI Agents SDK do not refresh on `list_changed`.
- Anthropic tool search: about 85% fewer upfront tokens, and higher tool
  accuracy on its MCP evals.
- Speakeasy: models skip a search tool unless its description gives a
  category outline. Meta-tools add two or three calls for each task.
- Official client guidance: a catalog step (`search_tools`), an inspect
  step (detail levels), and an execute step (`call_tool`).

## Brainstorming

1. Server `instructions` from the builder.
2. A per-route `core` flag that emits `_meta["anthropic/alwaysLoad"]`.
3. A catalog mode that lists core tools plus `search_tools` and `call_tool`.
4. Three meta-tools: `search_tools`, `describe_tool`, `call_tool`.
5. `notifications/tools/list_changed` with `enable_toolset`.
6. Cursor pagination of `tools/list`.
7. Pick the catalog per request from a header or a query parameter.
8. A second mount path for the small catalog.
9. Embedding search.
10. A category outline (from route tags) in the `search_tools` description.
11. Code mode: one `execute` tool that runs a script.
12. The draft `io.modelcontextprotocol/groups` extension.

Selected: 1, 2, 3, 10, and the detail levels of 4 inside `search_tools`.

- Item 4 as a separate tool adds one more tool and one more call.
  `search_tools` with `detail = "full"` does the same work.
- Item 5 breaks the 2026-07-28 rule and the prompt cache.
- Item 6 does not reduce tokens, because a client reads all pages.
- Items 7 and 8 need a second catalog per mount, and a client sets them by
  hand. The builder mode is enough for one app. They can come later.
- Item 9 needs a model and a dependency.
- Item 11 needs a sandbox.
- Item 12 is a draft that can change.

## Reverse brainstorming (failure modes)

| Failure mode | Prevention |
| --- | --- |
| An existing client sees a different catalog. | `McpCatalog::Full` is the default. A full catalog is the same as before, plus `_meta` on core tools only. |
| `call_tool` skips auth, guards, rate caps or audit. | `call_tool` replaces the call parameters and goes through the same dispatch path as a direct call. |
| `call_tool` calls itself in a loop. | `call_tool` refuses a meta-tool name with `-32602`. |
| An agent uses `call_tool` to find tools that do not exist. | An unknown name gets the same `-32602` as a direct call. |
| A client skips the confirmation prompt for `call_tool`. | `call_tool` has `readOnlyHint: false` and `destructiveHint: true`. |
| `search_tools` shows a tool that `tools/list` would hide. | `search_tools` reads the same catalog. `secure_mcp` gates both. |
| A user tool named `search_tools` hides a meta-tool. | In progressive mode the router drops the user tool with a warning, as for a duplicate name. The AEO documents see the same list. |
| One huge `query` blocks a worker. | A query has at most 512 bytes and 16 distinct words. The server builds the word index once, at start. |
| A gateway rule on `Mcp-Name` misses the inner tool. | The guide says so. A gateway must read `params.arguments.name`, or refuse `call_tool`. |
| A client rule that allows `call_tool` allows every tool. | The guide says so, and tells the author to mark a destructive tool `core`. |
| An operator cannot tell a wrapped call from a direct call. | The audit record adds `via = "call_tool"`. |
| The model never searches. | The `search_tools` description gives the tool count and the tags. The default instructions say when to search. |
| Search results change between calls. | Scores are integers. Ties keep registration order. |
| A large `limit` returns the whole catalog with schemas. | `limit` is 1 to 50. |
| A hidden tool cannot be called directly. | A direct `tools/call` by name still works. |
| A task or streaming tool breaks behind `call_tool`. | `call_tool` keeps `params._meta`, so the task capability and the progress token reach the inner call. |
| The `core` flag exposes a route. | `core` is metadata only. It does not opt a route in. |
| `ApiDoc` struct literals break. | Documented in `docs/migrations/next.md`, as for `mcp_task`. |

## Six thinking hats

- **White:** see "Facts".
- **Red:** an author wants a short catalog without new route code. One
  builder call (`mcp_catalog`) does it. An agent that cannot find a tool
  feels stuck. An empty query lists every tool.
- **Black:** the meta path costs extra calls. Arguments sent through
  `call_tool` are not checked against a schema by the client. The server
  still checks them, and a bad argument gets the normal error.
- **Yellow:** a large app fits the 30 to 50 tool limit. Deferring clients
  keep typed tools in full mode. Other clients get the meta path.
- **Green:** read the route tags for the category outline. Use the same
  `tool_ok` result shape, with `structuredContent`.
- **Blue:** TDD order: integration tests and unit tests (red), API and
  catalog module (green), refactor, docs, multi-angle review.

## Decisions

1. `#[api_doc(mcp, core)]` and `Route::mcp_core()` set `ApiDoc::mcp_core`.
   `Route::mcp_core()` also sets `mcp_tool`, as `Route::mcp_task()` does.
2. A core tool carries `_meta: {"anthropic/alwaysLoad": true}` in both
   catalog modes and in both eras.
3. `AppBuilder::mcp_instructions` and `TestApp::mcp_instructions` set the
   server `instructions`. `initialize` and `server/discover` return them.
4. `AppBuilder::mcp_catalog(McpCatalog)` and `TestApp::mcp_catalog` select
   the catalog. `McpCatalog` is `#[non_exhaustive]`, with `Full` (default)
   and `Progressive`.
5. In progressive mode `tools/list` returns `search_tools`, `call_tool`, then
   the core tools in registration order. Both meta-tools carry the
   `alwaysLoad` marker.
6. `search_tools` takes `query`, `detail` (`name`, `summary`, `full`) and
   `limit` (1 to 50, default 10). It returns `total` and `tools`.
7. `call_tool` takes `name` and `arguments`, then runs the named tool as a
   direct `tools/call` would.
8. In progressive mode the server appends default guidance to the
   instructions.
9. Out of scope: pagination, `list_changed`, the groups extension, embedding
   search, a per-request catalog choice, and Harvest adoption.
10. Code review (spec, security, API and docs, tests) found and fixed: an
    unbounded `query`, per-call word splitting, byte-based prefix matching,
    repeated query words, no camelCase split, a reserved-name drift into the
    AEO documents, a `null` arguments mismatch, dropped outer parameters,
    blank instructions, missing `via` audit metadata, the `Mcp-Name` and
    approval caveats, and gaps in tests and docs.
11. `call_tool` does not refuse a destructive tool. The guide documents the
    approval caveat instead. A later `McpCatalog` variant can drop
    `call_tool`, because the enum is `#[non_exhaustive]`.

## Acceptance criteria

| ID | Criterion |
| --- | --- |
| AC1 | `#[api_doc(mcp, core)]`, `core = true` and `core = false` parse. `core` sets `ApiDoc::mcp_core`. The unknown-key error lists `core`. |
| AC2 | `Route::mcp_core()` sets `mcp_tool` and `mcp_core`. |
| AC3 | A core tool has `_meta["anthropic/alwaysLoad"] = true` in `tools/list`, in both eras. Other tools have no `_meta`. |
| AC4 | `mcp_instructions` puts `instructions` in the `initialize` and `server/discover` results. With no instructions and the full catalog, the field is absent. |
| AC5 | With the default catalog, `tools/list` lists every tool, as before. The existing MCP suites pass unchanged. |
| AC6 | In progressive mode `tools/list` returns `search_tools`, `call_tool` and the core tools only. The meta-tools carry the `alwaysLoad` marker. |
| AC7 | `search_tools` matches names, descriptions, tags and argument names. It ranks by score, honors `detail` and `limit`, returns `total`, and lists all tools for an empty query. |
| AC8 | A bad `search_tools` or `call_tool` argument gets `-32602`. |
| AC9 | The `search_tools` description gives the tool count and the tags. |
| AC10 | `call_tool` runs a hidden tool through the real pipeline. A guard on the route still refuses. |
| AC11 | `call_tool` refuses an unknown name and a meta-tool name with `-32602`. |
| AC12 | A direct `tools/call` of a hidden tool still works. |
| AC13 | `call_tool` works in the modern era, and returns a task handle for a task tool when the client declares tasks. |
| AC14 | Progressive mode adds default guidance after the author's instructions. |
| AC15 | In progressive mode a user tool named `search_tools` or `call_tool` is skipped with a warning. |
| AC16 | `search_tools` has `readOnlyHint: true`. `call_tool` has `readOnlyHint: false` and `destructiveHint: true`. |
| AC17 | `TestApp` mirrors `mcp_instructions` and `mcp_catalog`. |
| AC18 | The guide, a changelog fragment and a migration note describe the change in ASD-STE100 style. `fmt`, `clippy` and the docs and changelog gates pass. |
| AC19 | `search_tools` refuses a `query` over 512 bytes or 16 distinct words with `-32602`. |
| AC20 | The audit record of a `call_tool` call names the inner tool and has `via = "call_tool"`. The grant rate cap counts calls of both kinds. |
| AC21 | `core` alone does not expose a route. `call_tool` streams a streaming tool, and forwards the caller session. |
