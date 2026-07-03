# Exposing Your API as MCP Tools

Autumn already makes it fast to ship a typed JSON API. The `mcp` feature lets
an AI agent *use* that API by projecting your existing routes into a
[Model Context Protocol](https://modelcontextprotocol.io) (MCP) server — the
same way Autumn projects them into an OpenAPI document. You tag the endpoints
you want to expose and mount one server; Autumn derives the tool schemas,
speaks JSON-RPC over Streamable HTTP, and dispatches each tool call through
your **real, authenticated handler pipeline**.

No second app. No hand-written protocol, transport, or tool schemas. The tool
catalog is derived from the same `ApiDoc` metadata that drives `generate_spec`,
so it **cannot drift** from your handlers — change a handler's types and the
tool schema changes with it, with no extra edit.

---

## 1. Enable the feature

The `mcp` feature builds on the OpenAPI schema machinery, so it implies the
`openapi` feature.

```toml
# Cargo.toml
[dependencies]
autumn-web = { version = "0.5", features = ["mcp"] }
```

---

## 2. Tag endpoints and mount the server

Opt in **per endpoint** with `#[api_doc(mcp)]`, then mount the endpoint once:

```rust
use autumn_web::prelude::*;

#[derive(serde::Serialize, serde::Deserialize)]
struct Todo { id: u32, title: String }

#[derive(serde::Serialize, serde::Deserialize)]
struct NewTodo { title: String }

#[get("/api/todos")]
#[api_doc(mcp, summary = "List all todos")]
async fn list_todos() -> AutumnResult<Json<Vec<Todo>>> {
    Ok(Json(vec![Todo { id: 1, title: "first".into() }]))
}

#[post("/api/todos")]
#[api_doc(mcp, summary = "Create a todo")]
async fn create_todo(Json(body): Json<NewTodo>) -> AutumnResult<Json<Todo>> {
    Ok(Json(Todo { id: 42, title: body.title }))
}

#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .routes(routes![list_todos, create_todo])
        .mount_mcp("/mcp")
        .run()
        .await;
}
```

That's it. `POST /mcp` now speaks MCP and exposes `list_todos` and
`create_todo` as agent-callable tools.

**Opt-in is the default; nothing is exposed implicitly.** A route with no
`#[api_doc(mcp)]` tag never becomes a tool.

---

## 3. What the agent sees

`mount_mcp` serves a single Streamable-HTTP endpoint that handles the three
methods an MCP client needs:

| Method | Purpose |
|--------|---------|
| `initialize` | Handshake; returns `serverInfo` and `capabilities.tools`. |
| `tools/list` | The derived tool catalog. |
| `tools/call` | Invoke a tool by name; dispatched through the real pipeline. |

`ping` and JSON-RPC notifications (messages with no `id`, e.g.
`notifications/initialized`) are handled too — notifications get an empty
`202 Accepted`, per the spec.

A `tools/list` entry looks like this — `name`, `description`, `inputSchema`,
and `annotations` are all derived from the handler's `ApiDoc`:

```json
{
  "name": "create_todo",
  "description": "Create a todo",
  "inputSchema": {
    "type": "object",
    "properties": {
      "body": { "$ref": "#/$defs/NewTodo" }
    },
    "required": ["body"],
    "$defs": { "NewTodo": { "type": "object", "title": "NewTodo" } }
  },
  "annotations": { "title": "Create a todo", "readOnlyHint": false }
}
```

### How `inputSchema` is built

Autumn merges the handler's typed contract into one object schema:

- **Path parameters** (`/api/todos/{id}`) become required `string` properties
  named after each capture (`id`).
- A **`Query<T>` extractor** becomes a `query` object property.
- A **JSON request body** (`Json<T>`) becomes a required `body` property.
- Named component schemas are inlined under `$defs` so the schema is
  self-contained.

Because every piece comes from the same `SchemaEntry` data the OpenAPI
generator uses, **there is no second schema to maintain** and no way for the
tool catalog to drift from the handler.

### Safety annotations

The HTTP method maps to MCP safety hints so agents and UIs can reason about
side effects:

| Verb | `readOnlyHint` | `destructiveHint` |
|------|:--------------:|:-----------------:|
| `GET` | `true` | — |
| `POST` / `PUT` / `PATCH` | `false` | — |
| `DELETE` | `false` | `true` |

---

## 4. Calling a tool

`tools/call` takes a tool `name` and an `arguments` object whose shape mirrors
the `inputSchema`: path parameters at the top level, query fields under
`query`, and the JSON body under `body`.

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "tools/call",
  "params": {
    "name": "create_todo",
    "arguments": { "body": { "title": "buy milk" } }
  }
}
```

The handler's JSON response comes back as the tool result's text content:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "content": [{ "type": "text", "text": "{\"id\":42,\"title\":\"buy milk\"}" }],
    "isError": false
  }
}
```

A non-2xx handler response is returned as `isError: true` with the status and
body, rather than a transport-level failure — so an agent can read and recover
from a validation error the same way a human-written client would.

You can drive it from the command line:

```bash
curl -s http://127.0.0.1:3000/mcp \
  -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
```

---

## 5. Streaming progressive results over SSE

A long-running tool — a code search over a large graph, a multi-step report, a
slow scan — feels broken if the agent waits in silence for the whole result.
The MCP Streamable-HTTP transport lets a `tools/call` emit
`notifications/progress` messages (and partial content) over the response's SSE
channel *before* the final result lands. Autumn already ships a first-class SSE
primitive (`sse.rs`: `Sse`/`Event`/`keep_alive`) — the exact transport MCP
streaming rides — so a streaming tool is just **a normal Autumn `Sse` stream
wearing an MCP hat**. You write zero JSON-RPC or SSE framing.

### Opt in with `stream`

Add the `stream` flag to `#[api_doc(mcp, stream)]` and return an `Sse` stream
of `Event`s. Because an `Sse` handler has no JSON response schema, `stream` also
exempts the tool from the JSON-out eligibility gate (see §8):

```rust
use std::convert::Infallible;
use autumn_web::prelude::*;
use autumn_web::sse::{Event, Sse};
use futures::stream::{self, Stream};

#[get("/api/search")]
#[api_doc(mcp, stream, summary = "Streaming code search")]
async fn search() -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    // A plain Autumn stream — each event is one incremental chunk of work.
    let stream = stream::iter(vec![
        Ok(Event::default().data("match src/a.rs:12")),
        Ok(Event::default().data("match src/b.rs:48")),
    ]);
    Sse::new(stream)
}
```

### What rides the wire

When a client calls a streaming tool **and** advertises it can read SSE
(`Accept: application/json, text/event-stream`), Autumn answers the `POST /mcp`
with `Content-Type: text/event-stream` and projects your stream onto it:

- **Each `Event` you yield** becomes a `notifications/progress` message — *but
  only when the client supplied a progress token* in `params._meta.progressToken`
  (per spec, progress requires a token). The event's text is the progress
  `message`; `progress` auto-increments per event.
- **The stream is terminated** by the final id-correlated `tools/call` result,
  whose content is the joined text of the streamed events.

```jsonc
// client → server
{
  "jsonrpc": "2.0", "id": 7, "method": "tools/call",
  "params": {
    "name": "search", "arguments": {},
    "_meta": { "progressToken": "tok-1" }
  }
}

// server → client, as SSE frames (one JSON-RPC message per `data:` frame)
data: {"jsonrpc":"2.0","method":"notifications/progress",
       "params":{"progressToken":"tok-1","progress":1,"message":"match src/a.rs:12"}}

data: {"jsonrpc":"2.0","method":"notifications/progress",
       "params":{"progressToken":"tok-1","progress":2,"message":"match src/b.rs:48"}}

data: {"jsonrpc":"2.0","id":7,"result":{"content":[{"type":"text",
       "text":"match src/a.rs:12\nmatch src/b.rs:48"}],"isError":false}}
```

The **time-to-first-signal is decoupled from total duration**: frames are
forwarded as your stream produces them, so the first progress notification
reaches the agent immediately even when the whole tool takes seconds.

### Structured progress and an explicit final payload

Two optional conventions, still framing-free:

- **Structured progress** — yield an `Event` whose data is a JSON object with a
  numeric `progress` (and optional `total`/`message`); those fields are
  forwarded verbatim into the notification's params instead of the
  auto-incrementing counter.
- **An explicit final result** — yield `Event::default().event("result").data(…)`
  to set the terminating result's content directly. Frames typed `result` are
  *not* surfaced as progress; everything else is.

### Buffered tools and non-SSE clients are unaffected

Streaming is **strictly opt-in per tool**. A tool without `stream` follows the
exact buffered path from §4 — nothing about it changes. And a streaming tool
called by a client that does *not* accept `text/event-stream` is served a
buffered JSON result (its streamed events collapsed into one tool result), so a
plain JSON client is never handed a body it can't read.

### Back-pressure and disconnect

The projection reuses the same lifecycle `sse.rs` uses for a dropped subscriber:
if the agent disconnects mid-stream, axum drops the response and Autumn drops
the underlying handler stream — the handler's task unwinds with no leaked task
and no panic on the closed stream. A `keep_alive` comment is sent on idle so
proxies don't drop a slow stream.

> **Server-initiated `GET` streams are not supported.** Streaming rides the
> `tools/call` `POST` response; a bare `GET /mcp` (for unsolicited server→client
> messages) returns `405`.

---

## 6. Authentication: reuse your bearer tokens

`tools/call` runs through the **real handler pipeline** — the same in-process
path Autumn's [test client](testing.md) uses. That means `#[secured]`,
authorization, tenancy, rate limits, and validation all apply identically to
an agent call and an ordinary HTTP call. There is no separate auth subsystem.

Agents authenticate exactly like any other API client: with a bearer token
verified by [`RequireApiToken`](../../autumn/src/auth.rs). The `Authorization`
header an agent sends to `/mcp` is **forwarded** into the dispatched request,
so the call runs as that verified principal. The `Cookie` and `X-CSRF-Token`
headers are forwarded too, so session-based `#[secured]` routes and
CSRF-protected writes behave identically to a direct call.

To put a tool behind token auth, register the route inside a `scoped` group
carrying the `RequireApiToken` layer. The scope keeps the route in the
registry (so MCP can derive the tool) *and* applies the layer (so every call —
agent or HTTP — is checked):

```rust
use std::sync::Arc;
use autumn_web::auth::{InMemoryApiTokenStore, RequireApiToken};

let store = Arc::new(InMemoryApiTokenStore::default());

autumn_web::app()
    .scoped(
        "/api",
        RequireApiToken::new(store.clone()),
        routes![list_todos, create_todo], // handlers declared at "/todos"
    )
    .mount_mcp("/mcp")
    .run()
    .await;
```

A `tools/call` with no token is rejected by `RequireApiToken` and surfaces as
`isError: true`; the same call with a valid `Authorization: Bearer <token>`
header on the `/mcp` request succeeds. This protects the **tools** — but
`initialize`/`tools/list` (the catalog) are still reachable, since the
per-route layer only wraps the dispatched call, not the `/mcp` envelope.

### Gating the whole endpoint

To require a credential for the *entire* endpoint — catalog included — wrap it
with `secure_mcp`, passing any tower layer (e.g. `RequireApiToken`):

```rust
autumn_web::app()
    .routes(routes![list_todos, create_todo])
    .mount_mcp("/mcp")
    .secure_mcp(RequireApiToken::new(store.clone())) // gates initialize/tools/list too
    .run()
    .await;
```

### Why the `/mcp` endpoint sits outside the global middleware stack

The `/mcp` envelope (`initialize`/`tools/list`) is mounted **outside** the
app's global middleware — your `AppBuilder::layer(...)` layers and the
framework's CSRF/session middleware do not wrap it. This is deliberate and
matches how the MCP SDKs work:

- Forcing **CSRF/session** middleware onto a JSON-RPC `POST` would reject
  legitimate agent calls — MCP authenticates with bearer tokens/OAuth, not
  browser form tokens. No MCP SDK wraps the endpoint in browser middleware.
- The protections that *do* matter are provided the MCP-native way: **`Origin`
  validation** lives in the MCP layer (below), every **`tools/call`** runs the
  full per-route pipeline (so per-route auth/validation always applies), and
  **`secure_mcp(...)`** is the explicit opt-in to gate the whole endpoint —
  the analogue of `fastapi-mcp`'s `AuthConfig`.

If you want a global concern (auth, logging) to cover the envelope too, pass it
to `secure_mcp(layer)` rather than relying on `AppBuilder::layer`.

### Origin validation (DNS-rebinding protection)

The MCP Streamable-HTTP transport **requires** servers to validate the
`Origin` header so a malicious web page can't use a browser to reach a local
MCP server via DNS rebinding. Autumn enforces this automatically against your
CORS `allowed_origins`:

- A request with **no `Origin`** header (curl, SDKs, server-side agents) is
  allowed — non-browser callers aren't subject to DNS rebinding.
- A request whose `Origin` **isn't** in `cors.allowed_origins` (or `*`) gets
  **403 Forbidden** before any parsing or dispatch.

So to allow a browser-based MCP client from `https://app.example.com`, add that
origin to your CORS config; agent clients need no configuration.

---

## 7. The whole-API hatch

For internal tools or trusted agents you can expose every eligible **read**
endpoint at once, without tagging each one, via `expose_all_as_mcp()`:

```rust
autumn_web::app()
    .routes(routes![/* ... */])
    .expose_all_as_mcp()   // mounts at /mcp; chain mount_mcp("/path") to change it
    .run()
    .await;
```

This is an explicit, separate opt-in — never the default. It is deliberately
conservative:

- **`GET` endpoints are auto-included**, but **mutating verbs
  (`POST`/`PUT`/`PATCH`/`DELETE`) still require an explicit `#[api_doc(mcp)]`
  opt-in.** A write is never exposed implicitly, even under the hatch.
- **Per-endpoint exclusions are always honored.** Mark a route
  `#[api_doc(mcp = false)]` to keep it out, even under `expose_all_as_mcp()`.

---

## 8. Eligibility: JSON in, JSON out

Only JSON endpoints are eligible. Autumn detects this structurally: a route is
eligible when its handler has a JSON **response schema** (it returns
`Json<T>`). HTML/Maud routes have no response schema and are **auto-excluded**.

If you tag an HTML route with `#[api_doc(mcp)]`, it is skipped with a
build-time log note rather than a runtime surprise:

```text
WARN skipping MCP exposure: endpoint has no JSON response schema
     (HTML/Maud routes are not eligible as MCP tools)
     operation_id="dashboard" method="GET" path="/dashboard"
```

**Exception: `204 No Content`.** A route whose success status is `204` has no
response schema *by contract* — a deliberate empty success (like the
repository macro's generated `DELETE`), structurally distinct from an HTML
route's schema-less `200`. Such routes stay eligible; the tool result of a
successful call is empty text.

---

## 9. Repository CRUD tools

The `#[repository(Model, api = "/path")]` macro generates five CRUD routes.
Add the `mcp` key to expose them as tools — no hand-written handlers or
attributes needed:

```rust
// list/get/create/update/delete all become tools:
#[autumn_web::repository(Post, api = "/api/posts", policy = PostPolicy, mcp)]
pub trait PostRepository {}

// reads only — list and get:
#[autumn_web::repository(Post, api = "/api/posts", policy = PostPolicy, mcp = "read")]
pub trait PostRepository {}
```

- **Bare `mcp`** opts in all five operations. The usual safety annotations
  apply: `list`/`get` carry `readOnlyHint: true`, and `delete` carries
  `destructiveHint: true`, so agents and UIs can warn before destructive
  calls.
- **`mcp = "read"`** opts in only `list` and `get`.
- `mcp` requires `api = "/path"` (there are no routes to derive tools from
  otherwise) — the macro rejects it at compile time.
- Every tool call still dispatches through the real pipeline, so a
  `policy = ...` repository enforces the same record-level checks for an
  agent as for any HTTP client.

The `create`/`update` tools take a `body` argument referencing the generated
`New<Model>`/`Update<Model>` component schemas. By default those resolve to
placeholder object schemas; register the real schemas on your
`OpenApiConfig` (the same registration the OpenAPI document uses) to give
agents fully-typed inputs.

---

## 10. Plugins and route-level opt-in

Typed routes registered by a [plugin](../plugins.md) — via
`AppBuilder::routes()` or `scoped()` inside `Plugin::build` — flow into the
same route registry as your own, so a plugin route tagged `#[api_doc(mcp)]`
becomes a tool exactly like a user route.

More often, a plugin author doesn't want to hard-wire the decision. The
chainable `Route` toggles — `Route::mcp()`, `Route::mcp_exclude()`, and
`Route::mcp_stream()` — mirror the attribute forms at registration time, so
a plugin can offer a fluent switch and let the **host** decide at install
time:

```rust
use autumn_web::Route;

pub struct HarvestPlugin {
    expose_mcp: bool,
}

impl HarvestPlugin {
    #[must_use]
    pub fn expose_mcp(mut self) -> Self {
        self.expose_mcp = true;
        self
    }
}

impl Plugin for HarvestPlugin {
    fn build(self, app: AppBuilder) -> AppBuilder {
        let mut rs = routes![list_runs, create_run, signal_run];
        if self.expose_mcp {
            rs = rs.into_iter().map(Route::mcp).collect();
        }
        app.routes(rs)
    }
}
```

```rust
// Host app: the management API becomes MCP tools only because the host said so.
autumn_web::app()
    .plugin(HarvestPlugin::new().expose_mcp())
    .mount_mcp("/mcp")
    .run()
    .await;
```

The toggles follow the same semantics as the attributes: an explicit
`mcp()` exposes any verb, `mcp_exclude()` always wins (even over
`expose_all_as_mcp()`), and `mcp_stream()` implies the opt-in while
exempting an `Sse` route from the JSON-out gate. The flags are plain
`ApiDoc` metadata, so a plugin crate can set them while compiling against
base `autumn-web` — they take effect only when the host enables the `mcp`
feature and calls `mount_mcp`.

> **Limitation:** raw routers mounted via `nest()`/`merge()` are opaque to
> the route registry — Autumn cannot derive tools from them. Register typed
> routes if a plugin's endpoints should be MCP-exposable.

---

## 11. End-to-end example

`examples/todo-app` ships an `/mcp` endpoint. Its bearer-token JSON API is
mounted in a `scoped("/api", RequireApiToken, …)` group and tagged
`#[api_doc(mcp)]`, exposing a read tool (`list_json`), an explicitly-opted-in
write tool (`create_json`), and a **streaming** tool (`scan_json`, tagged
`#[api_doc(mcp, stream)]`) — all behind the same token auth a mobile client uses:

```rust
.scoped(
    "/api",
    RequireApiToken::new(Arc::new(deferred.clone())),
    routes![
        routes::api::list_json,
        routes::api::create_json,
        routes::api::scan_json, // streaming: Sse → notifications/progress
    ],
)
.mount_mcp("/mcp")
```

Run it, issue a token via `POST /api/tokens`, then `tools/list` and
`tools/call` against `/mcp` with that token in the `Authorization` header. Call
`scan_json` with a `_meta.progressToken` and an `Accept: text/event-stream`
header to watch progress frames arrive as the scan runs.

---

## 12. How a tool call is dispatched (and why it can't loop)

When a `tools/call` arrives, the MCP handler reconstructs an ordinary HTTP
request — filling the path template, building the query string, and attaching
the JSON body — forwards the `Authorization` header, and replays it through a
clone of the fully-assembled application router. Because it traverses the same
routes, layers, and middleware an external request would, security and
validation are *shared*, not re-implemented.

The dispatch target is a **snapshot of the router taken before the `/mcp`
route is merged in**. So the routing graph is acyclic by construction:

```text
agent → POST /mcp → serve_mcp → dispatch (a router that has no /mcp route)
                                   → your handler → JSON → tool result
```

A tool call resolves to a normal handler in one hop and returns; it can never
re-enter the MCP endpoint. (Tool paths are derived only from your route
registry and are never `/mcp` to begin with — the pre-merge snapshot upgrades
that from convention to a structural guarantee.)

> **Caveat — mount path collisions.** Autumn does not yet pre-check that your
> chosen `mount_mcp` path is free. If a real handler is already mounted at the
> same path, `axum` panics at startup on the duplicate route (loud and early,
> not a silent failure). Pick a path you don't otherwise serve — `/mcp` is the
> convention.

---

## 13. Scope and roadmap

This slice is **tools-only**. Tool results are buffered by default, with
**opt-in progressive streaming over SSE** (§5). The following remain **out of
scope** and are tracked as follow-ups:

- **Daemon lifetime** for long-running, session-surviving work —
  [#1119](https://github.com/madmax983/autumn/issues/1119).
- **Durable workflow tools** — exposing Harvest `#[workflow]`s as
  start/status/signal MCP tools on top of this layer
  ([autumn-harvest#597](https://github.com/madmax983/autumn-harvest)); the
  management API itself can already be exposed via the plugin route toggles
  (§10).
- **Tool declarations for raw `nest()`/`merge()` routers** — opaque routers
  carry no `ApiDoc`, so plugins must register typed routes to be
  MCP-exposable (§10).
- **MCP resources, prompts, and sampling** — this slice is tools-only.
- **stdio transport** — agents target deployed apps, so HTTP only for v1.
- **Non-JSON endpoints** (file upload/download, HTML).
- **LLM-assisted tool descriptions** — descriptions come from `#[api_doc]`;
  garbage-in/garbage-out is the author's call.

For the typed JSON-Schema derivation this builds on, see the OpenAPI support
in [`openapi.rs`](../../autumn/src/openapi.rs); for the in-process dispatch
path, see the [Testing guide](testing.md).
