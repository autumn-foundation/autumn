# WebMCP: Project the Route Registry into the Page — Design

**Date:** 2026-10-25
**Issue:** #2619
**Status:** Validated (brainstorm, reverse brainstorm, six hats)
**Spec:** WebMCP CG draft, 2026-04-23. Not a standard. The API changes.

## Goal

An agent in the user's browser calls Autumn routes as tools. The call uses
the user's own session cookie and CSRF token. The real router runs the call.
The tool list comes from the same derivation as `/mcp`.

## Acceptance criteria

| # | Criterion |
|---|---|
| AC1 | A route with `#[api_doc(webmcp)]` is in `tools.json`. Its `inputSchema` is byte-identical to the `/mcp` tool schema. |
| AC2 | The shim uses `document.modelContext`, then `navigator.modelContext`. With neither, it does nothing and writes nothing to the console. |
| AC3 | A page with the shim works under the default CSP (`script-src 'self'`): no nonce, no inline script. |
| AC4 | `execute` sends a same-origin `fetch` with cookies and the CSRF token. `#[secured]` rejects an anonymous call. Tenancy resolves the same tenant. |
| AC5 | A form tool in an htmx fragment is unregistered when htmx swaps the fragment out. |
| AC6 | A tool behind an irreversible grant (or `destructiveHint`) asks the user before it sends. A "no" gives an error result and sends nothing. |
| AC7 | The agent-authority manifest reports WebMCP exposure apart from `/mcp` exposure. |
| AC8 | A guide (`docs/guide/webmcp.md`) and a worked example (`examples/todo-app`). |

## Brainstorming

Ideas, no filter:

- Reuse `mcp::derive_tools` with the WebMCP flags. One schema, no drift.
- Serve `tools.json` from the router fallback, as AEO serves its documents.
  A user route at the same path wins, and no collision check is necessary.
- Serve the shim as a framework script, next to `autumn-htmx-csrf.js`.
- Let the shim find its base path from its own URL. Apps under a prefix work.
- Mark a form as a tool with one hidden marker element. `form_tag` stays as
  it is.
- Make the form `inputSchema` from the form fields in the DOM.
- Tie each form tool to an `AbortController`. Abort it on `htmx:beforeSwap`.
  Scan again on `htmx:afterSettle`.
- Ask the user with `requestUserInteraction()` when the agent gives it.
- Put a `webmcp` flag on manifest rows. List ungoverned WebMCP tools apart.
- Send an `Autumn-WebMCP` header so the server can count agent calls.
- Vendor a polyfill for other browsers.

## Reverse brainstorming

"How do we make this harmful?" and the guard for each:

| Harm | Guard |
|---|---|
| `#[api_doc(mcp)]` quietly puts a tool in every visitor's page. | A separate opt-in: `#[api_doc(webmcp)]`. `mcp` does not imply it. |
| The hatch exposes a `DELETE` nobody tagged. | `expose_all_as_webmcp()` takes `GET` only, as `expose_all_as_mcp()` does. |
| The WebMCP schema drifts from the `/mcp` schema. | One derivation. A test compares the bytes. |
| A tool call skips auth, CSRF, or tenancy. | `execute` calls the real route with `fetch`. The full pipeline runs. |
| A cross-site page calls the tools. | Same-origin `fetch` with `credentials: "same-origin"`. CSRF still applies. |
| An agent deletes data with no human check. | `confirm` tools ask the user first. No prompt primitive means no call. |
| An agent calls a form tool whose DOM is gone. | Abort on swap. Drop any form that is not in the document. |
| Two scripts register one name. | The AEO script skips the names the projection serves. |
| A crawler indexes `tools.json`. | `X-Robots-Tag: noindex`. |
| A shared cache keeps one visitor's cookie with `tools.json`. | `Cache-Control: private, no-cache`. |
| The shim breaks the page on a new browser build. | Feature-detect each call. Catch each error. No console output. |
| A stream tool registers, but no WebMCP stream exists. | Stream tools are not projected. A warning names them. |
| An ungoverned write in the user's session passes review. | `--check` fails on it, as it does for `/mcp`. |

## Six thinking hats

- **White (facts):** The current draft has `registerTool(tool, {signal})`,
  `execute(input, {signal})`, and the `readOnlyHint` and `consequentialHint`
  annotations. A duplicate name rejects the promise. Older builds pass a
  client with `requestUserInteraction()`. Chrome 146 ships it behind a flag.
  Firefox and Safari have no date.
- **Red (feelings):** A tool that fires in my own session feels risky. The
  opt-in must be visible in the handler. A "no" from the user must stop the
  call.
- **Black (risks):** The JS API moves. The shim is the unstable part, so keep
  it small. Testing needs a browser. HTML forms differ a lot.
- **Yellow (benefits):** No second pipeline. The page is the client state.
  Forms become tools with no new code. The grant tells us at compile time
  which tools need a human.
- **Green (alternatives):** Call `/mcp` with the cookie (rejected: `/mcp`
  can need a bearer token, and the AEO script already does this for public
  tools). Native `toolname` form attributes (rejected: not in the draft yet).
  Polyfill (out of scope). The `Autumn-WebMCP` header (deferred: not
  needed for the criteria).
- **Blue (process):** TDD per slice: red test, green code, refactor. Order:
  macro flag → derivation → `tools.json` → shim → forms and swaps → confirm
  → manifest → docs and example → CI. Review with agents at the end.

## Design

### Opt-in

| Form | Effect |
|---|---|
| `#[api_doc(webmcp)]`, `Route::webmcp()` | The route is a WebMCP tool. |
| `#[api_doc(webmcp = false)]`, `Route::webmcp_exclude()` | Never a WebMCP tool. |
| `AppBuilder::expose_all_as_webmcp()` | Each eligible `GET` is a tool. |
| `AppBuilder::mount_webmcp()` | Serve `tools.json`. |

Eligibility is the `/mcp` rule: JSON out, or an empty-body status.

### Wire

- `GET /_autumn/webmcp/tools.json`:
  `{"version":1,"csrfHeader":"x-csrf-token","tools":[...]}`. Each tool has
  `name`, `description`, `inputSchema`, `annotations`, `method`,
  `path_template`, plus `path_params`, `has_query`, `has_body`, `confirm`
  and `webmcp_annotations` for the shim.
- `GET /static/js/autumn-webmcp.js` (and its hashed URL): the shim.

### Shim

1. Find `modelContext`. Stop if there is none.
2. Fetch `tools.json`. Register each tool.
3. `execute` fills the path, the query (the `Query<T>` dialect) and the JSON
   body. It sends `fetch` with the CSRF header from
   `<meta name="csrf-token">`.
4. A `confirm` tool asks the user first.
5. `[data-webmcp-tool]` marks a form tool. The fields make the schema.
   `execute` fills the fields and calls `requestSubmit()`.
6. `htmx:beforeSwap` aborts the form tools in the target.
   `htmx:afterSettle` scans again.

### Manifest

`RouteSummary` gets `webmcp_tool` and `webmcp_exposed_by`. `RouteRef` and
`UnregisteredAuthority` get `webmcp_tool`. `ActionRow::exposure` gets the
value `"webmcp-tool"`. The manifest gets `ungoverned_webmcp_tools`. The JSON
omits each new field when it is empty, so a committed manifest does not
drift.
