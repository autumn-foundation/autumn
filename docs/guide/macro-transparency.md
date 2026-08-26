# Macro Transparency

Autumn relies on procedural macros to eliminate boilerplate. This guide shows
you exactly what those macros generate so there are no surprises at runtime.

Examples in this guide track the Autumn 0.7.x line and Rust 1.88.0+ as of
2026-07-10.

The code snippets are **illustrative**, not compiled doctests: the "what it
expands to" blocks are hand-written conceptual expansions (the real output has
more fully-qualified paths and hidden spans). To see the byte-for-byte truth,
run `cargo expand` (see below). The macros themselves are covered by unit tests
in `autumn-macros/src/*.rs` and by trybuild/integration tests.

---

## Contents

- [Startup Log: What Did Autumn Configure?](#startup-log-what-did-autumn-configure)
- [Using `cargo expand` to See Generated Code](#using-cargo-expand-to-see-generated-code)
- **Macro-by-Macro Expansion Reference**
  - [Routing & Handlers](#routing--handlers) — `#[get]`/`#[post]`/`#[put]`/`#[delete]`/`#[patch]`, `#[oauth2_callback]`, `routes![]`, `#[static_get]` + `static_routes![]`, `#[ws]`, `#[api_doc]`, `#[autumn_web::main]`
  - [Models](#models) — `#[model]` and its field-level attributes
  - [Repositories](#repositories) — `#[repository(Model)]` and its advanced surface
  - [Services](#services) — `#[service]`
  - [Background Work: Scheduled Tasks, Jobs, Events, Listeners, One-off Tasks](#background-work-scheduled-tasks-jobs-events-listeners-one-off-tasks) — `#[scheduled]` + `tasks![]`, `#[job]` + `jobs![]`, `#[event]`, `#[listener]` + `listeners![]`, `#[task]` + `one_off_tasks![]`, `#[cached]`
  - [Guards & Rate Limiting](#guards--rate-limiting) — `#[secured]`, `#[authorize]`, `#[step_up]`, `#[feature_flag]`, `#[throttle]`, `#[query_budget]`
  - [Mail](#mail) — `#[mailer]`, `#[mailer_preview]` + `mail_previews![]`, `#[inbound_mail]`
  - [i18n, Stories & Path Helpers](#i18n-stories--path-helpers) — `t!`, `story!`, `paths![]`
- [The Companion Function Pattern](#the-companion-function-pattern)
- [Debugging Macro Issues](#debugging-macro-issues)

---

## Startup Log: What Did Autumn Configure?

When your application starts, Autumn logs every decision it makes. A typical
startup sequence looks like this:

```
  INFO autumn: Autumn starting version="0.7.0" profile="dev"
  INFO autumn: Database pool configured max_connections=10
  INFO autumn: Registered task name="db_cleanup" schedule="every 5m"
  INFO autumn: Listening addr=127.0.0.1:3000
```

If you omit the database:

```
  INFO autumn: Autumn starting version="0.7.0" profile="dev"
  INFO autumn: Database not configured
  INFO autumn: Listening addr=127.0.0.1:3000
```

Every line tells you something Autumn decided on your behalf. No silent
configuration -- if the framework did it, it logged it.

### Full transparency mode: `--show-config`

For a complete dump of everything Autumn configured -- every route, every
scheduled task, every middleware layer, and all resolved configuration values
-- use the `--show-config` flag:

```bash
autumn dev --show-config
```

Or with `cargo run`:

```bash
AUTUMN_SHOW_CONFIG=1 cargo run
```

This produces output like:

```
  INFO autumn: Autumn starting version="0.7.0" profile="dev"
  INFO autumn: Registered routes:
    /            GET      -> index
    /todos       GET      -> list_todos
    /todos       POST     -> create_todo
    /todos/{id}  DELETE   -> delete_todo
    /health      GET      -> health
    /actuator/*  GET      -> actuator
  INFO autumn: Scheduled tasks:
    cleanup (every 300s)
  INFO autumn: Active middleware: RequestId, SecurityHeaders, Session (in-memory), CORS, Metrics
  INFO autumn: Configuration:
    profile:    dev
    server:     127.0.0.1:3000
    database:   localhost/mydb (pool_size=10)
    log_level:  debug
    log_format: Pretty
    health:     /health (detailed=true)
    actuator:   sensitive=true
    shutdown:   1s
  INFO autumn: Database pool configured max_connections=10
  INFO autumn: Listening addr=127.0.0.1:3000
```

Database passwords are masked in the output. The log shows the fully resolved
configuration after all 5 layers have been merged, so you can verify that your
env vars, profile overrides, and TOML settings are all taking effect.

### What happens at startup (step by step)

1. **Load configuration** -- 5-layer merge (defaults → profile smart defaults
   → `autumn.toml` → `autumn-{profile}.toml` → `AUTUMN_*` env vars)
2. **Initialize logging** -- format and level come from the merged config
3. **Validate routes** -- panics immediately if no routes are registered
4. **Log banner** -- version and active profile
5. **Create database pool** -- or log "Database not configured" if no URL
6. **Run migrations** -- if `.migrations()` was called and a DB URL exists
7. **Build router** -- mount routes, middleware, static file serving
8. **Start scheduled tasks** -- log each task name and schedule
9. **Bind and listen** -- log the address

---

## Using `cargo expand` to See Generated Code

The most direct way to see what Autumn's macros produce is `cargo expand`.

### Install

```bash
cargo install cargo-expand
```

### Expand a single file

```bash
# Expand your entire crate
cargo expand

# Expand a specific module
cargo expand routes::todos
```

### Tips for readable output

- **Pipe through `rustfmt`** for formatting:
  ```bash
  cargo expand routes::todos | rustfmt
  ```
- **Redirect to a file** to search at your own pace:
  ```bash
  cargo expand > expanded.rs
  ```
- **Search for `__autumn_`** -- all generated companion functions use this
  prefix, making them easy to find in the expanded output.

---

## Macro-by-Macro Expansion Reference

The macros below are grouped by concern. Every subsection follows the same
shape: the macro syntax, *what you write*, *what it expands to (conceptually)*,
its options, and gotchas.

---

## Routing & Handlers

### `#[get("/path")]`, `#[post(...)]`, `#[put(...)]`, `#[delete(...)]`, `#[patch(...)]`

Your handler function is kept unchanged. The macro adds a hidden companion
function that returns route metadata. All five share one implementation
(`route::route_macro`), differing only in the HTTP method.

**You write:**

```rust
#[get("/hello")]
async fn hello() -> &'static str {
    "Hello!"
}
```

**The macro generates (alongside your function):**

```rust
pub fn __autumn_route_info_hello() -> ::autumn_web::route::Route {
    ::autumn_web::route::Route {
        method: ::http::Method::GET,
        path: "/hello",
        handler: ::axum::routing::get(hello),
        name: "hello",
        // ...plus OpenAPI `api_doc` metadata inferred from the signature
    }
}
```

`#[patch("/items/{id}")]` is identical, emitting `::http::Method::PATCH` and
`::axum::routing::patch(...)`.

If you add `#[intercept(MyLayer)]`, the handler is wrapped with `.layer()`:

```rust
handler: ::axum::routing::get(hello).layer(MyLayer),
```

### `#[oauth2_callback("/path")]`

A convenience alias for `#[get(...)]`, intended for OAuth2/OIDC callback
endpoints like `/auth/github/callback`. It calls the exact same
`route::route_macro("GET", ...)`, so the expansion and companion
(`__autumn_route_info_{name}`) are identical to `#[get]` — the different name
is purely to signal intent at the call site.

**`#[api_doc]` ordering caveat.** The expansion matches `#[get]`, but the *name*
`oauth2_callback` is **not** in `#[api_doc]`'s route recognizer
(`ROUTE_ATTR_NAMES`, which lists only `get`/`post`/`put`/`delete`/`patch`/
`static_get`/`ws`). So the [flexible `#[api_doc]` ordering](#api_doc) does **not**
apply here: an `#[api_doc]` placed *above* `#[oauth2_callback]` is treated as
standalone and silently stripped, and its OpenAPI metadata is lost. Place
`#[api_doc]` **below** `#[oauth2_callback]`, where the expanded GET route
consumes it:

```rust
#[oauth2_callback("/auth/github/callback")]
#[api_doc(summary = "GitHub OAuth callback", tag = "auth")]
async fn github_callback(/* ... */) { /* ... */ }
```

### `routes![handler_a, handler_b]`

Transforms a list of handler names into a `Vec<Route>` by calling each
companion function.

**You write:**

```rust
let all = routes![hello, create_todo];
```

**Expands to:**

```rust
let all = vec![
    __autumn_route_info_hello(),
    __autumn_route_info_create_todo(),
];
```

Module-qualified paths work: `routes![users::list, posts::create]` calls
`users::__autumn_route_info_list()` and `posts::__autumn_route_info_create()`.

`#[ws]` and `#[static_get]` handlers also produce a `__autumn_route_info_*`
companion, so they can be listed in `routes![]` alongside plain route handlers.

### `#[static_get("/path")]` + `static_routes![]`

Generates both a route companion (same as `#[get]`) and a static metadata
companion for build-time rendering.

**You write:**

```rust
#[static_get("/about")]
async fn about() -> Markup {
    html! { h1 { "About" } }
}
```

**Generates two companions:**

```rust
// Route (same as #[get])
pub fn __autumn_route_info_about() -> Route { ... }

// Static build metadata
pub fn __autumn_static_meta_about() -> StaticRouteMeta {
    StaticRouteMeta {
        path: "/about",
        name: "about",
        revalidate: None,
        params_fn: None,
    }
}
```

Collect the static metadata with `static_routes![about, ...]`, which calls the
`__autumn_static_meta_*` companions (same collector pattern as `routes![]`).

### `#[ws("/path")]`

A WebSocket upgrade route built on a **two-function** pattern: your outer
function runs at upgrade time (with normal extractors) and returns a value
implementing `WsHandler` that owns the live socket.

**You write:**

```rust
#[ws("/echo")]
async fn echo(state: AppState) -> impl WsHandler {
    |mut socket: WebSocket| async move {
        while let Some(Ok(msg)) = socket.recv().await {
            let _ = socket.send(msg).await;
        }
    }
}
```

**Generates (alongside your function):**

```rust
// 1. Upgrade handler: extracts the upgrade + State<AppState> (+ any of your
//    non-AppState params as extractors), calls your fn, then upgrades.
#[doc(hidden)]
async fn __autumn_ws_upgrade_echo(
    __autumn_ws: ::autumn_web::ws::WebSocketUpgrade,
    State(__autumn_state): State<::autumn_web::AppState>,
) -> impl IntoResponse {
    let __autumn_shutdown = __autumn_state.shutdown_token();
    let handler = echo(__autumn_state.clone()).await;
    __autumn_ws.on_upgrade(move |socket| async move {
        ::autumn_web::ws::WsHandler::handle(handler, socket, __autumn_shutdown).await;
    })
}

// 2. Route companion (registered as a GET upgrade) so routes![] just works.
#[doc(hidden)]
fn __autumn_route_info_echo() -> ::autumn_web::Route { /* method GET, hidden from OpenAPI */ }
```

- `AppState` parameters are supplied directly from the extracted app state;
  every other parameter becomes an Axum extractor on the upgrade handler.
- **Gotcha:** WebSocket routes are hidden from the generated OpenAPI spec by
  default (there is no meaningful JSON body). Register schemas manually via
  `OpenApiConfig::register_schema` if you need to document them.
- **Gotcha:** `#[ws]` does *not* accept the per-route `timeout_ms` / `timeout =
  "off"` attributes that `#[get]` etc. support. The inbound timeout only bounds
  the pre-upgrade handshake (`RouteTimeout::Inherit`); the established socket
  future runs on a separate task and is never polled under the deadline. Bound a
  slow handshake with `tokio::time::timeout` inside the upgrade handler instead.

### `#[api_doc(...)]`

Enriches a route's auto-generated OpenAPI documentation with fields that can't
be inferred from the signature. It does **not** stand alone: it folds its
metadata into the *paired* route macro's `ApiDoc`. Applied without a route
macro, it is a no-op. For what ends up in the served document, see the
[OpenAPI guide](openapi.md).

**You write:**

```rust
#[get("/users/{id}")]
#[api_doc(summary = "Fetch a user by id", tag = "users")]
async fn get_user(Path(id): Path<i32>) -> Json<User> {
    // ...
}
```

**Effect:** the `#[get]` companion's `ApiDoc { summary, tags, ... }` is
populated from the `#[api_doc]` keys instead of the defaults. The attribute is
consumed; nothing is left on the function.

| Key | Type | Effect |
|-----|------|--------|
| `summary` | string | Short one-line description |
| `description` | string | Longer multi-line description |
| `tag` | string | Single OpenAPI tag for grouping |
| `tags` | `[string, ...]` | Multiple OpenAPI tags |
| `operation_id` | string | Override the default operation id |
| `status` | integer | Success HTTP status code (defaults to `200`) |
| `hidden` | flag / bool | Exclude the route from the generated spec |
| `mcp` | flag / bool | Expose this endpoint as an MCP tool (`mcp = false` force-excludes it). Requires the `mcp` feature and a `mount_mcp` call. |
| `stream` | flag / bool | Mark an `Sse`-returning MCP tool as streaming (`#[api_doc(mcp, stream)]`). A streaming route has no JSON response schema, so without this flag an `mcp`-exposed `Sse` route is skipped as schema-less; `stream` exempts it from that gate and advertises it as a streaming tool. |

- **Ordering is flexible — with one exception:** `#[api_doc]` works whether it
  sits *above* or *below* the built-in route macros. Rust expands attribute
  macros outermost-first, so when `#[api_doc]` runs first it detects the pending
  route attribute and hands the metadata through rather than stripping itself.
  The recognizer (`ROUTE_ATTR_NAMES` / `is_route_attribute`) covers only
  `get`/`post`/`put`/`delete`/`patch`/`static_get`/`ws` — **not**
  `#[oauth2_callback]`. Because that name is unrecognized, an `#[api_doc]` placed
  *above* `#[oauth2_callback]` is treated as standalone and silently dropped;
  put it *below* the callback instead (see
  [`#[oauth2_callback]`](#oauth2_callbackpath)).
- **Only the standard HTTP route macros actually consume the metadata.** Being
  listed in `ROUTE_ATTR_NAMES` only stops `#[api_doc]` from dropping *itself* as
  standalone — it does **not** guarantee the paired macro reads the keys. Only
  `#[get]`/`#[post]`/`#[put]`/`#[delete]`/`#[patch]` call `api_doc::extract` (in
  `route.rs`) and fold the fields into their `ApiDoc`. `#[static_get]` and
  `#[ws]` build their `ApiDoc` literal directly and never call
  `api_doc::extract`, so an `#[api_doc(...)]` attached to a static route or
  WebSocket handler is **silently discarded regardless of ordering** — the
  OpenAPI entry keeps its defaults (and `#[ws]` stays `hidden`). Document those
  endpoints with `OpenApiConfig::register_schema` instead.

### `#[autumn_web::main]`

Sets up the Tokio runtime and framework environment variables.

**You write:**

```rust
#[autumn_web::main]
async fn main() {
    autumn_web::app().routes(routes![index]).run().await;
}
```

**Expands to:**

```rust
fn main() {
    autumn_web::config::__set_macro_context(
        env!("CARGO_MANIFEST_DIR").to_string(),
        cfg!(debug_assertions),  // true in debug, false in release
    );

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async move {
            autumn_web::app().routes(routes![index]).run().await;
        });
}
```

---

## Models

### `#[model]`

Generates Diesel derives, an insert struct, an update struct with `Patch<T>`
fields, a field enum, and a draft extension trait.

**You write:**

```rust
#[model(table = "posts")]
pub struct Post {
    #[id]
    pub id: i64,
    #[validate(length(min = 1))]
    pub title: String,
    #[default]
    pub published: bool,
}
```

**Generates these types:**

```rust
// 1. Query struct -- your original struct with Diesel derives
#[derive(Queryable, Selectable, AsChangeset, Serialize, Deserialize)]
#[diesel(table_name = posts)]
pub struct Post {
    pub id: i64,
    pub title: String,
    pub published: bool,
}

// 2. Insert struct -- #[id] and #[default] fields excluded
#[derive(Insertable, Serialize, Deserialize)]
#[diesel(table_name = posts)]
pub struct NewPost {
    #[validate(length(min = 1))]
    pub title: String,
}

// 3. Update struct -- all mutable fields wrapped in Patch<T>
#[derive(Serialize, Deserialize, Default)]
pub struct UpdatePost {
    #[serde(default)]
    pub title: Patch<String>,
}

// 4. Field enum
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum PostField {
    Title,
}

// 5. Draft extension trait (for mutation hooks)
pub trait PostDraftExt {
    fn from_patch(current: &Post, patch: &UpdatePost) -> AutumnResult<UpdateDraft<Post>>;
    fn title(&mut self) -> DraftField<'_, String>;
}
```

### `#[model]` field-level attributes

Beyond `#[id]`, `#[default]`, `#[validate(...)]`, `#[indexed]`, and
`#[lock_version]`, `#[model]` recognizes a set of framework field attributes.
These are stripped from the emitted Diesel query struct (they'd confuse the
derives) and instead drive extra generated code. The full recognized set is
`id`, `indexed`, `validate`, `default`, `factory_assoc`, `lock_version`,
`searchable`, `encrypted`, `private`, `normalize`, and `state_machine`. The
association attributes `belongs_to` / `has_many` / `has_one` are **not**
field-level — they are struct-level attributes placed *above* the struct (see
[Associations and search keys](#associations-and-search-keys) below).

#### `#[private]`

Excludes the field from the model's `Serialize` impl (JSON responses) while
keeping it a normal, queryable Rust field mapped to its column. Concretely it
adds `#[serde(skip_serializing)]` to the generated query struct (leaving
`Deserialize` intact). The write path (`New*` / `Update*` / changeset) is
unaffected, so a client can still *set* the value while never *reading* it back.

**`#[private]` does *not* redact `Debug`.** A field that is *only* `#[private]`
still prints verbatim in `{:?}` output, so it can leak into logs, panic
backtraces, and framework error messages. The redacting `Debug` impl is emitted
only for models that have at least one `#[encrypted]` field — mark a field
`#[encrypted]` (not just `#[private]`) when it must also stay out of `Debug`
output.

```rust
#[model(table = "users")]
pub struct User {
    #[id] pub id: i64,
    pub email: String,
    // Hidden from JSON responses, but still printed by `Debug`. Fine for a
    // non-secret internal field; for an actual secret (e.g. `password_hash`)
    // reach for `#[encrypted]` so it is redacted from `Debug` too.
    #[private] pub internal_notes: String, // still SELECTed & filterable, never serialized
}
```

**Gotcha:** `#[private]` controls JSON serialization only — it emits
`#[serde(skip_serializing)]` and nothing more. It does **not** redact `Debug`,
and the column is still selected and can be used in `find_by_*` queries — it is
not a column-level access control. For a value that must also stay out of
`Debug`/log output, use `#[encrypted]`.

#### `#[encrypted]` / `#[encrypted(deterministic | randomized, admin_visible, versioned_ciphertext)]`

Marks a `String` column as encrypted at rest (application-level AEAD).

- **`#[encrypted]`** (bare) — **randomized** AEAD (the default). Ciphertext is
  non-deterministic, so equality lookups on the column are impossible.
- **`#[encrypted(deterministic)]`** — stable ciphertext for the same plaintext,
  which *enables* equality filters (`find_by_*`) at the cost of leaking equality
  (identical plaintexts share ciphertext).
- **`admin_visible`** — decrypt the value in admin views and opt it back into
  JSON serialization (encrypted fields are hidden from JSON by default).
- **`versioned_ciphertext`** — store encrypted before/after ciphertext in the
  version-history record (requires `versioned` on the repository).

The generated `Debug` is redacting, and an `#[encrypted]` field is hidden from
JSON unless `admin_visible` is set. Options are validated at compile time
(`deterministic`, `randomized`, `admin_visible`, `versioned_ciphertext`).

```rust
#[model(table = "customers")]
pub struct Customer {
    #[id] pub id: i64,
    #[encrypted(deterministic)] pub tax_id: String, // equality-searchable
    #[encrypted] pub notes: String,                 // randomized, no lookups
}
```

**Gotcha:** deterministic mode trades a real security property (equality
leakage) for queryability — reach for it only when you must filter on the
column. See [Attribute Encryption](./attribute-encryption.md).

#### `#[normalize(trim, downcase, upcase, squish, with = path)]`

Runs an ordered normalizer chain over the owned `String` before every insert
and update. Built-ins (`trim`, `downcase`, `upcase`, `squish`) are
`fn(&str) -> String` in `autumn_web::normalize`; `with = path` calls your own
function with the same signature. Normalizers apply left-to-right.

```rust
#[model(table = "accounts")]
pub struct Account {
    #[id] pub id: i64,
    #[normalize(trim, downcase)] pub email: String,
}
```

**Gotcha:** `#[normalize]` is `String`-only — applying it to `Option<String>`
or any non-`String` field is a compile error. A bare `#[normalize]` or empty
`#[normalize()]` is also an error (it would be a silent no-op); list at least
one normalizer.

#### `#[state_machine(transitions(a -> b, b -> c: "guard"))]`

Declares allowed state transitions for a `String` field. Per annotated field it
emits three items on the model:

```rust
impl Post {
    // 1. Hidden transition table (from, to, optional guard name)
    #[doc(hidden)]
    pub const __AUTUMN_SM_STATUS_TRANSITIONS:
        &'static [(&'static str, &'static str, Option<&'static str>)] = &[ /* ... */ ];

    // 2. Predicate — calls the guard method for guarded transitions
    pub fn can_transition_status_to(&self, target: &str) -> bool { /* ... */ }

    // 3. Attempt — Ok(new_state) or Err if undefined / guard rejects
    pub fn transition_status_to(&self, target: &str) -> AutumnResult<String> { /* ... */ }
}
```

A guarded transition (`draft -> published: "can_publish"`) calls your
`self.can_publish()` method before allowing it.

**Gotchas:** `String`-only; multiple `#[state_machine]` on one field is
rejected; the guard name must be a plain Rust identifier (e.g. `can_ship`, not
`"can ship"`); a raw-identifier field like `r#type` derives method names from
the stripped name (`can_transition_type_to`). See
[State Machines](./state-machines.md).

#### Associations and search keys

`#[belongs_to(Target, fk = ...)]`, `#[has_many(Target, fk = ..., through = join)]`,
and `#[has_one(Target, fk = ...)]` declare relationships used by the eager-load
and association helpers. Unlike the field attributes above, **these are
struct-level attributes** — place them *above* the struct, next to `#[model]`,
just like `#[shard_key]`. The macro reads them only from the model's outer
attributes (`resolve_associations(name, outer_attrs)`); a `#[belongs_to(...)]`
sitting *on a field* is **not** registered — no preload metadata or accessors
are generated, and because it is not in the field-attribute allow-list it can
leak into the emitted Diesel query struct. The foreign key lives on *this* model
for `belongs_to`, on the *target* for `has_many`/`has_one`; `through =` marks a
many-to-many join table.

```rust
#[model(table = "posts")]
#[belongs_to(User, fk = author_id)]   // fk on THIS model
#[has_many(Comment)]                  // fk (post_id) on the TARGET
#[has_many(Tag, through = post_tags)] // many-to-many via a join table
pub struct Post {
    #[id] pub id: i64,
    pub author_id: i64,
    pub title: String,
}
```

An association that was not preloaded returns `NotLoaded` from its accessor
rather than issuing SQL — autumn never lazy-loads.

`#[searchable]` marks a column for the full-text search surface. The
sharding key is set with a **struct-level** `#[shard_key = "field_name"]`
attribute placed above the struct (its value names an existing field, e.g.
`#[shard_key = "shard_id"]`; `"id"` is always valid) — there is no field-level
`#[shard_key]` marker.

---

## Repositories

### `#[repository(Model)]`

Generates a concrete repository struct with CRUD methods, an Axum extractor
impl, and implementations for any derived query methods you declare.

**You write:**

```rust
#[repository(Post)]
pub trait PostRepository {
    fn find_by_published(published: bool) -> Vec<Post>;
    fn count_by_author_id(author_id: i64) -> i64;
}
```

**Generates:**

```rust
// The macro re-emits your trait, augmenting it with built-in CRUD alongside
// your derived queries, then implements the whole thing for a generated
// `Pg*` struct. The CRUD methods are trait methods (bring `PostRepository`
// into scope to call them), not inherent methods on `PgPostRepository`.
pub trait PostRepository: Send + Sync {
    // Built-in CRUD (always added to the trait)
    async fn find_by_id(&self, id: i64) -> AutumnResult<Option<Post>>;
    async fn find_all(&self) -> AutumnResult<Vec<Post>>;
    async fn save(&self, new: &NewPost) -> AutumnResult<Post>;
    async fn update(&self, id: i64, changes: &UpdatePost) -> AutumnResult<Post>;
    async fn delete_by_id(&self, id: i64) -> AutumnResult<()>;
    async fn count(&self) -> AutumnResult<i64>;
    async fn exists_by_id(&self, id: i64) -> AutumnResult<bool>;
    // ...plus your declared derived queries (find_by_published, count_by_author_id)
}

// Concrete struct with a connection pool
#[derive(Clone)]
pub struct PgPostRepository {
    pool: deadpool::Pool<AsyncPgConnection>,
}

// Axum extractor -- use `repo: PgPostRepository` in handler signatures
impl FromRequestParts<AppState> for PgPostRepository { ... }

// One impl block carries the built-in CRUD and your derived queries
impl PostRepository for PgPostRepository {
    // Built-in CRUD (always generated)
    async fn find_by_id(&self, id: i64) -> AutumnResult<Option<Post>> { ... }
    // ...find_all / save / update / delete_by_id / count / exists_by_id...

    // Derived queries (parsed from trait method names)
    async fn find_by_published(&self, published: bool) -> AutumnResult<Vec<Post>> {
        let mut conn = self.pool.get().await?;
        posts::table
            .filter(posts::published.eq(&published))
            .load::<Post>(&mut conn)
            .await
            .map_err(Into::into)
    }

    async fn count_by_author_id(&self, author_id: i64) -> AutumnResult<i64> {
        let mut conn = self.pool.get().await?;
        posts::table
            .filter(posts::author_id.eq(&author_id))
            .count()
            .get_result(&mut conn)
            .await
            .map_err(Into::into)
    }
}
```

**Derived query name parsing rules:**

| Method prefix   | Generated query                          |
|-----------------|------------------------------------------|
| `find_by_`      | `.filter(col.eq(val)).load()`            |
| `count_by_`     | `.filter(col.eq(val)).count()`           |
| `exists_by_`    | `select(exists(...filter(col.eq(val))))` |
| `delete_by_`    | `diesel::delete(...).filter(col.eq(val))`|
| `_and_`         | Joins multiple `.filter()` clauses       |

The full set of `#[repository(...)]` options (the authoritative list is the
attribute's own parse-error message) includes: `table = "..."`, `hooks = Type`,
`commit_hooks = true`, `api = "/path"`, `mcp` / `mcp = "read"`, `policy = Type`,
`scope = Type`, `cursor_key = field`, `cursor_key_type = Type`, `soft_delete`,
`tenant_scoped`, `no_upsert_trait`, `searchable`, `versioned = true`,
`no_versioned_record_impl`, `primary_reads`, `sharded`,
`dependent(ChildRepository, fk = "...", on_delete = ...)`, `broadcasts = true`,
`topic = "..."`, `render = fn`, and `container = "..."`.

### `#[repository]` advanced surface

#### Batched iteration: `find_in_batches` / `find_each`

Generated for **every** repository. Both walk the whole table in bounded-memory
chunks using a **primary-key keyset cursor** (`WHERE id > last ORDER BY id ASC
LIMIT n`), never `LIMIT`/`OFFSET`, so deep iteration stays flat and is stable
under concurrent inserts. Soft-delete filtering, tenant scoping, and read
routing match `find_all`.

```rust
// Chunks of up to 1,000 rows:
let mut batches = repo.find_in_batches(1_000); // -> FindInBatches<'_, Self>
while let Some(chunk) = batches.next_batch().await? { // Option<Vec<Post>>
    // process chunk, then drop it before requesting the next
}

// One row at a time (still fetched in batch_size chunks under the hood):
let mut each = repo.find_each(500); // -> FindEach<'_, Self>
while let Some(row) = each.next().await? { // Option<Post>
    // ...
}
```

**Gotchas:** `batch_size == 0` yields an error on every `next_batch()`. The
cursor only advances on success, so retrying after an `Err` retries the same
batch (`Ok(None)` always means completion). On a sharded + tenant-scoped repo,
`across_tenants()` iteration is rejected — iterate each shard via `from_shard`.

#### `find_or_create_by_<field>[_and_<field>...]`

Declare a race-safe get-or-insert as a trait method taking the lookup
parameters plus an extra `new` insert value; it returns `(Model, bool)` (the
bool is `true` when a row was actually created).

```rust
#[repository(Tag)]
pub trait TagRepository {
    fn find_or_create_by_slug(slug: &str, new: NewTag) -> (Tag, bool);
}
```

It does a read-path lookup first, then an `INSERT ... ON CONFLICT DO NOTHING` on
the primary (so no `23505` unique-violation ever aborts the transaction); on a
lost race it re-looks-up on the primary (read-your-writes). `after_create` /
commit hooks fire only on the created path. Unlike `upsert_many`, it *is*
generated on hooked repositories.

**Gotchas:** `_or_` is unsupported (compile error — race-safety needs a single
unique constraint, not a disjunction). It is only truly race-safe when the
lookup columns match a unique constraint; without one, `ON CONFLICT DO NOTHING`
has nothing to conflict on and concurrent callers can both insert.

#### Grouped aggregates

Declare an aggregate as a trait method **named `<agg>_grouped_by_<column>`**
whose return type is `Vec<(K, V)>`. The `_grouped_by_` segment is what marks the
method as an aggregate (a plain `sum_total_by_status` is *not* recognized), and
the declared pair type is how the macro bakes the concrete key/value SQL types.
The generated inherent method takes no arguments and returns a lazy
`GroupedAggregate<'_, K, V>` builder rather than the `Vec` — chain the builder,
then `.load().await` to run it and get the `Vec<(K, V)>`.

Supported names: `count_grouped_by_<col>` (value type must be `i64`) and
`sum_`/`avg_`/`min_`/`max_<num_col>_grouped_by_<col>` (`avg` → `Option<f64>`;
`sum`/`min`/`max` → `Option<T>`, since the group can be empty or all-`NULL`).
The key `K` must be the group column's **non-nullable** Rust type.

```rust
#[repository(Order)]
pub trait OrderRepository {
    fn sum_total_grouped_by_status() -> Vec<(String, Option<i64>)>;
    fn count_grouped_by_created_at() -> Vec<(DateTime<Utc>, i64)>;
}

let top = repo
    .sum_total_grouped_by_status() // -> GroupedAggregate<'_, String, Option<i64>>
    .order_by_aggregate_desc()
    .limit(5)
    .filter_range(lo, hi)
    .load()
    .await?; // -> Vec<(String, Option<i64>)>

// Time series:
let daily = repo
    .count_grouped_by_created_at()
    .bucket(DateBucket::Day)
    .load()
    .await?;
```

Builder methods available on **every** aggregate builder regardless of key type:
`.order_by_aggregate_desc()` / `.order_by_aggregate_asc()`, `.limit(n)`,
`.filter_eq(v)`, and `.filter_range(lo, hi)`. Filter values are bound (never
interpolated).

`.bucket(bucket)` (`DateBucket::Day` / `Week` / `Month`) is **only** defined when
the group key `K` is a timestamp type — `NaiveDateTime` or `DateTime<Utc>` — since
it swaps the raw group column for `date_trunc('<unit>', <col>)`. Calling it on a
non-temporal aggregate (e.g. the `String`-keyed `sum_total_grouped_by_status()`
above) is a compile error, because the method does not exist for that key. A
`timestamptz` (`DateTime<Utc>`) bucket uses `date_trunc(.., 'UTC')` for
timezone-stable buckets.

**Gotchas:** on a sharded, tenant-scoped repo, *every* grouped aggregate —
`count_grouped_by_*` included, not just `sum`/`avg`/`min`/`max` — rejects
`across_tenants()` at runtime (partial per-shard results can't be merged); run
the aggregate per shard via `from_shard(...)` instead. Grouping/filtering on an
`#[encrypted]` column is rejected at runtime (the stored value is ciphertext).

#### `dependent(...)` cascades

`dependent(ChildRepository, fk = "col", on_delete = destroy | delete_all |
nullify | restrict)` makes `delete_by_id` cascade into the child repository's
delete path within one transaction (overriding the plain `delete_by_id`).

**Gotcha:** the cascade is **single-level** — it applies this model's deletion
to each directly-matched child but does *not* recurse into the child's own
`dependent(...)` declarations, so grandchildren are not handled.

#### `cursor_page` (keyset pagination)

Only generated when you declare `cursor_key = field`.

- **With `cursor_key_type = Type`** — a fully-typed, two-part `(key, id)` keyset
  cursor: `WHERE (cursor_key < after_k) OR (cursor_key = after_k AND id <
  after_id)`, ordered `(cursor_key DESC, id DESC)`. Always correct.
- **Without `cursor_key_type`** — an `id`-only cursor (`WHERE id < after_id`).
  Correct **only** when `cursor_key` values are monotonically correlated with
  `id` (e.g. `created_at` on an auto-increment table). For non-monotonic data
  (backfills, imports), implement `cursor_page` manually.

See [Pagination](./pagination.md).

#### Read-replica routing

When `database.replica_url` is configured, generated read-only methods
(`find_by_id`, `find_all`, `count`, `paginate`, `cursor_page`, derived
`find_by_*`, search reads) acquire their connection from the replica pool;
mutating methods always use the primary. Add `primary_reads` to pin a
read-after-write-sensitive repository's reads to the primary, or call the
generated `on_primary()` to pin a single call chain (read-your-writes).

See [Repositories](./repositories.md).

---

## Services

### `#[service]`

Generates a concrete struct and an Axum extractor from a trait with a `deps()`
declaration.

**You write:**

```rust
#[service]
pub trait OrderService {
    fn deps(order_repo: PgOrderRepository, email: EmailClient);
    async fn place_order(&self, req: OrderRequest) -> AutumnResult<Order>;
}
```

**Generates:**

```rust
#[derive(Clone)]
pub struct OrderServiceImpl {
    pub order_repo: PgOrderRepository,
    pub email: EmailClient,
}

// Extractor -- each dependency is extracted from AppState
impl FromRequestParts<AppState> for OrderServiceImpl {
    type Rejection = AutumnError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState)
        -> Result<Self, Self::Rejection>
    {
        Ok(Self {
            order_repo: PgOrderRepository::from_request_parts(parts, state).await?,
            email: EmailClient::from_request_parts(parts, state).await?,
        })
    }
}
```

You implement the business methods on `OrderServiceImpl` yourself.

---

## Background Work: Scheduled Tasks, Jobs, Events, Listeners, One-off Tasks

### `#[scheduled(every = "5m")]` + `tasks![]`

**You write:**

```rust
#[scheduled(every = "5m", name = "cleanup")]
async fn cleanup(state: AppState) -> AutumnResult<()> {
    // cleanup logic
    Ok(())
}
```

**Generates (alongside your function):**

```rust
pub fn __autumn_task_info_cleanup() -> ::autumn_web::task::TaskInfo {
    ::autumn_web::task::TaskInfo {
        name: "cleanup".to_string(),
        schedule: Schedule::FixedDelay(Duration::from_secs(300)),
        handler: |state| Box::pin(async move { cleanup(state).await }),
    }
}
```

Collected via `tasks![cleanup]` (same pattern as `routes![]`). Also accepts
`cron = "0 0 0 * * *"` for cron scheduling.

### `#[job(...)]` + `jobs![]`

Declares an on-demand background job. Generates a `{PascalName}Job` companion
struct with typed enqueue helpers, plus a `__autumn_job_info_{fn}` companion
that `jobs![]` collects.

**You write:**

```rust
#[job(queue = "critical", max_attempts = 5)]
async fn send_password_reset(state: AppState, args: ResetArgs) -> AutumnResult<()> {
    Ok(())
}
```

**Generates:**

```rust
pub struct SendPasswordResetJob;

impl SendPasswordResetJob {
    pub const NAME: &'static str = "send_password_reset";

    pub async fn enqueue(args: ResetArgs) -> AutumnResult<()> { ... }
    pub async fn enqueue_in(args: ResetArgs, delay: Duration) -> AutumnResult<()> { ... }
    pub async fn enqueue_at(args: ResetArgs, when: DateTime<Utc>) -> AutumnResult<()> { ... }
    pub async fn enqueue_tracked(args: ResetArgs) -> AutumnResult<TrackedJobHandle> { ... }
    pub async fn enqueue_tracked_for(args: ResetArgs, owner: TrackedJobOwner)
        -> AutumnResult<TrackedJobHandle> { ... }
}

#[doc(hidden)]
pub fn __autumn_job_info_send_password_reset() -> JobInfo { /* name, queue, retries, handler */ }
```

Call it with `SendPasswordResetJob::enqueue(ResetArgs { .. }).await?`.

Options: `queue`, `max_attempts`, `backoff_ms`, `name`; uniqueness
(`unique` / `unique_by = "a,b"` / `unique_window = "pending"|"running"` /
`unique_for_ms`); concurrency (`concurrency = N` / `concurrency_key`); and
payload versioning (`version = N` / `upgrade = path`).

**Handler arity & tracking:** the job function takes `async fn(AppState, Args)`
or `async fn(AppState, Args, JobContext)`. The `enqueue_tracked` /
`enqueue_tracked_for` helpers are **always** generated regardless of arity, and
they return a `TrackedJobHandle` whose completion the runtime settles for *any*
tracked job — two-argument handlers included. Whether or not the handler takes a
`JobContext`, the runtime marks the record succeeded on success and failed on a
terminal failure (last attempt or a panic), so callers can always poll the
handle for completion. The optional third `JobContext` argument is only needed
to report *progress* and/or a *custom* result/error payload (via
`ctx.set_progress` / `ctx.set_result` / `ctx.set_user_error`); basic completion
tracking needs no `JobContext`:

```rust
#[job(name = "export_orders")]
async fn export_orders(state: AppState, args: ExportArgs, ctx: JobContext) -> AutumnResult<()> {
    ctx.set_progress(50, Some("Rows 1200/5000")).await?;
    ctx.set_result(serde_json::json!({ "download_url": "/blob/abc.csv" }));
    Ok(())
}

let handle = ExportOrdersJob::enqueue_tracked(ExportArgs { account_id: 1 }).await?;
println!("poll at {}", handle.status_path());
```

**Gotchas:** the handler must be `async` with exactly 2 or 3 args. `upgrade =`
requires `version >= 2` (an upgrade hook only runs on an older stored payload).
`unique = false` conflicts with any other uniqueness attribute. See
[Jobs](./jobs.md) and [Operating Background Jobs](./operating-background-jobs.md).

### `#[event]` / `#[event(name = "...")]`

Marks a struct as a typed domain event.

**You write:**

```rust
#[event]
struct UserSignedUp { user_id: i64 }
```

**Generates:**

```rust
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
struct UserSignedUp { user_id: i64 }

impl ::autumn_web::events::Event for UserSignedUp {
    // Default name is module-qualified so two `Created` events in different
    // modules never collide; `#[event(name = "user.signed_up")]` overrides it.
    const NAME: &'static str = concat!(module_path!(), "::", "UserSignedUp");
}
```

See [Events](./events.md).

### `#[listener(EventType, ...)]` + `listeners![]`

Declares an async listener reacting to a typed `#[event]`. Emits a
`__autumn_listener_info_{fn}` companion that `listeners![]` collects.

**You write:**

```rust
#[listener(UserSignedUp)]
async fn send_welcome(state: AppState, event: UserSignedUp) -> AutumnResult<()> {
    Ok(())
}

// Durable variant, enqueued onto the #[job] queue:
#[listener(UserSignedUp, durable, max_attempts = 5, backoff_ms = 1000)]
async fn seed_workspace(state: AppState, event: UserSignedUp) -> AutumnResult<()> {
    Ok(())
}
```

**Effect:** by default the listener runs **synchronously**, in-request, before
the response (`DispatchMode::Sync`). Adding `durable` enqueues it onto the
existing `#[job]` queue (`DispatchMode::Durable`, job name
`__event_listener::{module}::{fn}`) so it survives restarts and inherits retry +
DLQ semantics.

**Gotchas:** the function must be `async fn(AppState, Event)` (exactly two
args). `max_attempts` / `backoff_ms` only apply to `durable` listeners — using
them on a sync listener is a compile error.

### `#[task]` + `one_off_tasks![]`

Declares a one-off operational script (a manually-invoked task, not scheduled).
Emits a `__autumn_one_off_task_info_{fn}` companion collected by
`one_off_tasks![]`.

**You write:**

```rust
/// Backfill missing slugs.
#[task]
async fn backfill_slugs(repo: PgPostRepository) -> AutumnResult<()> {
    Ok(())
}
```

**Effect:** each parameter is resolved through `TaskExtractor` at run time; the
first doc-comment line becomes the task's description; `#[task(name = "...")]`
overrides the name (defaults to the function name).

### `#[cached]`

Wraps a function with an in-memory cache. Each annotated function gets its own
`static` cache, keyed by a hash of the arguments.

**You write:**

```rust
#[cached(ttl = "5m", max = 100, result)]
async fn get_user(id: i64) -> AutumnResult<User> {
    db.find(id).await
}
```

**Expands to (conceptually):**

```rust
async fn get_user(id: i64) -> AutumnResult<User> {
    static __AUTUMN_CACHE: OnceLock<MokaCache> = OnceLock::new();
    let __autumn_moka = __AUTUMN_CACHE.get_or_init(|| MokaCache::new(100, Some(::core::time::Duration::from_secs(300))));
    // Prefer a process-wide shared backend (e.g. Redis) when registered,
    // else fall back to the per-function Moka store.
    let __autumn_cache = global_cache().unwrap_or(__autumn_moka);
    let __autumn_key = make_cache_key(concat!(module_path!(), "::", "get_user"), &(id.clone(),));
    if let Some(hit) = get_cached::<User>(__autumn_cache, &__autumn_key) {
        return Ok(hit); // `result` mode caches only Ok values
    }
    let out = /* original body */;
    // insert on success ...
    out
}
```

Options: `ttl` (duration string, e.g. `"5m"`), `max` (entry cap, default
`10_000`, LRU eviction), and the `result` flag (cache only `Ok` values, pass
`Err` through uncached).

**Gotcha:** `#[cached]` cannot be applied to methods with a `self` receiver.
See [Fragment Caching](./fragment-caching.md) and [Cache Stampede](./cache-stampede.md).

---

## Guards & Rate Limiting

These attributes stack on top of a route macro and inject hidden extractors plus
a pre-body check. They share a family resemblance: `#[secured]`, `#[authorize]`,
`#[step_up]`, and `#[throttle]` all rewrite the handler to run a check first.

### `#[secured("role")]`

Injects a session extractor and an authorization check at the top of your
handler.

**You write:**

```rust
#[get("/admin")]
#[secured("admin")]
async fn admin_panel() -> &'static str {
    "welcome"
}
```

**Effectively becomes:**

```rust
#[get("/admin")]
async fn admin_panel(__autumn_session: Session) -> AutumnResult<&'static str> {
    ::autumn_web::auth::__check_secured(&__autumn_session, &["admin"]).await?;
    Ok("welcome")
}
```

- No roles listed (`#[secured]`) = authentication check only (401 if not
  logged in)
- Roles listed (`#[secured("admin", "editor")]`) = must have at least one of
  the listed roles (403 if missing)

### `#[authorize("action", resource = Type)]`

Injects hidden extractors (`Session`, `State<AppState>`, the granted
API-token scopes, and the route-version/idempotency-replay extensions) and
a record-level [`Policy`](./authorization.md) check at the top of your
handler. Mirrors `#[secured]` but answers "are you allowed to act on
*this* record?" instead of "are you allowed to call this *route*?"

**You write:**

```rust
#[get("/posts/{id}/edit")]
#[authorize("update", resource = Post)]
async fn edit_post(post: Post) -> AutumnResult<Markup> {
    Ok(html! { h1 { (post.title) } })
}
```

**Effectively becomes:**

```rust
#[get("/posts/{id}/edit")]
async fn edit_post(
    __autumn_token_scopes: Option<Extension<ApiTokenScopes>>,
    __autumn_session: Session,
    State(__autumn_state): State<AppState>,
    // … plus hidden route-version and idempotency-replay extractors
    post: Post,
) -> Response {
    // Inert build-time marker — nothing reads it at runtime. It lets the
    // route macro recover the binding when `#[authorize]` expands *before*
    // `#[get]` (attribute order decides which macro sees the other's output).
    const __AUTUMN_AUTHORIZE_BINDINGS: &[(&str, &str)] = &[("update", "Post")];

    if let Err(__autumn_error) = ::autumn_web::authorization::__check_policy_scoped::<Post>(
        &__autumn_state,
        &__autumn_session,
        __autumn_token_scopes.as_ref().map(|__e| &__e.0),
        "update",
        &post,
    ).await {
        return __autumn_error.into_response();
    }
    // … sunset check and idempotency-replay short-circuit …
    let __autumn_inner: AutumnResult<Markup> = (async move {
        Ok(html! { h1 { (post.title) } })
    }).await;
    __autumn_inner.into_response()
}
```

- The `from = name` argument overrides the default snake-case parameter
  binding (default: `Post` → `post`).
- The check returns the configured deny status — `404` by default to
  avoid leaking record existence, configurable via
  `[security] forbidden_response = "403"`.
- The scope-aware `__check_policy_scoped` takes the request's granted API-token
  scopes as its third argument, so a policy can decide on `ctx.has_scope(...)`
  for token-authenticated principals as well as session ones.
- The handler is rewritten to return `Response`, and the original body is
  wrapped in `(async move { … }).await` so the deny path can return early
  before it runs.
- `__AUTUMN_AUTHORIZE_BINDINGS` records the resource *identifier as written*,
  never the `Policy` impl that serves the check — that is resolved from the
  registry at boot. It is what puts the route's `(action, resource)` pair into
  the `authorization_policies` dimension of the build-time security manifest;
  see [Security Posture Manifest](./security-posture-manifest.md).
- Coexists with `#[secured]`: stack both attributes when a route should
  require both authentication/role gating and a record-level check. Each
  attribute leaves its own marker, so both the roles and the binding survive
  however the two expand around each other.

### `#[step_up]` / `#[step_up(max_age = "5m")]`

Requires a *fresh* authentication (recent re-auth) before the handler runs — a
step-up challenge for sensitive actions. Injects hidden extractors and prepends
a freshness check.

**You write:**

```rust
#[post("/settings/delete-account")]
#[step_up(max_age = "5m")]
async fn delete_account(session: Session) -> AutumnResult<Redirect> {
    // ...
}
```

**Effect:** before the body runs, `__check_step_up_with_config` verifies the
session authenticated within `max_age` (default 5 minutes if bare `#[step_up]`).
On failure it returns a JSON `401`-style challenge for API clients or a redirect
(honoring `Referer`) for browsers. `max_age` is always a **string literal**:
`"5m"`, `"1h"`, `"30s"`, or raw seconds as a string (`"300"`). A bare unquoted
number (`max_age = 300`) is a compile error.

### `#[feature_flag("key", fallback = fn)]`

Gates the entire route on a feature flag, evaluated inside a dedicated
`FromRequestParts` gate struct so Axum short-circuits **before** body extractors
(`Json`, `Form`) are consumed.

**You write:**

```rust
#[get("/beta")]
#[feature_flag("beta_dashboard")]
async fn beta_dashboard() -> Markup {
    html! { h1 { "Beta!" } }
}
```

**Generates** a per-handler gate `__AutumnFlagGate_beta_dashboard` whose
`FromRequestParts` impl returns `Err(response)` when the flag is disabled. The
default rejection is `404 Not Found` (**fail-closed** — a disabled feature looks
like it doesn't exist); a `fallback = my_handler` delegates to your handler
instead:

```rust
#[get("/experimental")]
#[feature_flag("experimental_feature", fallback = feature_disabled)]
async fn experimental() -> Markup { html! { "Experimental" } }

async fn feature_disabled() -> impl IntoResponse {
    (StatusCode::NOT_FOUND, "Feature not available yet")
}
```

See [Feature Flags](./feature-flags.md).

### `#[throttle(...)]`

A per-route rate-limit guard. Applies to **async functions only**. It injects
several hidden extractors and prepends a runtime check that returns `429` early
when over the limit.

**You write:**

```rust
#[post("/login")]
#[throttle(limit = 5, per = "1m", key = "ip")]
async fn login() -> AutumnResult<&'static str> {
    Ok("welcome back")
}
```

**Effect (conceptually):** the macro rewrites the handler to return `Response`,
injects hidden extractors (`State<AppState>`, `HeaderMap`,
`Option<MatchedPath>`, connection peer, principal, session, exempt/replay
markers), and prepends:

```rust
const __AUTUMN_THROTTLE_ROUTE_ID: &str = concat!(module_path!(), "::", "login");
if let Err(resp) = ::autumn_web::security::__check_throttle(
    &__autumn_state, __AUTUMN_THROTTLE_ROUTE_ID, /* matched path, spec, headers, peer, ... */
).await {
    return resp; // 429 Too Many Requests
}
// ... then the original body, coerced to a Response via IntoResponse
```

Forms:

- `#[throttle(limit = N, per = "1m", key = "ip" | "principal" | "token")]` —
  inline limiter (`key` optional; default strategy otherwise).
- `#[throttle("named")]` — a named limiter defined under
  `[security.rate_limit.named.named]` in config.

**Gotcha (attribute ordering):** put the **route method attribute outermost**,
above `#[throttle]`:

```rust
#[post("/login")]              // method attribute OUTERMOST
#[throttle(limit = 5, per = "1m", key = "ip")]
async fn login() -> Json<Session> { /* ... */ }
```

Both orders throttle correctly, but only method-outermost lets the route macro
see the handler's real return type (`Json<Session>`) for OpenAPI response-schema
generation. When `#[throttle]` expands first it rewrites the return type to
`Response` (like the sibling `#[secured]` / `#[step_up]` / `#[authorize]`
guards), and the `Json<T>` schema is lost from the generated document.

**Gotcha (body extractors run first):** the throttle check runs after
`FromRequestParts` extractors but *before* the body — however Axum parses body
extractors (`Json` / `Form` / `Multipart`) before the check, so an over-limit
client can still incur body parsing before its `429`. For hard pre-body
protection, combine with the global limiter layer under `[security.rate_limit]`.

**Runtime detail — `RateLimitEnvelopeCounted`:** this is **not** a macro. It's
an internal marker struct (`autumn_web::security::rate_limit::RateLimitEnvelopeCounted`)
set only on the MCP `tools/call` replay path to avoid double-charging a request
whose enclosing `/mcp` envelope was already counted. Only the framework-default
`[security.rate_limit]` limiter — which shares that envelope's bucket — skips a
request carrying it; per-route `#[throttle]` buckets and user-installed
path-override limiters (added via `AppBuilder::layer`) do **not** share the
envelope bucket and **still** charge it, exactly as a direct call (use
`RateLimitExempt` to bypass *every* limiter). It's mentioned here only because it
lives next to the throttle machinery. See [Rate Limiting](./rate-limiting.md).

### `#[query_budget(N)]`

A **build-time** gate, not a runtime guard: it walks the handler's AST, bounds
how many database queries any statically reachable path can issue, and fails
the compile when that bound exceeds `N`.

**You write:**

```rust
#[get("/posts")]
#[query_budget(2)]
async fn index(repo: PgPostRepository) -> AutumnResult<Markup> {
    let posts = repo.find_all().await?;                                // 1
    let posts = repo.preload(posts, Post::preload().author()).await?;  // 2
    Ok(render(&posts))
}
```

**Effect:** the handler is emitted **unchanged** — no injected extractors, no
prologue, no rewritten return type, nothing at runtime. The only added item is
a hidden constant recording the contract and the proof:

```rust
#[doc(hidden)]
#[allow(non_upper_case_globals, dead_code)]
const __AUTUMN_QUERY_BUDGET_index: ::autumn_web::query_budget::StaticQueryBudget =
    ::autumn_web::query_budget::StaticQueryBudget::new("index", Some(2u32), Some(2u32));
```

(That constant is withheld for a method taking `self`, where a trait impl would
reject an associated const the trait never declared.)

Two statement-level annotations, `#[query_cost(N)]` and
`#[query_exempt(reason = "…")]`, are this macro's own vocabulary: it reads them
and **strips them from the emitted function**, so they never reach rustc — and
so they are meaningless outside a `#[query_budget]`-annotated function.

**Attribute ordering:** either order works, since the handler is emitted
verbatim. Keep the method attribute outermost anyway, matching `#[throttle]` /
`#[secured]`, so the route macro still sees the real return type for the
OpenAPI response schema.

See [Compile-Time Query Budgets](./query-budgets.md).

---

## Mail

### `#[mailer]`

Applied to an `impl` block. For each method that returns
`autumn_web::mail::Mail` (via an `&self` receiver), it generates two delivery
helpers alongside the template method: `send_{method}` (async, sends now) and
`deliver_later_{method}` (enqueues delivery on the job queue).

**You write:**

```rust
#[mailer]
impl UserMailer {
    fn welcome(&self, user: &User) -> Mail { /* build the Mail */ }
}
```

**Generates** `UserMailer::send_welcome(...).await` and
`UserMailer::deliver_later_welcome(...)` from your `welcome` template method.

### `#[mailer_preview]` + `mail_previews![]`

Registers zero-argument, synchronous `-> Mail` associated functions for the dev
mail preview UI. Emits a `__autumn_mail_previews()` helper on the impl block;
`mail_previews![UserMailer, ...]` collects them into a `Vec<MailPreview>`.

**You write:**

```rust
#[mailer_preview]
impl UserMailer {
    fn preview_welcome() -> Mail { /* build a sample Mail */ }
}

let previews = mail_previews![UserMailer];
```

**Gotchas:** preview methods must be **synchronous**, **zero-arg** (not even
`&self`), non-generic, and return `Mail`.

### `#[inbound_mail(to = "...", processing = "...")]`

Annotates an async inbound-mail handler. Generates a companion
`{fn}_handler_info()` returning an `InboundMailHandlerInfo` ready to register on
an `InboundMailRouter`.

**You write:**

```rust
#[inbound_mail(to = "support@company.com")]
async fn handle_support(email: InboundEmail) -> AutumnResult<()> {
    tracing::info!(from = %email.from, "inbound support email");
    Ok(())
}

// Registration:
InboundMailRouter::new().handler(handle_support_handler_info())
```

Recipient matching: `to = "address@example.com"` (exact),
`to = "replies+{token}@app.example"` (plus-address; token via
`InboundEmail::plus_token()`), or `to = "prefix+*"` (local-part prefix).
`processing = "sync" | "background"` (default `"background"`). See
[Mail](./mail.md).

---

## i18n, Stories & Path Helpers

### `t!` (i18n translate)

Translates an i18n key, with **compile-time validation** that the key exists in
the default locale's `.ftl` file.

**You write:**

```rust
t!(locale, "welcome.title")
t!(locale, "welcome.greeting", name = "Ada") // Fluent { $name } placeable
```

**Compile-time behavior:** the macro reads
`$CARGO_MANIFEST_DIR/i18n/<default>.ftl` (default locale from
`AUTUMN_I18N_DEFAULT_LOCALE`, default `"en"`). A missing key becomes a
`compile_error!` pointing at the literal, with a "did you mean" suggestion for
near-miss typos. If the `.ftl` file doesn't exist yet, the macro degrades to a
pure runtime call so the build still succeeds. See [i18n](./i18n.md).

### `story!` (widget gallery)

Defines a widget story for the `/_stories` gallery:
`story!{ "Group", "Name", { ... } }`. The brace block is **both** executed for
the live render **and** captured byte-for-byte (comments and formatting
included) as the displayed source, so the shown code is provably the code that
rendered. The block must be a self-contained expression evaluating to
`maud::Markup` — it is coerced to a plain `fn() -> Markup`, so capturing
anything from the surrounding environment is a compile error.

### `paths![]` (typed path helpers)

Emits a `pub mod paths { … }` that re-exports each handler's typed path helper
(`__autumn_path_*`) under its short name. Only the standard HTTP route macros —
`#[get]`/`#[post]`/`#[put]`/`#[delete]`/`#[patch]` — emit that helper, so
`paths![]` accepts **only** handlers annotated with those macros. Unlike
`routes![]`, it does **not** accept `#[static_get]` or `#[ws]` handlers: those
emit a route companion (and, for `static_get`, static metadata) but no
`__autumn_path_*` helper, so listing one in `paths![]` fails to compile with an
unresolved-import error.

**You write:**

```rust
autumn_web::paths![show_post, create_post, posts::index];
```

**Expands to:**

```rust
pub mod paths {
    pub use super::__autumn_path_show_post as show_post;
    pub use super::__autumn_path_create_post as create_post;
    pub use super::posts::__autumn_path_index as index;
}
```

Callers then write `use crate::routes::paths;` and `paths::show_post(id)`
instead of the internal `__autumn_path_show_post(id)`. See
[Path Helpers](./path-helpers.md).

### Collector macros at a glance

Every attribute macro that emits a `__autumn_*` companion has a matching
list macro that gathers them into a typed `Vec` for the app builder:

| Collector | Gathers | Companion prefix |
|-----------|---------|------------------|
| `routes![]` | `Vec<Route>` | `__autumn_route_info_` |
| `static_routes![]` | `Vec<StaticRouteMeta>` | `__autumn_static_meta_` |
| `tasks![]` | `Vec<TaskInfo>` | `__autumn_task_info_` |
| `jobs![]` | `Vec<JobInfo>` | `__autumn_job_info_` |
| `listeners![]` | `Vec<ListenerInfo>` | `__autumn_listener_info_` |
| `one_off_tasks![]` | `Vec<OneOffTaskInfo>` | `__autumn_one_off_task_info_` |
| `mail_previews![]` | `Vec<MailPreview>` | `__autumn_mail_previews` |
| `paths![]` | `pub mod paths` | `__autumn_path_` |

---

## The Companion Function Pattern

All Autumn macros follow the same architectural pattern:

1. **Your code stays untouched** (or minimally modified for guards like
   `#[secured]` / `#[throttle]`)
2. **A hidden `__autumn_*` companion function** is generated next to your code
3. **A collection macro** (`routes![]`, `tasks![]`, `jobs![]`,
   `static_routes![]`, …) calls those companions to build typed vectors
4. **The app builder** consumes those vectors at startup

This means:
- Your handler signatures are real Rust -- IDE autocomplete and type checking
  work normally
- The generated code is always next to your code in the expanded output
- There is no runtime reflection, registration, or classpath scanning

---

## Debugging Macro Issues

### "I'm not sure if my macro is being applied"

Search the expanded output for the companion function:

```bash
cargo expand | grep __autumn_route_info_my_handler
```

If it's missing, the attribute macro didn't run. Check that:
- You imported the macro (`use autumn_web::get;` or `use autumn_web::prelude::*;`)
- The attribute is on the function, not on a `mod` block

### "My route isn't being registered"

The macro generates the companion, but `routes![]` must include it:

```rust
// This handler exists but is not mounted:
#[get("/secret")]
async fn secret() -> &'static str { "hidden" }

// Fix: add it to routes![]
.routes(routes![secret])
```

### "My `#[throttle]` route lost its OpenAPI response schema"

Attribute ordering. Put the route method attribute **outermost**, above
`#[throttle]` (and the sibling `#[secured]` / `#[step_up]` / `#[authorize]`
guards). When a guard expands first it rewrites the return type to `Response`,
so the route macro can no longer see the real `Json<T>` type for the generated
OpenAPI document. Both orders still *enforce* the guard correctly.

### "cargo expand shows too much noise"

Expand a single module to reduce output:

```bash
cargo expand routes::todos 2>/dev/null | rustfmt
```

### Compiler errors pointing at macro-generated code

The proc macros preserve your original `Span` information, so compiler errors
should point at your source code, not at generated code. If you see an error
in generated code, it usually means:
- A type mismatch in your handler parameters
- A missing `use` import for a Diesel table or type
- A field name in a `find_by_` method that doesn't match a database column
