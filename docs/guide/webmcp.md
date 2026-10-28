# WebMCP: Tools for the Agent in the Browser

[WebMCP](https://github.com/webmachinelearning/webmcp) lets a page give tools
to an AI agent that runs in the user's browser. Autumn makes these tools from
your routes and your forms.

> **Status.** WebMCP is a W3C community-group draft (2026-04-23). It is not a
> standard. Chrome has it behind a flag. Firefox and Safari do not have it.
> The page script tracks the draft and can change in a minor release. See
> [STABILITY.md](../../STABILITY.md#webmcp-issue-2619).

## How it is different from `/mcp`

| | `/mcp` ([guide](mcp.md)) | WebMCP |
|---|---|---|
| Agent | A server agent, on the other side of the network | An agent in the user's own tab |
| Credential | A bearer token (`RequireApiToken`) | The user's session cookie and CSRF token |
| Opt-in | `#[api_doc(mcp)]` | `#[api_doc(webmcp)]` |
| Transport | JSON-RPC to `/mcp` | A same-origin `fetch` to the route |

Both use the same tool derivation. A route tagged `#[api_doc(mcp, webmcp)]`
has the same name and the same `inputSchema` on both surfaces.

`#[api_doc(mcp)]` does **not** make a WebMCP tool. A tool for a server agent
is not always safe for a browser agent. The browser agent uses the session
of the user.

## Route tools

1. Enable the `webmcp` feature. It enables `mcp`.

   ```toml
   autumn-web = { version = "...", features = ["webmcp"] }
   ```

2. Tag each route:

   ```rust
   #[get("/todos.json")]
   #[api_doc(webmcp, summary = "List the todos")]
   async fn list_todos(mut db: Db) -> AutumnResult<Json<Vec<Todo>>> { /* ... */ }
   ```

3. Serve the tool list:

   ```rust
   autumn_web::app()
       .routes(routes![list_todos])
       .mount_webmcp()
       .run()
       .await;
   ```

4. Add the head tags to your layout:

   ```rust
   html! {
       head {
           (autumn_web::webmcp::head_tags(csrf.as_ref().map(CsrfToken::token)))
       }
   }
   ```

   This writes `<meta name="csrf-token">` and the script tag. The default
   CSP (`script-src 'self'`) allows the script. No nonce is necessary.

The rules for a route tool are the `/mcp` rules. The route returns
`Json<T>`, or an empty-body status (`204`/`205`). The tool gets its `body`
argument from the `Json<T>` extractor of the route. An HTML route is not a
tool. A stream route (`#[api_doc(mcp, stream)]`) is not a WebMCP tool.

| Form | Effect |
|---|---|
| `#[api_doc(webmcp)]`, `Route::webmcp()` | The route is a tool. |
| `#[api_doc(webmcp = false)]`, `Route::webmcp_exclude()` | The route is never a tool. |
| `.expose_all_as_webmcp()` | Each eligible `GET` is a tool. A write still needs the tag. |

### What a call does

The script reads `/_autumn/webmcp/tools.json` and registers each tool with
`document.modelContext.registerTool()` (or `navigator.modelContext` in older
builds). When the agent calls a tool, the script:

1. puts the path parameters into the path,
2. writes the `query` argument in the `Query<T>` format,
3. sends the `body` argument as JSON,
4. sends a same-origin `fetch` with the session cookie and the CSRF token.

So the real route pipeline runs: `#[secured]`, policies, tenancy,
validation, and rate limits. An anonymous agent gets the same `401` as an
anonymous `fetch`. The result is the response text. A status that is not
`2xx` gives `isError: true`.

The script refuses a path parameter that is empty, `.` or `..`. The URL
parser would remove such a segment and send the call to a different route.
The script does not follow a redirect. A redirect gives an error result, so
the CSRF header cannot go to another origin. When the agent cancels a call,
the script stops the request.

A WebMCP call does not go through the `/mcp` dispatcher. So the grant `rate`
cap and the agent audit record of `/mcp` do not apply. Use `/mcp` for a tool
that needs them.

## Asking the user first

A tool asks the user before it sends when its derived annotations say
`destructiveHint: true`. This is so for:

- a `DELETE` route,
- a route with an `#[agent_operable]` grant that is not `reversible`.

The script uses the agent's `requestUserInteraction()` when the agent gives
one. Otherwise it uses `window.confirm()`. When the user says no, or when
the script cannot ask, the tool gives an error result and sends nothing.

A write tool also gets the `consequentialHint` annotation, so the browser
can ask too.

## Form tools

A form can be a tool. Put the marker in the form:

```rust
form.form_tag("/todos", "post", html! {
    (autumn_web::webmcp::form_tool("add_todo", "Add an item to the todo list"))
    label for="title" { "Title" }
    input id="title" name="title" required maxlength="200";
    button { "Add" }
})
```

The script makes the `inputSchema` from the form fields: the name, the type,
`required`, `minlength`, `maxlength`, `pattern`, `min`, `max`, the `<select>`
options, and the label text. It does not include hidden, file, password,
disabled, or read-only fields. `mount_webmcp()` is not necessary for form
tools.

When the agent calls the tool, the script fills the fields and submits the
form. For an htmx form, the result is the text of the htmx response. For a
plain form, the page loads the response. These give an error result and send
nothing:

- an invalid form,
- a value that the field cannot hold (for example, a `<select>` value with
  no option),
- a second call to a form while its first call runs.

When the agent cancels a call, the script stops the htmx request. A call
that the agent cancels before the submit sends nothing.

A `DELETE` form asks the user first. The method comes from `hx-delete` and
the other `hx-*` verbs, else from `method`. On a `POST`, the `_method` field
replaces it, also for `hx-post`.
For another form that the user cannot undo, use
`webmcp::form_tool_confirm(name, description)`. The script asks before it
fills a field, so a "no" changes nothing on the page.

Form tools are not in the agent-authority manifest. The route that the form
sends to still applies its own guards.

### htmx swaps

A form tool lives as long as its form. Before htmx swaps content, the script
unregisters the form tools in that content. It also unregisters the tool of
a form that holds the content, because the fields can change. After the
swap, it registers the forms again. So an agent cannot call a tool whose
form is not on the page.

## Authority

Run `autumn agents manifest` to see the tools. The manifest lists WebMCP
exposure apart from `/mcp` exposure:

- an action row has `"exposure": "webmcp-tool"` when the route is a WebMCP
  tool and not an MCP tool,
- `route.webmcp_tool` is `true` for each route that is a WebMCP tool,
- `ungoverned_webmcp_tools` lists each WebMCP tool with no grant.

`--check` fails on an ungoverned WebMCP route tool that can write, as it does
for `/mcp`. Such a tool runs in the session of the user, so give it a grant.
The manifest reads the `webmcp` tag. It lists the route also when the app
does not call `mount_webmcp()`. Form tools are not in the manifest. See
[The Agent Authority Envelope](agent-authority.md).

## AEO and `/_autumn/webmcp.js`

[AEO](aeo.md#webmcp) has its own script, `/_autumn/webmcp.js`. It registers
the public `/mcp` tools and calls them through `/mcp`. When the app calls
`mount_webmcp()`, that script skips each name that the projection registers.
So no name is registered twice. Then add both head tags to the layout:
`aeo::webmcp::head_tags` and `webmcp::head_tags`.

## `tools.json`

`GET /_autumn/webmcp/tools.json` is public: the script must read it. It
shows the name, schema, method, and path of each WebMCP tool to each
client, also when `/openapi.json` is off and `/mcp` has a guard. Do not tag
a route whose path or schema must stay secret. Its guards still apply to
each call.

The response has `Cache-Control: private, no-cache` and
`X-Robots-Tag: noindex`. A route of your own at the same path wins. With the
`webmcp` feature on and no `mount_webmcp()`, the list is empty. So a page
with only form tools gets no `404`.

## Not in this release

- Cross-origin tools (`exposedTo`) and `allow="tools"` iframes.
- Streaming results.
- A polyfill for browsers with no WebMCP. With no `modelContext`, the
  script does nothing and writes nothing to the console.

## Testing

- The tool derivation needs no browser. Test `tools.json` with `TestApp`
  and `.mount_webmcp()`.
- Node runs the test cases for the script
  (`autumn/tests/fixtures/webmcp/shim_harness.js`).
- The browser cases need Chromium and the `system-tests` feature. Use
  `SystemTest::new().mount_webmcp()`.
