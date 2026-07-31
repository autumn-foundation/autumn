# OpenAPI Spec Generation

Autumn derives an OpenAPI 3.1 document from the routes you already wrote. There
is no second source of truth: paths, path parameters, query structs, request
bodies, response bodies, tags, and security requirements all come from each
handler's signature and its route macro, so changing a handler's types changes
the spec in the same compile — provided the route macro sits outermost, since a
guard that expands first erases the type the generator would have read
([§3](#attribute-ordering-rules)). What a signature *cannot* express —
summaries, the success status code, custom tags — you add with
[`#[api_doc(...)]`](#3-enriching-operations-with-api_doc).

You add one builder call. Autumn mounts `GET /openapi.json` and a Swagger UI at
`/swagger-ui`, and regenerates the document on every request so
deprecation/sunset state never goes stale.

---

## 1. Turn it on

The spec types and the served endpoints live behind the `openapi` feature:

```toml
# Cargo.toml
[dependencies]
autumn-web = { version = "0.6", features = ["openapi"] }
```

Then hand `AppBuilder` an `OpenApiConfig`:

```rust
use autumn_web::openapi::OpenApiConfig;
use autumn_web::prelude::*;

#[get("/hello")]
async fn hello() -> &'static str { "hi" }

#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .routes(routes![hello])
        .openapi(OpenApiConfig::new("My API", "1.0.0"))
        .run()
        .await;
}
```

That mounts two endpoints:

| Path | Serves |
|------|--------|
| `GET /openapi.json` | The generated OpenAPI 3.1 document (`application/json`) |
| `GET /swagger-ui` | A Swagger UI page pointed at the JSON above |

Nothing is mounted unless you call `.openapi(...)` — an app that never asks for
a spec never serves one.

The Swagger UI assets are **vendored and served same-origin** from beneath the
UI path (`/swagger-ui/swagger-ui.css`, `/swagger-ui/swagger-ui-bundle.js`,
`/swagger-ui/swagger-initializer.js`). There is no CDN fetch, so the page works
offline and under a strict CSP.

### Config surface

```rust
let config = OpenApiConfig::new("My API", "1.0.0")
    .description("Everything the mobile client talks to")
    // Move the JSON document.
    .openapi_json_path("/v3/api-docs")
    // Move the UI, or pass `None` to serve JSON only.
    .swagger_ui_path(Some("/docs".to_owned()));
```

Both paths must be **static**: non-empty, starting with `/`, with no `//`, no
`*` wildcard, and no `{…}` capture (balanced or not). `/tenants/{id}/docs` is
rejected with `InvalidOpenApiPath` at boot even though it looks like an
ordinary route path — the docs endpoints mount at one fixed location.

Also avoid a **colon-prefixed segment** (`/:spec`, axum 0.7's old capture
syntax). `validate_route_path` does not screen for it, but axum 0.8's
`Router::route` panics on it during assembly, so you get a startup crash rather
than a named error. The MCP mount-path validator *does* reject colon segments
for this exact reason; the OpenAPI one has not caught up.

They must also differ from each other and from
any `GET` route Autumn already owns. A conflict is a `RouterBuildError`
(`OpenApiPathCollision` / a duplicate-path error) raised **before** any router
is mounted, naming both sides — not a mid-boot panic. The check covers your
`routes![]` handlers, scoped groups, framework `GET`s (probes, actuator, htmx
assets, dev live-reload), and `AppBuilder::nest` prefixes. See
[route collision diagnostics](./getting-started.md#route-collision-diagnostics).

> **One exception: `AppBuilder::merge`.** Axum does not expose a raw merged
> router's route table, so Autumn cannot introspect it. Merging a router that
> serves `GET /openapi.json`, the Swagger UI path, or one of its asset paths
> gets you a startup **panic**, not a clean `RouterBuildError` — the pre-mount
> check logs a `tracing::warn!` saying it was skipped rather than failing. If
> you use `merge`, pick OpenAPI paths you know its handlers don't claim.

---

## 2. What Autumn infers from a handler

The route macros (`#[get]`, `#[post]`, `#[put]`, `#[patch]`, `#[delete]`) build
an `ApiDoc` at compile time from the path and the handler signature:

| Source | Becomes |
|--------|---------|
| Route path `/users/{id}` | An operation under that path, plus a required `path` parameter per `{...}` segment (typed `string`) |
| HTTP method | The `get`/`post`/`put`/`patch`/`delete` key of the path item |
| Handler function name | The default `operationId` |
| First non-parameter path segment | The default tag (`/api/articles` → `api`) |
| `Query<T>` argument | One optional query parameter with `style: form, explode: true`, so `T`'s fields serialize as independent keys (`?q=foo&page=2`) |
| `Json<T>` or `Valid<Json<T>>` argument | A required `application/json` request body referencing `T`'s schema |
| `Json<T>` return, including `Result<Json<T>, _>` / `AutumnResult<Json<T>>` and tuples like `(StatusCode, Json<T>)` | The success response body |
| `Vec<T>` in either position | `type: array` with `items` from `T` |
| `Option<T>` in either position | Nullable — `type: ["string", "null"]` for primitives, `oneOf: [{$ref}, {type: "null"}]` for refs |
| `String`/`str`, `bool`, `i8`–`i64`, `u8`–`u64`, `isize`/`usize`, `f32`/`f64` | Inline primitive schemas, never a `$ref`. **`i128`/`u128` are not in the list** — they fall through to a named `$ref` and land on the object placeholder |
| Anything else named | A `$ref` into `#/components/schemas/…` (see [§4](#4-component-schemas)) |

Only these shapes are recognized deliberately: an unknown wrapper type is left
alone rather than guessed at, because a wrong schema is worse than an absent
one. A handler returning `impl IntoResponse`, `Markup`, `Sse`, or a redirect
contributes an operation with no response body schema.

Several places where the generated document can describe a request the handler
will not accept. None of them fails the build:

> **`Query<T>` must be flat — scalars only.** The `style: form, explode: true`
> mapping is accurate only while every field of `T` is a scalar. `Query<T>`
> decodes with `serde_urlencoded`, which handles neither nested objects nor
> **sequences**: a `Vec<String>` field advertised as an array is sent by a
> conforming client as `?tags=a&tags=b` and fails to deserialize with
> `invalid type: string "a", expected a sequence`. Take anything but scalars as
> a JSON body.
>
> The MCP projection notices the nested-object case and logs a build-time
> `tracing::warn`, but it still publishes the tool — so it appears in
> `tools/list` and fails when an agent calls it. Neither output refuses to
> generate; both only warn at best.

> **The query parameter is always `required: false`.** That flag is emitted
> unconditionally, whatever `T` looks like. If `T` has a non-`Option` field, a
> client that follows the spec and omits the query string entirely gets a
> deserialization failure. Make genuinely-optional query fields `Option<T>`,
> and say so in the operation's `description` when the query is in fact
> mandatory.

> **Path parameters are always untyped strings.** Every `{…}` segment is
> emitted as `type: string`; the generator never looks at the `Path<T>` in the
> signature. So `#[get("/users/{id}")]` with `Path<i64>` advertises a parameter
> that accepts `abc`, and extraction rejects it at request time. Note the
> constraint in the operation's `description` when it matters to callers.

> **Only the first `Query<T>` is documented.** `infer_query_params` stops at
> the first one it finds, so a handler taking `(Query<A>, Query<B>)` documents
> only `A` — fields that `B` requires are absent from the contract while
> extraction still demands them.

> **Catch-all routes do not survive the translation.** Axum's
> `#[get("/files/{*rest}")]` reaches the document with its path template
> unchanged and a path parameter literally named `*rest`. OpenAPI has no
> slash-spanning parameter, so a generated client cannot reliably call
> `/files/a/b`. (The MCP projection strips the `*`; the OpenAPI side does not.)
> Treat catch-all routes as undocumentable — `#[api_doc(hidden)]` if the noise
> matters.

### A worked example

```rust
use autumn_web::openapi::OpenApiSchema;
use autumn_web::prelude::*;

#[derive(serde::Serialize, serde::Deserialize, OpenApiSchema)]
struct Article {
    id: i64,
    title: String,
    body: String,
    tags: Vec<String>,
}

#[derive(serde::Serialize, serde::Deserialize, OpenApiSchema)]
struct NewArticle {
    title: String,
    body: String,
    draft: Option<bool>,
}

#[derive(serde::Deserialize, OpenApiSchema)]
struct ArticleQuery {
    q: Option<String>,
    page: Option<i64>,
}

#[get("/api/articles")]
#[api_doc(summary = "List articles", tag = "articles")]
async fn list(_query: Query<ArticleQuery>) -> Json<Vec<Article>> { /* … */ }

#[post("/api/articles")]
#[api_doc(summary = "Create an article", tag = "articles", status = 201)]
async fn create(Json(body): Json<NewArticle>) -> (StatusCode, Json<Article>) { /* … */ }
```

`GET /openapi.json` then contains (abridged — the shared error responses of
[§5](#5-errors-the-shared-problemdetails-contract) are omitted here):

```jsonc
{
  "openapi": "3.1.0",
  "info": { "title": "Blog API", "version": "1.0.0" },
  "paths": {
    "/api/articles": {
      "get": {
        "operationId": "list",
        "summary": "List articles",
        "tags": ["articles"],
        "parameters": [
          {
            "name": "ArticleQuery",
            "in": "query",
            "required": false,
            "schema": { "$ref": "#/components/schemas/ArticleQuery" },
            "style": "form",
            "explode": true
          }
        ],
        "responses": {
          "200": {
            "description": "OK",
            "content": {
              "application/json": {
                "schema": {
                  "type": "array",
                  "items": { "$ref": "#/components/schemas/Article" }
                }
              }
            }
          }
        }
      },
      "post": {
        "operationId": "create",
        "summary": "Create an article",
        "tags": ["articles"],
        "requestBody": {
          "required": true,
          "content": {
            "application/json": {
              "schema": { "$ref": "#/components/schemas/NewArticle" }
            }
          }
        },
        "responses": {
          "201": { "description": "Created", "content": { /* Article */ } }
        }
      }
    }
  }
}
```

> **Gotcha — the status code is not read from your return type.** A handler
> returning `(StatusCode::CREATED, Json<T>)` still documents `200` unless you
> write `#[api_doc(status = 201)]`. The tuple tells the generator *what* the
> body is, not which status carries it.

---

## 3. Enriching operations with `#[api_doc(...)]`

Everything a signature cannot express goes in `#[api_doc(...)]`:

| Key | Type | Effect |
|-----|------|--------|
| `summary` | string | One-line summary |
| `description` | string | Longer prose |
| `tag` | string | Single tag (replaces the path-segment default) |
| `tags` | `[string, …]` | Multiple tags |
| `operation_id` | string | Override the default (the function name) |
| `status` | integer | Success status code (default `200`) |
| `hidden` | flag / bool | Omit the route from the spec entirely |
| `mcp` | flag / bool | Also expose the endpoint as an MCP tool (`mcp = false` force-excludes) — see [the MCP guide](./mcp.md) |
| `stream` | flag / bool | Mark an `Sse`-returning MCP tool as streaming |

Unknown keys are a compile error naming the supported set, so a typo never
silently produces an undocumented operation.

```rust
#[get("/users/{id}")]
#[api_doc(
    summary = "Fetch a user by id",
    description = "Returns 404 when the id does not exist or the caller may not see it.",
    tags = ["users", "public"],
    operation_id = "getUserById",
)]
async fn get_user(Path(id): Path<i64>) -> AutumnResult<Json<User>> { /* … */ }
```

### Attribute ordering rules

- `#[api_doc]` may sit **above or below** `#[get]`/`#[post]`/`#[put]`/
  `#[delete]`/`#[patch]` — both orders work, including the fully-qualified
  `#[autumn_web::get(...)]` form.
- Put the **route macro outermost**, above `#[secured]`, `#[throttle]`,
  `#[step_up]`, and `#[authorize]`. Those guards rewrite the handler's return
  type to `Response` when they expand first, which erases the `Json<T>` the
  route macro needed to infer the response schema. Enforcement is correct
  either way; only the documented schema is lost.
- `#[static_get]` and `#[ws]` build their metadata directly and never read
  `#[api_doc(...)]` — an attribute attached to them is silently discarded, and
  `#[ws]` routes stay hidden from the spec.
- `#[api_doc]` above `#[oauth2_callback]` is dropped (that name is not in the
  route-attribute recognizer); put it below.

The full attribute-expansion story lives in
[macro transparency](./macro-transparency.md#api_doc).

---

## 4. Component schemas

A referenced type resolves to `#/components/schemas/<Key>` from one of three
sources, and the generator fills the document in this order:

1. **`OpenApiConfig::register_schema(key, json)`** — schemas you register
   explicitly, seeded first.
2. **Route-supplied schemas** — a route may carry a registration hook.
   `#[repository(api = "/api/…")]` uses one to document its generated list
   endpoint's pagination request (`PageRequest` / `CursorRequest`) and response
   envelope (`<Model>Page` / `<Model>CursorPage`).
3. **`#[derive(OpenApiSchema)]`** — the derive registers the struct in a
   compile-time inventory, and the generator back-fills any referenced type
   that nothing above already registered. This is the only zero-wiring path.

A referenced type that none of the three covers becomes the placeholder
`{"type": "object", "title": "<Key>"}`. The endpoint is still documented; only
its field list is missing.

> **Gotcha — implementing `OpenApiSchema` is not the same as registering it.**
> Only the *derive* feeds the back-fill inventory. A hand-written
> `impl OpenApiSchema` is never consulted while building the document — and
> that includes the impls `#[model]` generates for a model and its `New*` /
> `Update*` companions. A handler returning `Json<Bookmark>` therefore
> documents `Bookmark` as a placeholder until you say so explicitly:
>
> ```rust
> use autumn_web::openapi::OpenApiSchema;
>
> OpenApiConfig::new("Bookmarks API", "1.0.0")
>     .register_schema("Bookmark", <Bookmark as OpenApiSchema>::schema())
>     .register_schema("NewBookmark", <NewBookmark as OpenApiSchema>::schema())
> ```
>
> The key is the component key the `$ref` uses — the model's short type name in
> the ordinary, no-collision case.

```rust
use autumn_web::openapi::OpenApiSchema;

#[derive(serde::Deserialize, OpenApiSchema)]
#[serde(rename_all = "camelCase")]
struct ReportQuery {
    from_date: String,      // documented as `fromDate`, required
    to_date: String,        // documented as `toDate`, required
    include_drafts: Option<bool>,  // `includeDrafts`, optional
}
```

The derive mirrors the schema `#[model]` builds: each named field becomes a
property, every non-`Option` field is `required`, `Vec<T>` becomes an array,
`Option<T>` becomes nullable, and a container `#[serde(rename_all = "…")]` or
field-level `#[serde(rename = "…")]` is honored so property names match the
wire format. It rejects generic types and non-struct / tuple-struct inputs with
a clear compile error — use a manual `impl OpenApiSchema` plus
`register_schema` for those.

The rename handling is exact for the ordinary symmetric attributes. The **split
form** — `#[serde(rename_all(serialize = "kebab-case", deserialize =
"camelCase"))]`, and its field-level equivalent — is the exception: the schema
takes the *serialize* side, while `Query<T>` and `Json<T>` accept the
*deserialize* side. On a request type that would advertise a key the handler
will not accept, so name request fields with a symmetric `rename` /
`rename_all`, or register the schema by hand.

> **"Field-accurate" means the fields, not every serde attribute.** Renaming is
> the only serde behavior the derive interprets. It emits *every* named field
> and marks *every* non-`Option` field `required`, so these diverge from the
> real wire shape:
>
> | Attribute | What the schema says | What actually happens |
> |---|---|---|
> | `#[serde(default)]` | `required` | The field may be omitted |
> | `#[serde(skip)]` / `skip_deserializing` | Present as a property | Never read from the wire |
> | `#[serde(skip_serializing_if = "…")]` | Always present | May be absent from the response |
> | `#[serde(flatten)]` | A nested object property | The inner fields are inlined |
>
> Reach for `register_schema` when a type uses these — and note that a manual
> registration is only as accurate as the JSON you hand it; nothing checks it
> against the type.

For a type you cannot annotate (a foreign type, or a payload whose JSON shape
differs from its Rust shape):

```rust
OpenApiConfig::new("My API", "1.0.0").register_schema(
    "Money",
    serde_json::json!({
        "type": "object",
        "required": ["amount_cents", "currency"],
        "properties": {
            "amount_cents": { "type": "integer" },
            "currency": { "type": "string", "minLength": 3, "maxLength": 3 },
        },
    }),
)
```

### Component keys and collisions

A schema's component key is its short type name (`Article`). When two
*different* types share that last segment — the classic `create::Args` versus
`update::Args` — Autumn qualifies each with the fewest trailing module segments
that disambiguate them (`create.Args`, `update.Args`), deterministically and
independently of link order. In the common no-collision case the key stays the
plain short name.

Nested `$ref`s inside a schema body are rewritten to those same keys — but only
where the `$ref` target is a **type identity** (the full `type_name`), which is
what `#[derive(OpenApiSchema)]` emits. A body you hand to `register_schema`
yourself is copied verbatim: its `$ref`s are matched against the identity graph,
find nothing, and stay as written. That is fine while a short key is
unambiguous, and wrong the moment it isn't — if `create::Args` and
`update::Args` collide, the real components become `create.Args` / `update.Args`
while your hand-written `#/components/schemas/Args` keeps pointing at a key
nothing defines. **In a manually registered schema, write the
collision-resolved key you actually want.**

---

## 5. Errors: the shared `ProblemDetails` contract

Autumn returns [Problem Details](https://www.rfc-editor.org/rfc/rfc9457) bodies
(RFC 7807, obsoleted by RFC 9457) for errors, so every operation is documented
with the same error responses — `400`, `401`, `403`, `404`, `409`, `413`, `415`, `422`, `500`, and
`503` — each as `application/problem+json` with
`$ref: "#/components/schemas/ProblemDetails"`. That component is registered
once and describes the real payload: `type`, `title`, `status`, `detail`,
`instance`, `code` (matching `^autumn\.[a-z0-9_]+$`), `request_id`, and a
per-field `errors` array.

You do not declare these, and a generated client gets one error type for the
whole API rather than a bespoke shape per endpoint. They are only filled in for
status codes the operation does not already document, so an operation whose
success status is one of them (`#[api_doc(status = 409)]`, say) keeps your
declaration.

> **The contract covers framework errors, and the document does not say so.**
> `AutumnError` is what produces a `problem+json` body. A handler returning
> `Result<Json<T>, E>` for its own `E: IntoResponse` emits whatever `E` emits —
> plain text, a bespoke JSON shape, any media type — and nothing normalizes it
> on the way out. The route macro reads only the `Ok` side, so those ten
> responses are still advertised on that operation, and a generated client will
> try to parse your custom error as `ProblemDetails`. Return `AutumnError`
> (or convert into it) on any endpoint whose spec a client consumes.

---

## 6. Security schemes

An auth guard on the handler is what puts security requirements in the spec —
the generator never guesses from a path prefix.

| Handler | Documented as |
|---------|---------------|
| `#[secured]` or `#[secured("admin")]` | `SessionAuth` — an `apiKey` scheme in the session cookie |
| `#[secured(scopes = ["reports:read"])]` | `BearerAuth` — HTTP bearer, plus `x-required-scopes` |
| `#[secured("admin", scopes = ["reports:read"])]` | Both, in one requirement object (an AND) |
| `#[authorize(...)]` with no `#[secured]` | `SessionAuth`, with no roles or scopes — but see the warning below |
| No guard at all | No `security` entry; if no route in the app is guarded, no `securitySchemes` block at all |

> **`#[authorize]` alone does not require authentication.** The generator
> records `SessionAuth` for a policy-guarded route, but that is *documentation,
> not enforcement*. `PolicyContext` carries an **optional** `user_id`, and the
> authorize path simply asks `policy.can(...)` — so a policy that returns
> `true` admits an anonymous caller, and a service principal authorizes on its
> token scopes with no session user at all. If the endpoint must have an
> authenticated caller, say so with `#[secured]`; do not read the spec's
> `SessionAuth` entry as proof that it does.

The `SessionAuth` cookie name is taken from your app's live
`session.cookie_name` config, so a renamed cookie is reflected in the document
without touching the OpenApiConfig. Required scopes also surface in the
`x-required-scopes` vendor extension, which is machine-readable for gateway or
CI checks that OpenAPI's own model cannot express.

Roles from `#[secured("role")]` are *not* enumerated in the spec (OpenAPI has no
vocabulary for them); document them in a `description` when they matter to
callers. To audit which routes are protected, use
[`autumn routes audit`](./routes-cli.md) and the
[security posture manifest](./security-posture-manifest.md).

---

## 7. Versions, deprecations, and sunsets

Routes tagged with `api_version = "v1"` carry that version as an extra tag, and
the generator consults the `ApiVersion` lifecycle you registered on the builder:

- past `deprecated_at` (or `sunset_at`) → the operation is marked
  `"deprecated": true`;
- a version with a `sunset_at` → a documented `410 Gone` response
  (`application/problem+json`), unless the route sets `sunset_opt_out`.

Because the document is rebuilt per request against the app's clock, an
operation starts advertising itself as deprecated the moment its date passes —
no redeploy, no stale artifact. Full lifecycle mechanics live in
[API versioning](./api-versioning.md).

---

## 8. Scoped route groups

Routes mounted under `.scoped("/api/v2", layer, routes![…])` are documented at
their **effective** URL: the scope prefix is joined onto each path, and any
`{param}` declared in the prefix is merged into that operation's path
parameters ahead of the route's own. The spec always shows the URL a client
actually calls.

---

## 9. Choosing what appears

Every route in `routes![]` (and in a scoped group) lands in the spec —
including HTML page handlers, which show up as operations with no JSON body.
`#[ws]` handlers are the exception: they set `hidden` themselves and never
appear.

That includes **pre-rendered pages**. A `#[static_get]` handler belongs in
`routes![]` *and* in `static_routes![]` — the pre-renderer drives the real
router, so a page listed only in `static_routes![]` is never mounted and
cannot be rendered — and being in `routes![]` is what puts it in the spec.
`examples/blog` registers its about page both ways for exactly this reason.
Note that `#[static_get]` builds its metadata directly and ignores
`#[api_doc(...)]` entirely, so `#[api_doc(hidden)]` will **not** take a
pre-rendered page out of the document.

Three ways to shape what remains:

- `#[api_doc(hidden)]` on routes that are not part of the API contract
  (internal endpoints, ordinary `#[get]` HTML pages you would rather not
  advertise). As above, this does not work on a `#[static_get]` page — that
  attribute is ignored there.
- Tags: give your JSON endpoints an explicit `tag`, so the UI groups them apart
  from page routes that inherit a path-segment tag.
- Path layout: keeping the API under `/api/...` gives every endpoint the same
  default tag for free.

---

## 10. Production posture

The spec and the UI are unauthenticated by default. Decide deliberately whether
your API contract is public.

**Profile gate.** `[openapi]` in `autumn.toml` gates the endpoints per profile,
without touching code:

```toml
# autumn-prod.toml
[openapi]
enabled = false
```

With `enabled = false`, neither `/openapi.json` nor `/swagger-ui` is mounted
even though `.openapi(...)` is still in `main.rs` — so dev and staging keep
the UI while production serves nothing. The *path* the spec is served at always
comes from `OpenApiConfig::openapi_json_path` in code; the `[openapi] path` key
does not relocate the endpoint.

**Under multi-tenancy**, the docs endpoints are deliberately not auto-exempted
from tenant resolution (their mounted path is not visible to the config layer).
If the spec should be reachable without a tenant, list it explicitly:

```toml
[tenancy]
public_paths = ["/openapi.json", "/swagger-ui"]
```

Other levers worth combining: keep the endpoints internal at the proxy/network
layer, or serve JSON only (`swagger_ui_path(None)`) and ship the UI elsewhere.

---

## 11. Exporting the spec

**At build time — if your app pre-renders.** `autumn build` (which runs the app
with `AUTUMN_BUILD_STATIC=1`) writes both `dist/openapi.json` and
`dist/openapi.yaml` next to the pre-rendered pages when `.openapi(...)` is
configured. That artifact is what you publish, feed to a client generator, or
diff between releases.

> **The spec export rides along with static generation.** `autumn build` bails
> out early — `No static routes registered. Nothing to build.` — before it
> reaches the OpenAPI writer, so an app with no `#[static_get]` routes gets no
> `dist/openapi.json` no matter how `.openapi(...)` is configured. A pure JSON
> API (`examples/bookmarks` among them) has to take the spec from the running
> endpoint instead.

**From a running app** — the path that always works, pre-rendering or not:

```bash
curl -fsS http://127.0.0.1:3000/openapi.json > openapi.json
npx @openapitools/openapi-generator-cli generate \
  -i openapi.json -g typescript-fetch -o ./client
```

**In CI**, the cheapest useful gate is a breaking-change diff: regenerate the
spec on the PR branch, compare it against the committed copy with an
OpenAPI-diff tool, and fail on removals. Because the spec is derived from
handler types, that diff catches contract breaks — a removed route, a changed
method or status, a parameter that vanished — as part of the normal build.

A diff can only see what the document describes, so **field-level** breaks are
caught exactly for the types that carry a field-accurate schema: anything with
`#[derive(OpenApiSchema)]` or an explicit `register_schema`. A type resolving
to the `{"type": "object", "title": "…"}` placeholder of
[§4](#4-component-schemas) — including a `#[model]` you have not registered —
has no fields in the spec at all, so renaming or dropping one changes nothing
for the diff to fail on. If you intend to lean on this gate, make sure the
types on your API boundary are not placeholders first.

---

## 12. Testing the spec

`TestApp` mounts the docs endpoints the same way the real router does, so the
document is assertable in an ordinary integration test:

```rust
use autumn_web::openapi::OpenApiConfig;
use autumn_web::test::TestApp;

#[tokio::test]
async fn articles_are_documented() {
    let client = TestApp::new()
        .routes(routes![list, create])
        .openapi(OpenApiConfig::new("Blog API", "1.0.0"))
        .build();

    let response = client.get("/openapi.json").send().await;
    response.assert_ok();

    let spec: serde_json::Value = response.json();
    assert_eq!(spec["openapi"], "3.1.0");
    assert_eq!(
        spec["paths"]["/api/articles"]["post"]["responses"]["201"]["description"],
        "Created",
    );
    // No schema silently degraded to the placeholder:
    assert!(spec["components"]["schemas"]["NewArticle"]["properties"]["title"].is_object());
}
```

For a spec-shaped assertion without booting a router, `generate_spec(&config,
&docs)` builds the document from `ApiDoc` values directly — that is how
Autumn's own [`openapi` integration
tests](../../autumn/tests/integration/openapi.rs) work.

---

## 13. The same metadata powers MCP

The `mcp` feature implies `openapi` and derives its tool catalog from the very
same `ApiDoc` values, through the same schema-resolution rules and the same
component keys. Tag a handler `#[api_doc(mcp)]`, call `mount_mcp("/mcp")`, and
an agent gets a typed tool whose `inputSchema` is derived from the same typed
contract the OpenAPI operation is. See
[Exposing your API as MCP tools](./mcp.md).

> **One shape doesn't survive the projection.** A tool's arguments are a single
> flat object: path parameters by name, plus the reserved keys `query` and
> `body` for the `Query<T>` extractor and the JSON body. A route whose *path
> parameter* is itself named `query` or `body` — `/items/{body}` with a
> `Json<T>` — loses that property to the reserved key, and the tool cannot
> rebuild the URL. The OpenAPI operation is still correct; only the MCP tool is
> unusable. Rename the path parameter if you expose such a route as a tool.

---

## Troubleshooting

| Symptom | Cause |
|---------|-------|
| A schema shows as `{"type": "object", "title": "X"}` | Nothing registered it — add `#[derive(OpenApiSchema)]`, or `register_schema` for a `#[model]` / hand-written impl ([§4](#4-component-schemas)) |
| A response body disappeared after adding `#[throttle]` / `#[secured]` | Guard expanded outermost and rewrote the return type — move the route macro above it |
| `#[api_doc]` had no effect | It is on a `#[static_get]`/`#[ws]` handler, above `#[oauth2_callback]`, or on a function with no route macro at all |
| The success status is `200` on a `201`-returning handler | Add `#[api_doc(status = 201)]` |
| Boot fails with an OpenAPI path collision | `openapi_json_path`/`swagger_ui_path` equals another `GET` route or each other |
| Startup *panics* on a duplicate route instead of erroring cleanly | The colliding handler came from `AppBuilder::merge`, which the pre-mount check cannot inspect ([§1](#1-turn-it-on)) |
| `/openapi.json` 404s in production | `[openapi] enabled = false` in that profile, or `.openapi(...)` was never called |
| `autumn build` wrote no `dist/openapi.json` | It printed `No static routes registered` and exited first — the export rides along with static generation ([§11](#11-exporting-the-spec)) |
| A query struct documents one opaque `object` parameter | Expected shape — `style: form, explode: true` means clients send its fields as individual keys |

---

## Where to go next

- [Exposing your API as MCP tools](./mcp.md) — the same metadata, projected for
  agents.
- [API versioning](./api-versioning.md) — `ApiVersion` lifecycles, deprecation
  and sunset headers, and the `410` documentation above.
- [Routes CLI](./routes-cli.md) and the
  [security posture manifest](./security-posture-manifest.md) — auditing which
  routes exist and which are protected.
- [Authentication](./authentication.md) and
  [authorization](./authorization.md) — what `#[secured]` documents in the spec
  actually enforces.
- [Macro transparency](./macro-transparency.md#api_doc) — the exact expansion,
  ordering rules, and generated `ApiDoc` literal.
- [Repositories](./repositories.md) — `#[repository(api = "/api/…")]` generates
  documented CRUD endpoints; see `examples/bookmarks` for a runnable app that
  serves its own spec.
- Rustdoc: [`autumn_web::openapi`](../../autumn/src/openapi.rs).
