---
name: autumn-web
description: >
  Use when building, debugging, documenting, or upgrading Rust web applications
  with autumn-web, autumn-cli, or first-party Autumn crates; also use for
  Autumn route/model/repository/job/webhook/admin macros, AppBuilder setup,
  Maud + htmx server-rendered UI, Diesel async Postgres, and Autumn 0.5.x
  migration or release work.
---

# autumn-web - Rust Web Framework

**Repository**: https://github.com/madmax983/autumn
**Branch**: `trunk-dev`
**Latest published release**: 0.5.0 on crates.io | **Edition**: 2024 | **MSRV**: 1.88.0
**Author**: madmax983

**Version identity trip wire**: the workspace on `trunk-dev` is versioned
0.6.0, but 0.6.0 is **not published** — crates.io still serves 0.5.0. Apps
depending on the published crates (`autumn-web = "0.5"`, `cargo install
autumn-cli --version 0.5.0`) do not have trunk-dev-only features. Anything
below marked **(unreleased)** is merged on `trunk-dev` but absent from the
published 0.5.0 crates — do not use it in an app built against 0.5.0.
Unmarked features are in the published release.

autumn-web is a Spring Boot-style web framework for Rust, built on Axum. It
assembles Axum, Diesel, Maud, htmx, Tailwind, Tokio, tracing, and production
defaults into a convention-over-configuration stack with proc-macro ergonomics.

## Read these references

This file is the quick operating guide. Load the adjacent reference files only
when their details matter:

- `references/api-reference.md` - release-line API map, proc macros,
  feature flags, AppBuilder methods, config env names, and dependency versions.
- `references/examples.md` - official 0.5.0 example patterns for minimal apps,
  CRUD, production-ish jobs, Redis channels, S3 storage plugins, and signed
  webhooks. Use this before generating full app code.
- `docs/guide/accessibility.md` - accessible-by-construction UI. Prefer the
  typed `autumn_web::a11y` primitives (`Img` / `Button` / `Link` / `MenuItem`
  plus the labeled-typestate form controls `TextField` / `TextArea` / `Select`
  (+ `SelectOption`) / `Checkbox` / `FileField`) over raw `<img>` / `<button>` /
  `<input>` / `<textarea>` / `<select>` in `html!` — the accessible name (alt
  text, button label, field label) is a required constructor argument (a form
  control cannot render until `.label()` / `.aria_label()` / `.labelled_by()`
  supplies one), so a missing one is a compile error, not a runtime audit miss.
  The form controls also carry validation/ARIA setters (`.required()`,
  `.aria_required()`, `.aria_invalid()`, `.described_by()`, per-type
  min/max/length) plus a `.hx(name, value)` escape hatch for arbitrary `hx-*`
  attributes. Treat `autumn a11y verify` as an advisory/best-effort CI net, not
  a guarantee — the typed primitives are the compile-time proof (trunk-dev,
  #1706). See `skills/autumn-web/references/api-reference.md` for the full
  setter surface.

## Prefer framework idioms over raw Diesel/Axum

Autumn deliberately lets you drop to raw Axum routers and raw Diesel queries.
That is a **last resort**, not a starting point. Before hand-rolling any of
the patterns below, check this table and the matching `docs/guide/*.md` page —
the framework almost certainly already generates or ships it:

| Temptation (do NOT hand-roll) | Autumn-native answer |
|---|---|
| Status-transition validation written by hand in `before_create`/`before_update` hooks or handlers (`if old == "draft" && new == "published"` match blocks) | `#[state_machine(transitions(...))]` on the model field — generated `can_transition_{field}_to` / `transition_{field}_to` enforce the graph. See "Model state machines" below and `docs/guide/state-machines.md` |
| Raw `axum::Router` routes and handlers, manual `.route("/x", get(...))` | `#[get]`/`#[post]`/`#[put]`/`#[patch]`/`#[delete]` + `routes![...]`, `.scoped(prefix, layer, routes)` for groups. `.merge()`/`.nest()` exist for the rare escape hatch only |
| Raw Diesel queries for CRUD, lookups, pagination, or bulk writes | `#[repository]`-generated methods: `find_by_id`, `find_all`, `save`, `update`, `delete_by_id`, derived `find_by_*`, `page(&PageRequest)`, `cursor_page(&CursorRequest)`, bulk `save_many`/`update_many`/`delete_many`/`upsert_many`. See `docs/guide/repositories.md`, `docs/guide/pagination.md` |
| Manual per-item queries in a loop (N+1) or hand-written JOINs to fetch associations | `#[belongs_to]`/`#[has_many]`/`#[has_one]` + `repo.preload(records, Model::preload()...)` (unreleased) |
| Hand-rolled auth, session, or token checks in handler bodies | `#[secured]` / `#[secured("role")]`, `#[authorize]`, repository `policy =`/`scope =`; `#[secured(scopes = [...])]` for service tokens (unreleased) |
| A punishingly low global rate limit to protect one abuse-prone endpoint (login, search, export) | `#[throttle(limit = 5, per = "1m", key = "ip")]` sits alongside `#[get]`/`#[post]`; `#[throttle("login")]` reads `[security.rate_limit.named.login]` from config (unreleased — issue #1350, see `docs/guide/rate-limiting.md`) |
| Hand-assembled `<form>` markup with manual value re-fill and error display | `autumn_web::form::form_for(&changeset, action, method)` + the `#[model]`-derived `FormModel` (unreleased — trunk-dev, not in published 0.5.0) renders the whole form — CSRF, `_method` override, one pre-filled control per column, inline errors, submit — in one call; see "Whole-form rendering" below. Compose the per-field helpers only when its escape hatches don't fit: `form_tag`, `method_input`, `text_input` (published); `number_input`, `datetime_input`, `date_input`, `checkbox_input`, `select_input` + `Changeset` 422 re-render (unreleased) |
| `find_all()` + a loop (or raw Diesel `LIMIT`/`OFFSET` paging) to sweep a whole table in a task/job/backfill | `repo.find_in_batches(n)` / `repo.find_each(n)` (unreleased — trunk-dev) — bounded-memory primary-key keyset iteration. See "Generated repository surface" below and `docs/guide/pagination.md` |
| Ad-hoc `tokio::spawn` / background threads for deferred work | `#[job]` (+ retries, backends, uniqueness/concurrency caps), `#[scheduled]` for recurring, `#[task]` for operator CLI work |
| Hand-written memoization or cache-aside code | `#[cached]` on functions; `cache::get_or_compute` / `get_or_compute_with` for stampede-safe read-through fills (unreleased) |
| Hand-written transaction retry loops for serialization failures | `Db::tx(...)`; `Db::tx_with(TxOptions::serializable(), ...)` auto-retries 40001 (unreleased) |
| Hand-rolled HMAC verification for Stripe/GitHub/Slack callbacks | `SignedWebhook` extractor + `[webhooks.<name>]` config |
| Hand-rolled pager markup (page-number windows, prev/next links) | `pagination_nav(&page, &PagerOptions::new("/posts"))` / `cursor_pagination_nav` (unreleased) |
| Hand-rolled cross-module notifications (calling every reaction inline) | `#[event]` + `#[listener]` typed event bus, `.listeners(listeners![...])` (unreleased) |
| Hand-rolled cards, tabs, modals, delete-confirm dialogs, method-override links | `autumn_web::widgets`: `card`/`stat_card`, `tabs`, `modal`/`confirm_action`, `link_to`/`button_to` + `ui::WIDGETS_CSS_PATH` stylesheet (unreleased) |
| Hand-built file-download responses (manual `Content-Disposition`/`Content-Type`/`Content-Length` headers, byte-buffered blob reads) | `autumn_web::download::Download` — `from_bytes` / `from_stream` / `from_async_read` / `from_blob(&store, key).await?` + `.filename(...)` / `.content_type(...)` / `.inline()`; RFC 5987 filenames, injection-safe, streams blobs without buffering (unreleased) |
| Hand-parsed `Range` headers / manual `206 Partial Content` / `Content-Range` / `416` for seekable media or resumable downloads | `autumn_web::range` (`resolve` + `partial_bytes_response`) and `Download::into_response_ranged(&headers).await` — RFC 7233 single-range parsing, multi-range single-range collapse, `If-Range` via `.etag(..)`/`.last_modified(..)`, blob slices via `BlobStore::get_range` (no whole-object buffering) (unreleased) |
| Hand-written RSS/Atom XML strings for a `/feed.xml` or podcast/blog feed | `feed::Feed::atom(..)` / `feed::Feed::rss(..)` + `feed::FeedEntry` — builds the XML, implements `IntoResponse` with the right `application/atom+xml`/`application/rss+xml` type, XML-escapes text, and `Feed::conditional(&headers)` reuses the `etag` layer for `304`s (unreleased). See `docs/guide/conditional-get.md` |
| Hand-assembled `Cache-Control` header strings on a handler | `etag::cache_for(Duration)` → `CacheControl`; attach as a tuple `(cache_for(dur).public(), html!{…})` or `.wrap(resp)`. Chain `public`/`private`, `max_age`, `s_maxage`, `stale_while_revalidate`, `no_store`, `no_cache`, `must_revalidate`, `immutable`; `header_value()` renders a deterministic value. Defaults to `private` (a secured page can't be silently made public); composes with `fresh_when` — the directives ride the `200` and the preserved `304` (unreleased — issue #1344). See `docs/guide/conditional-get.md` |

When none of these fit, dropping to raw Axum (`.merge()`/`.nest()`/`.layer()`)
or raw Diesel (`&mut *db` with `diesel_async`) is supported and fine — but only
after checking this table and `docs/guide/`.

## Crate naming trip wires

| Concept | Name |
|---|---|
| Main library crate on crates.io | `autumn-web` |
| Rust import path | `autumn_web::` |
| Workspace member directory | `autumn/` |
| CLI crate | `autumn-cli` |
| CLI binary | `autumn` |
| Proc macro crate | `autumn-macros` |
| Admin plugin crate | `autumn-admin-plugin` |
| S3 storage plugin crate | `autumn-storage-s3` |
| Redis cache plugin crate | `autumn-cache-redis` |
| Search plugin crate | `autumn-search` |
| Main entry macro | `#[autumn_web::main]`, not `#[autumn::main]` |

The name `autumn` is the CLI binary, not the framework crate. In code, import
from `autumn_web::prelude::*`.

## Project shape

```text
my-app/
├── src/
│   ├── main.rs        # AppBuilder, migrations, routes, jobs, tasks, plugins
│   ├── models.rs      # Diesel models or #[model]
│   ├── schema.rs      # Diesel table! definitions
│   ├── routes/        # #[get], #[post], #[ws], #[static_get] handlers
│   ├── jobs.rs        # #[job] request-triggered background work
│   └── tasks.rs       # #[scheduled] and #[task] operational work
├── migrations/
├── static/
├── Cargo.toml
├── autumn.toml
└── autumn-dev.toml    # legacy profile file; [profile.dev] also works
```

## Cargo.toml

```toml
[package]
name = "my-app"
version = "0.1.0"
edition = "2024"

[dependencies]
autumn-web = { version = "0.5", features = ["db", "htmx", "maud"] }
chrono = { version = "0.4", features = ["serde"] }
diesel = { version = "2", features = ["postgres", "chrono"] }
diesel-async = { version = "0.8", features = ["postgres"] }
diesel_migrations = "2"
maud = { version = "0.27", features = ["axum"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tokio = { version = "1", features = ["full"] }
validator = { version = "0.20", features = ["derive"] }
```

Use `pq-sys = { version = "0.7", features = ["bundled_without_openssl"] }`
when avoiding a system libpq install.

For a reddit-clone-style app with live feeds, file uploads, and blob variants:
```toml
autumn-web = { version = "0.5", features = [
    "mail",       # transactional email + mailer previews
    "ws",         # WebSocket routes, SSE, broadcast channels
    "presence",   # Presence extractor for online-user tracking
    "storage",    # BlobStore + Blob columns + signed URLs
    "multipart",  # multipart/form-data file uploads
    "redis",      # Redis sessions, channels, and job backend
    "variants",   # blob.variant(...) image transformation
] }
```

## Feature flags

Defaults: `maud`, `htmx`, `tailwind`, `db`, `cache-moka`.

| Feature | Purpose |
|---|---|
| `ws` | WebSocket routes, SSE helpers, local/Redis broadcast channels |
| `flash` | Flash messages |
| `multipart` | Multipart uploads |
| `redis` | Redis sessions, channels, jobs, webhook replay, and integration points |
| `oauth2` | OAuth2/OIDC helpers and `autumn generate auth --oauth` scaffolding |
| `openapi` | OpenAPI route metadata and spec generation |
| `mcp` | Project typed JSON endpoints as MCP tools; implies `openapi` |
| `markdown` | Markdown rendering with frontmatter and static-site support |
| `telemetry-otlp` | OpenTelemetry OTLP export |
| `test-support` | Testcontainers-backed `TestApp`, `TestClient`, and `TestDb` |
| `i18n` | Locale extractor and compile-time checked translations |
| `storage` | `BlobStore`, local storage, `Blob` columns, signed URLs |
| `mail` | Transactional email, mailer macros, previews, deferred delivery |
| `seed` | `SeedContext` for seed binaries |
| `system-info` | Optional system information in actuator surfaces |

For S3 storage add `autumn-storage-s3 = "0.5"`; `storage-s3` is no longer an
`autumn-web` feature. For a shared Redis cache add `autumn-cache-redis = "0.5"`.
For keyword + vector search with lifecycle-synced indexes add `autumn-search`.

## main.rs pattern

```rust
mod jobs;
mod routes;
mod schema;
mod tasks;

use autumn_web::migrate::{embed_migrations, EmbeddedMigrations};
use autumn_web::prelude::*;

const MIGRATIONS: EmbeddedMigrations = embed_migrations!();

#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .migrations(MIGRATIONS)
        .routes(routes![routes::index, routes::create_post])
        .tasks(tasks![tasks::refresh_rankings])
        .jobs(jobs![jobs::send_welcome_email])
        .run()
        .await;
}
```

Use `.one_off_tasks(one_off_tasks![...])` for `#[task]` handlers invoked by
`autumn task <name>`.

## AppBuilder API

| Method | Purpose |
|---|---|
| `.routes(routes![...])` | Register route handlers |
| `.static_routes(static_routes![...])` | Register `#[static_get]` routes for `autumn build` |
| `.tasks(tasks![...])` | Register scheduled `#[scheduled]` work |
| `.jobs(jobs![...])` | Register request-triggered `#[job]` work |
| `.one_off_tasks(one_off_tasks![...])` | Register operational `#[task]` commands |
| `.migrations(MIGRATIONS)` | Register embedded Diesel migrations |
| `.plugin(plugin)` / `.plugins((...))` | Install first- or third-party plugins |
| `.openapi(config)` | Configure OpenAPI generation |
| `.policy::<R, _>(policy)` / `.scope::<R, _>(scope)` | Register repository API authorization |
| `.scoped(prefix, layer, routes)` | Mount a scoped route group |
| `.merge(router)` / `.nest(path, router)` | Attach raw Axum routers |
| `.layer(layer)` | Add Tower middleware |
| `.error_pages(renderer)` / `.exception_filter(filter)` | Customize error rendering |
| `.with_config_loader(loader)` | Replace TOML + env config loading |
| `.with_pool_provider(provider)` | Replace database pool creation |
| `.with_session_store(store)` | Replace sessions |
| `.with_channels_backend(backend)` | Replace broadcast channels |
| `.with_blob_store(store)` | Install a file storage backend |
| `.with_cache_backend(cache)` | Install a cache backend |
| `.with_mail_delivery_queue(queue)` | Install durable deferred mail |
| `.with_mail_suppression_store(store)` | Install a durable bounce/complaint suppression backend (unreleased; in-memory default auto-wired otherwise) |
| `.with_audit_sink(sink)` | Install structured audit sink |
| `.listeners(listeners![...])` | Register `#[listener]` event listeners (unreleased) |
| `.static_gate(layer)` | Middleware that also guards `#[static_get]` pre-render (unreleased; `has_static_gate::<L>()`, `get_static_gate_types()`, `TestApp::static_gate` mirror it) |
| `.with_shard_router(router)` | Install a shard router for `[[database.shards]]` (unreleased) |
| `.run()` | Launch the server |

## Route macros

```rust
#[get("/posts")]
async fn list(db: Db) -> AutumnResult<Markup> { /* ... */ }

#[get("/posts/{id}")]
async fn show(Path(id): Path<i64>, db: Db) -> AutumnResult<Markup> { /* ... */ }

#[post("/posts")]
#[secured]
async fn create(db: Db, Valid(Form(input)): Valid<Form<CreatePost>>) -> AutumnResult<Markup> {
    /* ... */
}

#[patch("/posts/{id}")]
#[secured]
async fn patch(Path(id): Path<i64>, db: Db) -> AutumnResult<Markup> { /* ... */ }

#[delete("/posts/{id}")]
#[secured]
async fn delete_post(Path(id): Path<i64>, db: Db) -> AutumnResult<Markup> { /* ... */ }

#[static_get("/about")]
async fn about() -> Markup { html! { h1 { "About" } } }

#[ws("/socket")]
async fn ws() -> impl autumn_web::ws::WsHandler {
    |mut socket: autumn_web::ws::WebSocket| async move {
        while let Some(Ok(msg)) = socket.recv().await {
            if let autumn_web::ws::Message::Text(text) = msg {
                socket.send(autumn_web::ws::Message::Text(text)).await.ok();
            }
        }
    }
}
```

Route functions are collected with `routes![...]`. Static routes also need
`static_routes![...]` so `autumn build` can pre-render them.

**Route-level SEO defaults (issue #1182, unreleased — trunk-dev):** declare
per-page meta tag values once on the route with a `seo(...)` argument instead of
rebuilding a `SeoMeta` in every handler. `SeoMeta` is an extractor, so a handler
that takes one receives a builder pre-populated with the declared values:

```rust
use autumn_web::seo::SeoMeta;

#[get("/about", seo(title = "About • My Blog", description = "Learn about us"))]
async fn about(seo: SeoMeta) -> Markup { html! { head { (seo.render()) } } }

// Static default on the attribute, dynamic fields filled in by the handler.
// The builder is consuming, so the handler's value wins for the keys it
// touches while the untouched attribute keys survive.
#[get("/posts/{slug}", seo(og_type = "article"))]
async fn show(Path(slug): Path<String>, seo: SeoMeta, db: Db) -> AutumnResult<Markup> {
    let post = Post::find_by_slug(&slug, db).await?;
    let seo = seo.title(format!("{} • Blog", post.title));
    Ok(layout_with_seo(seo, html! { /* ... */ }))
}
```

Keys mirror the `SeoMeta` builder: `title`, `description`, `canonical`,
`og_title`, `og_description`, `og_image`, `og_type`, `og_url`, `twitter_card`,
`twitter_title`, `twitter_description`, `twitter_image`, `robots`. Values must
be string literals; an unknown or repeated key is a compile error. `#[static_get]`
accepts the same argument, so pre-rendered pages carry the tags. The extractor
never fails — on a route without `seo(...)` it yields an empty builder. Note the
attribute supplies *values*, not markup: the handler still emits them, normally
via `seo.render()` inside the layout's `<head>`.

**Duplicate-route preflight (issue #1012, unreleased — trunk-dev):** two
handlers that resolve to the same `(method, path)` after `.scoped(...)` prefix
resolution — including `#[repository]`-generated API routes — fail app build
with `RouterBuildError::DuplicateUserRoute { method, path, existing, incoming }`
BEFORE any router mounts, instead of the previous `axum::MethodRouter::merge`
startup panic. Distinct methods on the same path (`GET /admin` + `POST /admin`)
still merge cleanly. Opaque `.merge(...)`/`.nest(...)` routers can't be
introspected — a non-empty opaque table emits a `tracing::warn!`
("check skipped") rather than a false pass. See
`docs/guide/getting-started.md` "Route collision diagnostics".

## Models and repositories

Autumn uses Diesel + diesel-async for Postgres. Primary keys are `i64` /
`BIGSERIAL`; do not use UUIDs as primary keys. Add UUIDs as separate columns
when external correlation needs them.

```rust
// #[model] and #[repository] are NOT in prelude — use qualified paths:
#[autumn_web::model(table = "posts")]
#[derive(Validate)]
pub struct Post {
    pub id: i64,
    #[validate(length(min = 1, max = 500))]
    pub title: String,
    pub body: String,
}
```

Repository-generated APIs in production must either declare a policy or be
explicitly acknowledged in config:

```rust
#[autumn_web::repository(Post, api = "/api/posts", policy = PostPolicy, scope = PostScope)]
pub trait PostRepository {}
```

```toml
[security]
allow_unauthorized_repository_api = true # only when intentional
```

### Associations and preloading (unreleased — trunk-dev, not in published 0.5.0)

Declare `#[belongs_to]` / `#[has_many]` / `#[has_one]` on a `#[model]` for
batched eager loading — no N+1, no lazy loading (un-preloaded access returns
typed `NotLoaded`, never issues SQL):

```rust
#[autumn_web::model]
#[belongs_to(User, fk = author_id)]
#[has_many(Comment)]
#[has_many(Tag, through = post_tags)] // many-to-many join table
pub struct Post {
    #[id]
    pub id: i64,
    pub author_id: i64,
    pub title: String,
}

let posts = repo.find_all().await?;
let posts = repo.preload(posts, Post::preload().author().comments().tags()).await?;
for post in &posts {
    let author = post.author()?;     // Result<Option<&Preloaded<User>>, NotLoaded>
    let comments = post.comments()?; // Result<&[Preloaded<Comment>], NotLoaded>
    let tags = post.tags()?;         // Result<&[Arc<Preloaded<Tag>>], NotLoaded>
}
```

`through = <join_table>` on `#[has_many]` declares a many-to-many
association: join columns default to `{source}_id` / `{target}_id`
(`fk = ...` / `target_fk = ...` to override), the macro emits the join
table's `diesel::table!` itself (only a migration with a composite primary
key on both columns is needed — no `schema.rs` entry), and the repository
gets `add_{singular}` / `remove_{singular}` / `set_{plural}` mutation
helpers, each idempotent:

```rust
repo.add_tag(post_id, tag_id).await?;    // ON CONFLICT DO NOTHING
repo.remove_tag(post_id, tag_id).await?; // no-op if unlinked
repo.set_tags(post_id, &tag_ids).await?; // replace-all, one transaction
```

See `examples/reddit-clone` (`Post` ↔ `Tag` via `post_tags`) for a full
worked example, and `docs/adr/0008-associations-and-eager-loading.md` for
the design.

Deleting a parent can cascade to its children in one transaction — declare
`dependent(...)` on the parent's `#[repository]` instead of hand-writing the
child cleanup **(unreleased — trunk-dev)**:

```rust
#[autumn_web::repository(Post,
    dependent(CommentRepository, fk = "post_id", on_delete = destroy))]
pub trait PostRepository {}
```

`on_delete` = `destroy` (soft-delete-aware, fires each child's hooks; on
trunk-dev this **now recurses** into each child's own `dependent`, cascading
into grandchildren — see the recursive/bulk/model-side note below) |
`delete_all` (bulk delete, no child hooks) | `nullify` (set the FK null) |
`restrict` (probe for referencing rows *before* mutating; errors `cannot delete:
dependent N row(s) …` if any still exist). The generated `delete_by_id` loads and
locks the parent, applies every declared action, then deletes the parent — all in
a single transaction (Part of #1369). See `docs/guide/repositories.md`.

On trunk-dev the cascade is recursive and bulk-aware, and can be declared on
the model instead of the repository **(unreleased — trunk-dev, issues #1738,
#1739, #1740)**:

- **Grandchildren cascade.** `destroy` now recurses — each destroyed child runs
  its OWN `dependent(...)` before its row is removed, so a `Post -> Comment ->
  Reply` graph clears `Reply` rows too, all in one transaction. A self- or
  mutually-referential graph terminates via a `(table, id)` cycle guard rather
  than looping; `delete_all` stays single-level by design (issue #1739).
- **Bulk `delete_many`.** The generated `delete_many` runs the same
  restrict/destroy/`delete_all`/`nullify` cascade per affected parent inside its
  transaction — restrict probes first (a `409` rolls the whole batch back) — so
  bulk-deleting parents no longer orphans or FK-errors dependent children
  (issues #1740, #1787).
- **Model-side declaration.** Declare the cascade on the association instead of
  the repository attribute: `#[has_many(Comment, dependent = destroy)]` (or
  `on_delete = destroy`). The repository codegen consults this at run time (via
  a generated `Model::dependents()`) when no repository-side `dependent(...)` is
  present; the repository attribute wins when both are declared. Ships for
  `destroy`, `delete_all`, `nullify`, and `restrict`, grandchild recursion and
  the `delete_many` bulk path included. When both a repository `dependent(...)`
  and a model-side `#[has_many(..., dependent=...)]` are declared the repository
  attribute still wins, but a debug-only `tracing::warn!` (emitted in
  `debug_assertions` builds) now surfaces the silently-inert model-side
  declaration instead of dropping it without a trace (issue #1788).
  `dependent`/`on_delete` on a `through = <join_table>` association is a compile
  error (issue #1738).

See `docs/guide/repositories.md`.

### Model state machines — `#[state_machine]`

**Never hand-roll status-transition validation.** Declare valid transitions on
the model field; the macro generates the transition table and enforcement:

```rust
#[autumn_web::model]
pub struct Order {
    #[id]
    pub id: i64,
    pub amount: i64,
    #[state_machine(transitions(
        pending -> processing,
        processing -> shipped: "can_ship",   // quoted guard method name
        processing -> cancelled,
        shipped -> delivered,
    ))]
    pub status: String,
}

impl Order {
    fn can_ship(&self) -> bool { self.amount > 0 }  // guard: &self -> bool
}
```

For a field named `status` this generates on the struct:

| Item | Signature |
|---|---|
| `can_transition_status_to` | `(&self, target: &str) -> bool` — edge exists and guards pass |
| `transition_status_to` | `(&self, target: &str) -> AutumnResult<String>` — `Ok(target)` or a 400 `bad_request` error |
| `__AUTUMN_SM_STATUS_TRANSITIONS` | `&'static [(&'static str, &'static str, Option<&'static str>)]` — `(from, to, guard)` edge list for UI/API metadata |

Rules: `String` fields only; state and guard names are plain identifiers
(`in_progress`, not `in-progress`); guards are quoted names of `&self -> bool`
methods on the model; multiple `#[state_machine]` fields per model are fine
(each gets its own `{field}`-named methods).

The idiomatic enforcement point is a repository `before_update` hook — this
replaces any hand-written status `match` block:

```rust
async fn before_update(
    &self,
    _ctx: &mut MutationContext,
    draft: &mut UpdateDraft<Order>,
) -> AutumnResult<()> {
    if draft.after.status != draft.before.status {
        // Guards must see the proposed content, but the edge lookup needs
        // the *current* status — clone after, restore the before-status:
        let mut proposed = draft.after.clone();
        proposed.status = draft.before.status.clone();
        proposed.transition_status_to(&draft.after.status)?;
    }
    Ok(())
}
```

Instead of inline `transitions(...)`, a field can reference a reusable
`#[lifecycle]` enum — `#[state_machine(lifecycle = OrderState)]` — whose typed
edges become the field's transition table (#1911/#1916). The `autumn lifecycle
check` CLI command statically verifies every `#[lifecycle]` state machine
(referenced-state existence, reachability, that every non-terminal state can
reach a terminal one; exits non-zero when unsound), and `autumn lifecycle
diagram` emits a Graphviz DOT or Mermaid `stateDiagram-v2` per lifecycle.

See `docs/guide/state-machines.md` and `examples/wiki` (`Page` model,
`draft → published → archived`).

### Generated repository surface

`#[repository]` generates far more than CRUD — reach for these before writing
raw Diesel:

| Method / attr key | Purpose |
|---|---|
| `find_by_id`, `find_all`, `count`, `exists_by_id`, `save`, `update`, `delete_by_id` | Core CRUD |
| `page(&PageRequest)` | Offset pagination (`?page=N&size=M`, clamped, never 400) |
| `cursor_page(&CursorRequest)` + `cursor_key = field` (opt. `cursor_key_type =`) | Keyset pagination for feeds/large tables; `cursor_key` must be non-nullable |
| `save_many`, `save_many_skip_invalid`, `update_many`, `delete_many`, `upsert_many` | Bulk ops, hook-aware, auto-chunked under the 65,535-param ceiling; `upsert_many` is a compile error on hooked repositories |
| `with_lock` | `SELECT ... FOR UPDATE` row locking in a transaction |
| `primary_reads` (attr) / `on_primary()` | Pin reads to primary; read-your-writes after a save |
| `soft_delete`, `tenant_scoped` (attrs), `across_tenants()` | Soft deletion and tenant scoping |
| `hooks = MyHooks` (attr) | `before_/after_create/update/delete` + `after_*_commit` lifecycle hooks with `MutationContext` |
| `from_shard(&ShardedDb)`, `with_pool_untracked(pool)` | **(unreleased)** shard-scoped construction; `with_pool_untracked` is new on trunk-dev — published 0.5.0 repositories have **no** pool constructor at all |
| `find_in_batches(batch_size)`, `find_each(batch_size)` | **(unreleased)** Bounded-memory whole-table iteration via a primary-key keyset cursor (`WHERE id > last ORDER BY id ASC LIMIT batch_size` — never `LIMIT`/`OFFSET`), generated on every repository. `find_in_batches` returns a `FindInBatches` handle — drive with `while let Some(chunk) = b.next_batch().await?`; `find_each` returns `FindEach` yielding one model per `next().await?`. Inherits soft-delete filtering, tenant scoping, and read routing like `find_all`; errors are retryable (cursor advances only on success; `Ok(None)` always means completion); `batch_size == 0` errors instead of spinning; `batch_size` is **not** clamped to `MAX_PAGE_SIZE`; sharded repos reject cross-shard `across_tenants()` iteration (iterate per shard via `from_shard`). Handle types: `autumn_web::batches::{FindInBatches, FindEach, BatchSource}` (not in the prelude). See "Batched iteration" in `docs/guide/pagination.md` |
| `find_or_create_by_<field>[_and_<field>...](<field>, &new)` | **(unreleased)** Race-safe get-or-insert; declare `fn find_or_create_by_slug(slug: String);` (lookup fields only) to generate an inherent `find_or_create_by_slug(&self, slug: String, new: &NewModel) -> AutumnResult<(Model, bool)>`. Reads on the read path first (tenant/soft-delete aware), else inserts on the primary with `ON CONFLICT DO NOTHING` — under concurrency exactly one row is created, exactly one caller sees `created == true`, and no `23505` escapes. `before_/after_create` + commit hooks fire only on the created path; works on hooked repos (unlike `upsert_many`). **Requires a unique constraint on the lookup column(s)** (`_or_` is rejected). See "Race-safe get-or-insert" in `docs/guide/repositories.md` |

Read routing: with `database.replica_url` set, all generated reads use the
replica automatically; writes always hit the primary. See
`docs/guide/repositories.md` and `docs/guide/pagination.md`.

### JSON API endpoints — page envelope + write validation (unreleased — trunk-dev)

A `#[repository(api = "/api/posts")]` list endpoint returns a page envelope, and
create/update handlers validate the decoded payload before touching the DB:

- **List** — `GET /api/posts?page=1&size=20` returns
  `{ content: [...], page, size, total_elements, total_pages, has_next,
  has_previous }` (`page` is 1-based, default 1; `size` clamped). Custom
  handlers can use `filter.page()` / `filter.size()` / `filter.limit()` /
  `filter.offset()`.
- **Create/update** — the decoded write payload runs the model's
  `#[validate(...)]` rules first; on failure the handler returns **422 Problem
  Details** with a per-field `errors` map instead of inserting. Models without a
  `Validate` derive compile to a no-op via the autoref `MaybeValidate`
  specialization (no migration burden), and this applies to plain and
  policy-backed handlers (#1237, #1253). See `docs/guide/pagination.md`.

### Partial-update validation — the effective merged model (unreleased — trunk-dev, issue #1778)

On a repository with `hooks = ...`, a `PATCH`/`PUT` update now validates the
**effective merged model** — the existing row ∪ the patch, after normalization —
not only the patch struct's own fields. `#[model]` derives `validator::Validate`
on the read model (gated on `has_validation`, symmetric with the `New*`/`Update*`
models) and keeps the full `#[validate(...)]` set there; the generated
`from_patch` validates the reconstructed concrete model before returning the
draft, running before `before_update` (mirroring create, where validation runs
before `before_create`). Because the merged model's fields are concrete `T`
rather than `Patch<T>`, validators that cannot be expressed on `Patch<T>` — `ip`
on `Option` fields and `does_not_contain` (E0119 trait-coherence walls under
validator `0.20`), plus the cross-field `custom`/`must_match`/`nested` (no
single-field `Patch<T>` trait) — are now enforced on update, returning the same
**422** field-error map as create.

This covers every update path that builds a draft via `from_patch` (hooked
repositories and their `--api` handlers). The blind `__to_changeset` update paths
(plain/`api`/`policy` repositories without hooks) still run only the patch-struct
validators (follow-up: issue #1801). The `Patch<T>` per-field impls and the
`UpdateModel` denylist are unchanged, so this is backward compatible (issue
#1778). See `docs/guide/repositories.md`.

### Transactions

`Db::tx(f)` runs a READ COMMITTED transaction. On trunk-dev
**(unreleased)**, `Db::tx_with(opts, f)` adds isolation levels and automatic
retry of serialization failures (SQLSTATE 40001) with capped exponential
backoff:

```rust
use autumn_web::db::{TxOptions, IsolationLevel};

let opts = TxOptions::serializable()      // or ::repeatable_read(), ::read_committed()
    .read_only()
    .max_attempts(8);                     // serializable()/repeatable_read() default to 5
db.tx_with(opts, |conn| async move { /* &mut AsyncPgConnection */ }.scope_boxed()).await?;
```

`TxOptions::default()` is identical to `Db::tx`. See
`docs/guide/transactions.md` and `docs/guide/hooks-and-transactions.md`.

## Security and auth

```rust
#[get("/dashboard")]
#[secured]
async fn dashboard(session: Session) -> AutumnResult<Markup> { /* ... */ }

#[get("/admin")]
#[secured("admin")]
async fn admin_panel() -> AutumnResult<Markup> { /* ... */ }

// Record-level auth on repository-generated REST endpoints:
#[autumn_web::repository(Post, api = "/api/posts", policy = PostPolicy, scope = PostScope)]
pub trait PostRepository {}

// Manual handler: load the record first, then check inline.
// #[authorize] is used by the repository macro; for manual handlers
// the pattern is explicit ownership checks in the body:
#[post("/posts/{id}")]
#[secured]
async fn update_post(Path(id): Path<i64>, mut db: Db, session: Session) -> AutumnResult<Markup> {
    // session.get() returns Option<String>; parse to i64
    let user_id: i64 = session.get("user_id").await
        .ok_or_else(|| AutumnError::unauthorized_msg("Login required"))?
        .parse()
        .map_err(|_| AutumnError::bad_request_msg("Invalid session"))?;
    let post = find_post(&mut *db, id).await?;
    if post.user_id != user_id {
        return Err(AutumnError::forbidden_msg("not your post"));
    }
    /* ... */
    Ok(html! { "updated" })
}
```

**(unreleased)** Scoped service tokens (trunk-dev only): mint named,
optionally-expiring API tokens carrying flat scopes via `IssueTokenSpec` +
`issue_scoped_api_token`; gate handlers with
`#[secured(scopes = ["posts:write"])]` (no session required — default-deny,
403 when missing) or `#[secured("admin", scopes = [...])]` for both; check in
policies with `PolicyContext::has_scope/has_any_scope/has_all_scopes`; manage
with `autumn token issue <principal> --name ... --scope ...` / `list` /
`rotate`. The
published 0.5.0 `autumn token` has only `issue <principal>` / `revoke`.

Active session management ships with `autumn generate auth` (published 0.5.0):
a `{user}_sessions` row per login, generated `sessions()`,
`revoke_session(id)`, `revoke_other_sessions(...)`, `revoke_all_sessions()`
methods, an `/account/sessions` Maud + htmx page, and `[auth.sessions]`
config (incl. `revoke_on_credential_change`, default on).

In `prod` / `production`, configure a stable signing secret or startup fails:

```bash
export AUTUMN_SECURITY__SIGNING_SECRET="$(openssl rand -hex 32)"
```

For rotation, set `[security.signing_secret].previous_secrets` until old
cookies, CSRF tokens, flash state, and signed storage URLs expire.

### Audit actor attribution (unreleased — trunk-dev)

Version/audit writes are auto-attributed to the current actor — no per-call
plumbing. The log-context middleware opens an empty actor scope per request; set
the authenticated user once and every `VersionEntry.actor` (via
`MutationContext::actor`) records it:

```rust
use autumn_web::current::Current;

Current::set_actor(format!("user:{user_id}")); // ambient, task-local, request-scoped
// any repository mutation in this request now records this actor on its VersionEntry
```

For jobs and the scheduler (no request scope), set a process-wide default with
`Current::set_default_actor(...)`, or run a bounded scope via
`autumn_web::current::with_actor(actor, async { ... })`. Unset → `actor` is
recorded as `"system"` — the `_autumn_version_history.actor` column is `NOT NULL
DEFAULT 'system'` and the generated code falls back to `VersionEntry::SYSTEM_ACTOR`
(#1383). See `docs/guide/version-history.md`.

## OAuth2/OIDC scaffolding

OAuth2/OIDC social login is in the 0.5.0 line. Do not repeat the stale
changelog claim that it was reverted; the revert was followed by a reapply and
review fixes. Prefer the current tree and `docs/guide/oauth.md` over that old
summary line.

```bash
autumn generate auth User --oauth github,google
```

The generator creates `src/routes/oauth.rs`, an `oauth_identities` migration,
login buttons, and `[auth.oauth2.<provider>]` config stubs. The flow uses
PKCE S256, state validation, OIDC nonce validation, and provider presets for
GitHub, Google, and Microsoft. OAuth support stays behind the `oauth2` feature.

## Signed webhooks

Autumn 0.4.0 added `SignedWebhook` for Stripe, GitHub, Slack, and generic
HMAC callbacks. The extractor verifies the exact raw body bytes and replay key
before handler logic runs. Timestamp freshness is **provider-specific**: Stripe
(signature `t=` field) and Slack (`X-Slack-Request-Timestamp`) validate staleness
automatically; GitHub and generic-HMAC providers only check the body signature
unless you add `timestamp_header` to the `[webhooks.<name>]` config block
explicitly.

```rust
#[post("/webhooks/stripe")]
async fn stripe(webhook: SignedWebhook) -> AutumnResult<Json<serde_json::Value>> {
    let event: serde_json::Value = webhook
        .json()
        .map_err(|err| AutumnError::bad_request_msg(format!("invalid JSON: {err}")))?;

    Ok(Json(serde_json::json!({
        "accepted": true,
        "provider": webhook.provider(),
        "delivery_id": webhook.delivery_id(),
        "event_type": webhook.event_type(),
        "event": event,
    })))
}
```

Production replay protection should use Redis:

```toml
[security.webhooks.replay]
backend = "redis"

[security.webhooks.replay.redis]
url = "redis://redis:6379/0"
key_prefix = "myapp:webhooks:replay"
```

Read `docs/guide/signed-webhooks.md` and `examples/signed-webhooks/`.

## Mail CSS inlining — render styled in Gmail/Outlook (unreleased — trunk-dev)

Gmail strips `<style>` in many contexts and Outlook (Word engine) ignores
`<head>`/`<style>` and most external CSS, so email authored with a stylesheet
arrives **unstyled**. The decades-old fix is to inline CSS onto elements at send
time. Autumn does this for you (issue #1254).

- **Author with a `<style>` block + classes**, then opt in — either per message
  or per environment:
  ```rust
  // Per message (wins over the config default in both directions):
  Mail::builder()
      .to(to).subject("Welcome")
      .html(r#"<style>.btn{color:#fff;background:#06c}</style><a class="btn">Go</a>"#)
      .inline_css(true)      // ← rewrites .btn onto the <a> as style="…" at send
      .build()?
  ```
  ```toml
  # Per environment — default it on for every mailer:
  [mail]
  inline_css = true
  ```
- **Precedence:** an explicit `MailBuilder::inline_css(true|false)` always beats
  the `mail.inline_css` default (`inline_css(false)` opts one message out of an
  environment that defaults on). Default is **off** — existing apps are
  unaffected until they opt in.
- **What's preserved:** un-inlinable `@media`/pseudo-class rules stay in a
  retained `<style>` block; text parts and bodies with no `<style>` pass through
  unchanged; inlining is idempotent (running it twice equals once).
- **Failure is loud, not corrupting:** on an inliner error the original body is
  kept and a typed `MailError::CssInline` is surfaced rather than delivering
  malformed HTML.
- `autumn generate mailer` scaffolds this end to end (a `<style>` template + a
  mailer that calls `.inline_css(true)`).

## Mail suppression — stop emailing bounced addresses (unreleased — trunk-dev)

Autumn *detects* delivery failure (inbound bounce/complaint webhooks) **and**
acts on it: `Mailer::send()` consults a bounce/complaint suppression list
**before** transport and skips addresses that hard-bounced or complained. This
protects sender reputation (re-sending to a hard bounce is what gets a domain
throttled by Gmail/Microsoft/SES). Distinct from recipient-initiated
List-Unsubscribe (`#[mailer(list_unsubscribe)]`): that is a user opt-out;
this is *provider-reported* failure.

- **Zero-config:** an in-memory `SuppressionStore` is auto-wired; the loop works
  out of the box on a single instance. For multi-instance deploys register the
  durable Postgres backend (shared across replicas):

  ```rust
  use autumn_web::mail::suppression::PgSuppressionStore;
  App::builder().with_mail_suppression_store(PgSuppressionStore::new(pool))
  ```

  > **You must create the `mail_suppressions` table yourself** —
  > `PgSuppressionStore` ships no migration (same convention as the
  > List-Unsubscribe `mail_unsubscribes` store, whose table is written by
  > `autumn generate mailer --list-unsubscribe`). Add a migration with:
  > ```sql
  > CREATE TABLE mail_suppressions (
  >     address       TEXT PRIMARY KEY,
  >     reason        TEXT NOT NULL,
  >     suppressed_at TIMESTAMPTZ NOT NULL DEFAULT now()
  > );
  > ```
  > Without it, every send errors on the suppression lookup.

- **Close the loop** — receive provider bounce webhook → suppress → later send
  is skipped. The inbound handler and the `Mailer` must consult the **same**
  store. `with_mail_suppression_store` takes a store *by value* and wraps it in
  a fresh handle internally, so build **one** `InMemorySuppressionStore` and
  hand out `.clone()`s of it — the clone shares the same `Arc<Mutex<…>>` state
  (inbound handlers are plain `fn` pointers, so stash a handle in a `OnceLock`):

  ```rust
  use std::sync::OnceLock;
  use autumn_web::mail::suppression::{
      record_inbound, InMemorySuppressionStore, SuppressionStoreHandle,
  };

  static SUPPRESSION: OnceLock<SuppressionStoreHandle> = OnceLock::new();

  // ONE store; clones share the same underlying state.
  let store = InMemorySuppressionStore::new();
  SUPPRESSION
      .set(SuppressionStoreHandle::new(store.clone())) // inbound side
      .ok();
  let app = App::builder().with_mail_suppression_store(store); // Mailer side — same state

  // Inbound router: the provided handler records the bounce.
  InboundMailRouter::new()
      .endpoint(InboundMailEndpointConfig::mailgun("/mail/inbound", signing_key))
      .on_bounce(|email| Box::pin(async move {
          record_inbound(SUPPRESSION.get().unwrap().inner().as_ref(), &email).await?;
          Ok(())
      }));
  ```

  A bounce suppresses the provider-reported `email.bounced_address`
  (`SuppressionReason::HardBounce`). Afterwards `mailer.send(mail_to("a@x.com"))`
  returns `Err(MailError::AllRecipientsSuppressed)` (zero transport calls) while
  `b@x.com` still delivers.

  > **`on_spam` is an inbound spam *verdict*, not an outbound complaint.**
  > autumn's `on_spam` fires on a provider-side inbound spam flag
  > (`X-Mailgun-Sflag: Yes`) — it carries no outbound complainant address, and
  > `email.to` there is *your app's own inbound address*. So `record_inbound`
  > deliberately suppresses **nothing** for a bare spam verdict (it logs
  > instead), and never suppresses `email.to`. Only a genuine FBL complaint
  > that populates `InboundEmail::complained_address` suppresses that
  > complainant (`SuppressionReason::Complaint`). Wire a real feedback-loop
  > source to that field before routing complaints here.

- **Observability:** every skip logs `outcome = "skipped_suppressed"` and bumps
  `suppression::suppressed_skips()` — a suppressed drop is never silent.
- **Critical mail bypass:** `Mail::builder().….ignore_suppression()` delivers
  even to a suppressed address (password resets, MFA codes, security alerts).
- **Escape hatch:** `store.unsuppress(addr)` removes an address (e.g. the
  recipient fixed their mailbox); `store.suppress(addr, SuppressionReason::Manual)`
  adds one by hand.

## In-app notifications (unreleased — trunk-dev, issue #1148)

A first-class per-recipient notification store with read/unread state — do not
hand-roll a notifications table, model, and unread-count queries. Scaffold with
`autumn generate notifications` (backend-aware migration + feed routes + smoke
test), then use the `Notifications` extractor (in the prelude, surfaced like
`Session`):

```rust
use autumn_web::prelude::*;

#[post("/comments")]
async fn create_comment(notifications: Notifications) -> AutumnResult<&'static str> {
    notifications
        .notify(recipient_id, "comment.created", serde_json::json!({"post": 42}))
        .await?;
    Ok("ok")
}
```

- **API:** `notify(recipient_id, kind, payload)`; `list(recipient_id, &ListQuery,
  &PageRequest) -> Page<Notification>` (`?filter[unread]=true`,
  `?filter[kind]=…`, `?sort=created_at|id`, newest-first default);
  `unread_count(recipient_id)`; idempotent `mark_read(id)` /
  `mark_read_for(recipient_id, id)` / `mark_all_read(recipient_id)`. In
  user-facing handlers use `mark_read_for` — it refuses to touch other
  recipients' rows.
- **Storage resolution** (mirrors `SessionStore`): a store registered via
  `AppBuilder::with_notification_store(...)` → `DbNotificationStore` when a DB
  pool is configured (needs the generated `notifications` table) →
  `MemoryNotificationStore` (process-local; what `TestApp` without a DB uses,
  so generated smoke tests need no database).
- **Realtime push (`ws` feature):** `notify_with_push(...)` persists then
  publishes the notification JSON on `Notifications::topic(recipient_id)`
  (`"notifications:{id}"`) — best-effort, a channel failure never fails the
  notify. In the subscribing WS/SSE handler, derive the topic from the
  **authenticated** user (`Auth`/session), never a client-supplied id — topics
  are guessable and carry the full payload; use `subscribe_authorized` /
  `sse::stream_authorized` for channel-level enforcement.
- Guide: `docs/guide/notifications.md`. Out of scope by design: bell widget,
  email/SMS channels, preferences/digests, cross-recipient fan-out.

## Background work

Use built-in jobs and tasks before reaching for a workflow engine:

| Tool | Use for |
|---|---|
| `#[scheduled]` + `.tasks()` | Recurring app-local work; Postgres coordination is available for replicas |
| `#[job]` + `.jobs()` | Request-triggered background work with retries and local/Redis backends |
| `#[task]` + `.one_off_tasks()` | Operator-invoked CLI work via `autumn task` |
| Autumn Harvest | Durable multi-step workflows, activity retries, timers, and dedicated runners |

`autumn-admin-plugin` includes `/admin/jobs` for inspecting, retrying,
discarding, and canceling framework jobs. `GET /actuator/jobs` exposes
lower-level counters.

Job attributes beyond `name`/`max_attempts`/`backoff_ms` (published 0.5.0):
`#[job(unique)]` dedupes on an args hash, `unique_by = "field"`,
`unique_window = "running"|"pending"`, `unique_for_ms = N` (debounce),
`concurrency = N` + `concurrency_key = "field"` caps simultaneous runs. A
coalesced enqueue is a no-op `Ok(())`.

**(unreleased)** trunk-dev jobs additions:

- **Named queues**: `#[job(queue = "critical")]`; drain order via
  `[jobs] queues = ["critical", "default", "low"]` (strict priority) or a
  `[jobs.queues]` weight table (weighted, starvation-free). No `queue` →
  `"default"`. See `docs/guide/jobs.md`.
- **Tracked jobs**: `job::enqueue_tracked` / `enqueue_tracked_for` (and
  generated `{Job}::enqueue_tracked` companions) return a `TrackedJobHandle`
  with a public token; `#[job]` accepts an optional third `JobContext` arg
  (`async fn(AppState, Args, JobContext)`) with `set_progress(pct, msg)` /
  `set_result(json)` / `set_user_error(msg)`; poll `GET /_autumn/jobs/{token}`
  (JSON or self-polling htmx fragment). Config: `jobs.tracking.ttl_secs`
  (default 24h), `jobs.tracking.route_enabled`.
- **Versioned payloads**: opt into a payload schema version with
  `#[job(version = N, upgrade = upgrade_fn)]`. Autumn wraps the args in an
  `{ "__autumn_schema_version": N, "args": … }` envelope; the `upgrade` hook
  (`fn(u32, serde_json::Value) -> Result<Value, E>`) runs only for older stored
  payloads, so a rolling deploy drains the old queue instead of dead-lettering.
  A job with no `version` is stored raw (zero behaviour change). Helpers:
  `autumn_web::payload_version::{split_version, wrap}` (Closes #1205). See
  `docs/guide/jobs.md`.

**(unreleased)** Events & listeners — a typed domain event bus so one action
can fan out without inline coupling: `#[event]` on a struct, publish via the
`Events` extractor (`events.publish(UserSignedUp { .. }).await?`), react with
`#[listener(UserSignedUp)]` (sync, in-request) or
`#[listener(UserSignedUp, durable, max_attempts = 5)]` (jobs-backed), and
register with `.listeners(listeners![...])`. Do not also list durable
listeners in `jobs![...]`. See `docs/guide/events.md`.

## Distributed locks (unreleased — trunk-dev)

For "run this exactly once across replicas right now" work (nightly cleanup,
cache warming, one-shot backfills, "send the daily digest once"), use
`autumn_web::lock::Lock` (re-exported from the prelude) instead of hand-rolling
`pg_try_advisory_lock` raw SQL. It is the same Postgres advisory-lock machinery
that already gates migrations, `#[scheduled]` leader election, and ISR.

- Build: `Lock::from_state(&state, "name")?` (primary pool) or
  `Lock::new(pool, "name")`. Names hash to a stable, namespaced 64-bit key via
  `distributed_lock_key` (a `"autumn:lock:v1"` domain prefix keeps app keys out
  of the scheduler/migration/ISR/repository keyspaces).
- Acquire: `try_lock()` → `Option<LockGuard>` (non-blocking, `None` when held
  elsewhere); `lock()` blocks; `lock_timeout(dur)` blocks with a typed
  `LockError::Timeout`.
- Run-and-release closures: for **run-once-across-replicas** work (must not run
  twice) use `try_with(f)` → `Option<T>` — the winner runs the closure, every
  other replica gets `None` and skips (check `ran.is_none()`). The blocking
  `with(f)` / `with_timeout(dur, f)` instead *serialize* a mutually-exclusive
  section: they block until the holder releases and then run the closure, so
  every waiter eventually runs — they are **not** a run-once primitive. The lock
  auto-releases when the section ends — normal return, early `?`, or panic — and
  the lock-bearing connection is never recycled to the pool while held, so it
  cannot leak.
- Non-goals: not fair (no FIFO), not a lease (no heartbeat — use the scheduler
  for long-lived leader election), not row-level (use `with_lock`), Postgres
  only.

See `docs/guide/distributed-locks.md` and
`docs/adr/0010-app-facing-distributed-lock.md`.

## File storage and cache plugins

For local or pluggable file storage:

```toml
autumn-web = { version = "0.5", features = ["storage", "multipart"] }
autumn-storage-s3 = "0.5" # when storage.backend = "s3"
```

```rust
let store = autumn_storage_s3::S3BlobStore::from_config(&config.storage.s3)
    .await
    .expect("S3 store");
autumn_web::app().with_blob_store(store).run().await;
```

For keyword **and** vector search with an index that stays in sync:

```toml
autumn-search = "0.6"
```

```rust
#[autumn_web::model]
#[searchable(language = "english")]
pub struct Article {
    #[id] pub id: i64,
    #[searchable(weight = "A")] pub title: String,
    #[searchable(weight = "B", embed)] pub body: String,   // embed => vector search
}

// `#[repository(hooks = ...)]` takes a plain type NAME, so alias the generic.
type ArticleSearchHooks =
    autumn_search::SearchSyncHooks<Article, NewArticle, UpdateArticle>;

#[autumn_web::repository(Article, hooks = ArticleSearchHooks, commit_hooks = true)]
pub trait ArticleRepository {}

autumn_web::app()
    .plugin(
        autumn_search::SearchPlugin::new()
            .postgres()
            .embedder(std::sync::Arc::new(MyEmbedder))
            .index::<Article>(),
    )
    .run()
    .await;

// In a handler: the plugin installs the client as an AppState extension.
let search = state.extension::<autumn_search::SearchClient>().expect("SearchPlugin");
let page = search.search::<Article>("rust web", &page_req).await?;   // ranked Page
let hits = search.similar::<Article>("how do I add auth?", 5).await?; // k-NN
```

`autumn search reindex [--index NAME] [--purge]` rebuilds an index. See
`docs/guide/search.md`.

For shared Redis cache:

```toml
autumn-web = { version = "0.5", features = ["redis"] }
autumn-cache-redis = "0.5"
```

```rust
autumn_web::app()
    .plugin(autumn_cache_redis::RedisCachePlugin::new())
    .run()
    .await;
```

**(unreleased)** Cache stampede protection — do not hand-roll cache-aside
fills: `cache::get_or_compute(cache, key, Some(ttl), fill)` runs `fill` once
per process for concurrent callers; `get_or_compute_with` +
`GetOrComputeOptions::new().distributed_fill_lock(true)` (Redis lock) and
`.stale_while_revalidate(grace)` add cross-replica single-fill and
serve-stale; `cache::jittered_ttl(base, fraction)` de-synchronizes mass
expiry. A failed fill never poisons the key. See
`docs/guide/cache-stampede.md`.

**(unreleased)** Upload content-type validation — multipart uploads are
validated by **magic bytes**, not the spoofable client `Content-Type`. The
`Multipart` extractor sniffs the real type (`sniff_content_type`) whenever an
`allowed_content_types` allow-list is configured or strict mode is on. Reject
spoofs with:

```toml
[security.upload]
reject_on_content_type_mismatch = true  # declared-vs-sniffed mismatch (or an
                                        # unsniffable declared-binary) → rejected
```

(env `AUTUMN_SECURITY__UPLOAD__REJECT_ON_CONTENT_TYPE_MISMATCH`) (#1354).

## View helpers and widgets (unreleased — trunk-dev, not in published 0.5.0)

Trunk-dev ships framework view widgets — prefer these over hand-rolled Maud
for the common cases:

| Helper | Purpose |
|---|---|
| `link_to(label, href)` / `link_to_with(..., &LinkToOptions)` | Escaped GET anchors; auto `rel="noopener"` on `target="_blank"` |
| `button_to(label, href, Method, csrf_token)` / `button_to_with` | Single-button form for state-changing actions; CSRF is a required arg; non-GET emits hidden `_method` override |
| `card(&body, &CardConfig::new().title("..."))` / `stat_card(label, value, link)` | Titled panels and metric tiles (`autumn_web::widgets`, prelude re-export) |
| `sparkline(&points)` / `bar_chart(&series)` / `line_chart(&series)` (+ `_with(&ChartConfig)`) | Server-rendered, accessible, zero-JS **SVG charts** from `&[(&str, f64)]` (`/_stories` gallery). `bar_chart` anchors bars at zero; `ChartConfig::new().title(...).min(...).max(...)` sets an axis override / accessible name (#1231) |
| `tabs(id, &[(id, label, markup)])` | No-JS CSS-only tab switcher (`docs/guide/tabs.md`) |
| `modal(id, title, &body, &ModalConfig)` / `confirm_action(...)` | Native `<dialog>` confirm for destructive actions — replaces `hx-confirm`/`window.confirm()` |
| `badge(label, BadgeVariant::for_label(status))` / `status_tag(label)` | Semantic status pill; `BadgeVariant` = `Neutral`/`Info`/`Success`/`Warning`/`Danger`, `for_label` picks a deterministic color; `badge_with`/`BadgeConfig` set `title`/`aria-label`. Composes inside a `data_table` cell |
| `avatar(name, &AvatarConfig::new().image(url).size(AvatarSize::Small))` | Person chip: `<img>` (lazy, square, name `alt`) or a deterministic colored-initials fallback — never a broken image, no JS, no external call |
| `alert(AlertVariant::Info, body)` / `alert_with(..., &AlertConfig::new().title("...").icon(true).dismissible(true))` | Inline callout / empty-state / error box; `role` per variant, optional title + inline-SVG icon + no-JS dismiss. `error_summary(&changeset)` renders an `Error` alert of all field errors (or `None` when valid) |
| `flash_messages(&flash.consume().await)` (`autumn_web::flash`) | Accessible flash banners: per-severity `role`/`aria-live`, `autumn-flash--<level>` classes, empty slice renders nothing; `flash_messages_with` adds a no-JS dismiss |
| `pagination_nav(&page, &PagerOptions::new("/posts"))` / `cursor_pagination_nav` | Accessible, filter-preserving, htmx-opt-in pager from a `Page`/`CursorPage` (prelude re-export) |
| `toast(message, variant)` / `toast_region(DEFAULT_TOAST_REGION_ID)` / `toast_in(region_id, ...)` | Transient htmx action feedback: drop `toast_region` once in the layout, then return `toast(...)` next to your swapped fragment — it appends into the region OOB (`hx-swap-oob="beforeend:#toast-region"`). CSS-only auto-dismiss (no `<script>`); `variant` reuses `AlertVariant`. `toast_region` is a persistent `aria-live="polite"` region — non-error toasts inherit its politeness (no own `role`/`aria-live`); `Error` announces assertively via its own `role="alert"` |
| `infinite_feed(items, next_cursor, &FeedConfig)` / `feed_page(items, next_cursor, &FeedConfig)` | htmx infinite-scroll / "Load more" feed from a `CursorPage`: single `hx-get` sentinel carries the cursor and appends the next page in place (no reload, no duplicate rows). `FeedMode::{Reveal,Button}`; progressive `<a href>` fallback. `feed_page` is the append fragment a handler returns for each page (`page.next_cursor.as_deref()`) |
| `autumn_web::ui::WIDGETS_CSS` / `WIDGETS_CSS_PATH` | One shipped stylesheet backing every `autumn-*` widget class — link `href=(WIDGETS_CSS_PATH)` instead of copying widget CSS into `input.css`. Accent now follows `var(--primary)` (violet), not the old hardcoded indigo (`docs/guide/widget-styling.md`) |

### Whole-form rendering — `form_for` (unreleased — trunk-dev, not in published 0.5.0)

`#[model]` derives `autumn_web::form::FormModel` (one `FormField` per
`NewX`-editable column: humanized label, type-appropriate `FieldControl`,
`required` = non-`Option`), and `form_for` renders the entire `<form>` from a
`Changeset` in one call — opening tag with CSRF and hidden `_method` override
(same audited path as `form_tag`), pre-filled controls with inline per-field
errors, and a submit button. Import from `autumn_web::form` (not in the
prelude):

```rust
use autumn_web::form::{form_for, FieldControl};

// changeset: Changeset<Post> — blank for `new`, error-carrying on 422 re-render
let markup = form_for(&changeset, "/posts", "post") // non-GET/POST method emits hidden _method
    .csrf(&csrf_token)                              // .csrf_field_name(...) overrides "_csrf"
    .exclude("internal_notes")
    .override_field("status", FieldControl::Select {
        options: vec![("draft".into(), "Draft".into()), ("published".into(), "Published".into())],
    })                                              // last call wins per field
    .override_label("body", "Content")
    .submit_label("Publish")                        // default "Save"
    .render();
```

Derived control mapping: strings/`Uuid`/unknown types → `Text` (promote enums
via `.override_field`), integers → `Number` (step 1), floats/`Decimal` →
`Number` (step any), `bool` → `Checkbox` (nullable `bool` → tri-state
`Select`), `NaiveDate` → `Date`, `NaiveDateTime`/`DateTime<Utc>`/
`DateTime<Local>` → `DateTime`, any other `DateTime` zone → `Text` (an
offsetless `datetime-local` value is ambiguous there). Any
`FieldControl::File` field (or `.multipart()`) makes `render()` emit
`enctype="multipart/form-data"`.

Round-trip contracts are handled by the `#[model]`-generated `NewX`: unchecked
checkboxes decode as `false` (`#[serde(default)]` — `checkbox_input` emits no
hidden false fallback), and `datetime-local` submissions decode via the
`deserialize_datetime_local_utc[_option]` /
`deserialize_datetime_local_local[_option]` /
`deserialize_naive_datetime_local[_option]` helpers (which also still accept
RFC 3339 JSON bodies). Serde-renamed columns pre-fill correctly via
`FormField::value_name` (set automatically by the derive; hand-written
`FormModel` impls use `FormField::new(...).with_value_name(...)`). Trunk-dev
`autumn generate scaffold` views render through a single shared
`{snake}_form_for` helper built on this (except `--live-validation`, which
keeps per-field htmx emission).

For **master-detail** (has-many) forms saved atomically — an order plus its
line items in one submit — use `autumn_web::nested_form` (unreleased —
trunk-dev, #1915) instead of hand-wiring indexed field names: implement
`NestedChild` on the child new-model, render the child rows with
`inputs_for(&nested, &InputsForOptions, |row| …)` (add/remove rows + a
`destroy_checkbox` for deletes) off a `NestedChangesetForm<Parent, Child>`, and
decode the flat submission back into parent + children with
`decode_nested_urlencoded`, so the parent and its children validate and persist
as one transaction.

## Resumable SSE streams (unreleased — trunk-dev, issue #1356)

Don't hand-roll `Last-Event-ID` bookkeeping or a manual replay buffer for
server-sent events. On trunk-dev the `ws`-gated channels backend keeps a
**bounded per-topic replay ring buffer** and assigns every event an
**epoch-tagged per-topic `id` automatically** (wire format `epoch.seq`, opaque
to clients) — no manual `.id(...)` in handler code.

- `autumn_web::sse::stream_resumable(&state, topic, last_event_id)` is the route
  primitive. It reads the client's `Last-Event-ID`, replays the buffered events
  the client missed in order, then continues live — no duplicated or skipped
  events at the seam (publish assigns seqs and broadcasts under one lock; resume
  subscribes under that same lock and snapshots the buffer). Within one epoch it
  replays entries with `seq > last_event_id.seq`; across an epoch boundary it
  gaps and replays the full retained current epoch (see Limitations).
- Read the inbound header with `autumn_web::sse::last_event_id(&headers)` →
  `Option<EventId>` (`EventId { epoch: u64, seq: u64 }`, exported as
  `autumn_web::sse::EventId` and re-exported `autumn_web::channels::EventId`), or
  the `autumn_web::sse::LastEventId(pub Option<EventId>)` extractor (never fails;
  absent/malformed/legacy-bare-integer → `None` → treated as a cold connection).
- **Cold connection** (`None`) behaves exactly like `sse::stream`: no replay,
  just live events, dense seqs preserved.
- **Gap sentinel:** when the requested id has aged out of the retained window
  (the buffer overflowed) **or belongs to a different epoch**, the stream emits
  one `gap` event (`event: gap`, `data: {"gap":true}`, no `id`) *before* the
  replay so clients can detect missed events rather than silently receiving a
  hole. A live broadcast lag surfaces the same `gap` sentinel and advances the
  seq counter.
- The existing non-resumable helpers — `sse::stream`, `sse::stream_authorized`,
  `sse::from_subscriber`, `Channels::sse_stream` — are **unchanged and remain
  id-less**; `stream_resumable` is purely additive.
- Only the in-process local backend retains a replay buffer; the Redis fan-out
  backend degrades gracefully to a live-only `ResumeHandle`
  (`Channels::resume(topic, last_event_id) -> ResumeHandle` is available on any
  backend).

```rust
use autumn_web::prelude::*;
use autumn_web::sse::LastEventId;

#[get("/events")]
async fn events(State(state): State<AppState>, LastEventId(last): LastEventId) -> impl IntoResponse {
    autumn_web::sse::stream_resumable(&state, "feed", last)
}
```

Retention is configurable via `channels.replay_buffer` (`N`, default `256`;
env `AUTUMN_CHANNELS__REPLAY_BUFFER`). Memory is `O(N)` per topic regardless of
throughput. This is distinct from `channels.capacity`, which sizes the live
broadcast fan-out ring.

**Limitations** (in-process best-effort scope, per issue #1356's "Out of
Scope") — the replay buffer is a same-process convenience, not a durable log:

- The replay buffer lives in the topic's in-memory state, which is dropped when
  the topic is garbage-collected (a topic is retained only while it has a live
  receiver or an outstanding `Sender`). A topic with only transient SSE
  subscribers can lose its buffer during the disconnect window. The recreated
  topic gets a **new epoch**, so a reconnect across the gap is not silently
  corrupted: the epoch mismatch is signalled as a `gap` sentinel plus a full
  replay of the current epoch (the old epoch's events themselves are gone).
- On process restart (or any topic GC/recreation) the per-topic `seq` counter
  resets to `1` under a fresh `epoch`. The epoch tag means a stale `Last-Event-ID`
  from a previous epoch — even one whose `seq` the new epoch has already passed —
  is always distinguishable from a current-epoch id, so the reconnect yields a
  `gap` sentinel followed by a full replay of the current epoch rather than
  silently dropping the new epoch's early events (the pre-epoch-tag hole, issue
  #1356). The buffered history from before the restart is still gone (persist it
  yourself for durability), but the client is never fed a corrupted partial.
- Publishing through `channels.publish()` / `broadcast().publish()` /
  `channels.sender().send()` is safe on resumable topics: all three route
  through the backend's `publish`, assigning a seq and appending to the replay
  buffer. Only calling `.send()` directly on the raw `broadcast::Sender`
  (obtained from `channels.sender().keepalive` or the `ensure_topic` trait
  method) broadcasts without assigning an id or appending to the replay buffer,
  which breaks resumability for that topic.

For cross-restart / multi-replica durability, back the stream with a durable log
(e.g. a database table or a Redis stream) instead of the in-process buffer.

## Configuration

Config layering, lowest to highest:

1. framework defaults
2. profile smart defaults (`dev` / `prod`)
3. `autumn.toml`
4. `[profile.<name>]` inside `autumn.toml`
5. `autumn-{profile}.toml`
6. `AUTUMN_*` environment variables

Profile selection precedence:

1. `AUTUMN_ENV`
2. `AUTUMN_PROFILE`
3. `--profile <name>`
4. debug/release auto-detection

Use `AUTUMN_SECTION__FIELD` for env overrides, for example
`AUTUMN_DATABASE__PRIMARY_URL`, `AUTUMN_JOBS__BACKEND`,
`AUTUMN_SECURITY__SIGNING_SECRET`, and
`AUTUMN_SECURITY__WEBHOOKS__REPLAY__BACKEND`.

### Process roles — web/worker split (unreleased — trunk-dev)

Scale HTTP and background work independently by giving a process a **role** (no
app-code change) via `role = "web"|"worker"|"combined"` in config or the
`AUTUMN_ROLE` env var:

| Role | Serves HTTP | Runs workers + scheduler |
|---|---|---|
| `combined` (default) | yes | yes — unchanged behaviour |
| `web` | yes (can still enqueue jobs) | no |
| `worker` | no (probe-only router) | yes |

- Run a specific tier: `autumn serve --role web|worker|combined`.
- A split (non-`combined`) role **requires a `postgres`/`redis` jobs backend** —
  an in-memory queue can't cross processes.
- `release init --split-workers` splices a dedicated `worker:` service into the
  generated **docker-compose** output and sets the web-tier role on the `app`
  service (#1613). See `docs/guide/cloud-native.md`.

### Per-queue worker pools, pinning & `ProcessRole` on `AppState` (unreleased — trunk-dev)

Carve the per-process worker pool up per queue and dedicate a worker tier to a
subset of queues — all config-only, no app-code change:

- **Reserved / capped pools** — make a `[jobs.queues]` value a table:
  `critical = { weight = 4, reserved = 2 }` keeps 2 slots that no other queue may
  ever consume; `bulk = { weight = 1, concurrency = 4 }` caps a queue at 4 of the
  process's `jobs.workers` slots. A bare integer is still just a weight. Total
  capacity stays `jobs.workers`; the rules only redistribute it and are enforced
  on every backend (local/Postgres/Redis).
- **Worker pinning** — `jobs.pin = ["critical"]` (or `AUTUMN_JOBS__PIN=bulk,default`,
  comma-separated) makes a `worker`-role process claim *only* those queues,
  preserving weighted/strict order within the subset; empty/unset drains every
  queue. A worker leaving a configured queue uncovered logs a startup `WARN`
  (an `ERROR` if it would claim nothing); `autumn doctor --strict` reports
  coverage informationally (`jobs_queue_coverage`) without failing (issue #1623).
- **Per-queue actuator gauges** — `<actuator-prefix>/jobs` adds a `queues` key
  with per-queue `depth` and `oldest_waiting_age_ms` alongside the existing
  per-job-type gauges (per-process approximations on multi-process backends).
- **Backend-derived queue gauges (unreleased — trunk-dev, issue #1752)** — on
  the durable backends (Postgres/Redis) those `queues` gauges and the
  per-job-type `queued` counter are no longer per-process approximations: a
  periodic survey of the durable store (Redis every 2s, the interval doubling as
  the gauge cache TTL) wholesale-replaces this process's local enqueue marks each
  tick, so an enqueue-only `web` replica reports the true shared backlog and a
  queue absent from the latest survey resets to `depth` 0. The Redis survey pages
  the whole due-delayed ZSET so scheduled/retry bursts count exactly. The `local`
  backend keeps the in-process mark path. See `docs/guide/jobs.md`.
- **`ProcessRole` on `AppState`** — `state.role()` returns the resolved
  `ProcessRole` (exported at `autumn_web::ProcessRole`) with `serves_http()` /
  `runs_workers()` predicates, so app-owned background loops in `on_startup`
  self-gate to the right tier instead of re-reading `AUTUMN_ROLE` (issue #1726).
  See `docs/guide/jobs.md`.

## Operator alerts (unreleased — trunk-dev, issue #1610)

Connect Autumn's built-in failure signals to email + a signed webhook with **no
app code** — configure a destination under `[alerts]` and every built-in
condition is delivered, deduplicated, with a recovery notice when it clears:

```toml
[alerts]
email = "oncall@example.com"        # AUTUMN_ALERTS__EMAIL
webhook_url = "https://alerts.example.com/hooks/autumn"
webhook_secret = "…"                # REQUIRED with webhook_url; alerts are always
                                    # signed (prefer AUTUMN_ALERTS__WEBHOOK_SECRET)
```

Built-in conditions — each carries a stable `dedup_key`, a `severity`
(`critical` on trigger, `recovery` on resolve), the host/replica, and a "where
to look" actuator pointer:

- **Dead-lettered job** — a job exhausts its retries (deduped per job type).
- **Health indicator down** — an indicator stays non-healthy past
  `health_grace_secs` (default 60).
- **High 5xx rate** — the rolling 5xx rate crosses `error_rate_threshold`
  (default `0.05`, a fraction in `(0, 1]`) over ≥ `error_rate_min_requests`.
- **Scheduled-task failure** — a `#[scheduled]`/framework task returns an error.
  A failed `autumn db backup` offsite upload also raises this condition,
  delivered via the outbound-HTTP alert channels only (not email)
  **(unreleased — trunk-dev, issue #1743)**.

Delivery is best-effort and off the request path (background tick every
`eval_interval_secs`, default 30), reuses your existing mailer + outbound-webhook
machinery, and the webhook is signed exactly like other Autumn webhooks
(`Autumn-Signature: t=…,v1=<hmac-sha256>`). `enabled = false` is the master off
switch (also silences custom channels). Add your own transport (PagerDuty,
Slack, …) by implementing `AlertChannel` and registering it with
`AppBuilder::with_alert_channel`. `autumn doctor` warns (in production) on a
missing or unusable destination — see the `doctor` skill. See
`docs/guide/operator-alerts.md`.

### Native transports (unreleased — trunk-dev, issue #1630)

PagerDuty, Slack, and Discord now ship as built-in `AlertChannel`s — no code,
just `[alerts]` keys (each with an `AUTUMN_ALERTS__*` env override):

```toml
[alerts]
pagerduty_routing_key = "…"   # Events API v2 integration key; enables PagerDuty
pagerduty_url = "https://events.pagerduty.com/v2/enqueue"  # optional override
pagerduty_severities = "all"  # "all" (default) | "critical"
slack_webhook_url = "https://hooks.slack.com/services/…"
slack_severities = "all"
discord_webhook_url = "https://discord.com/api/webhooks/…/slack"  # append /slack
discord_severities = "all"
```

- **PagerDuty** delivers each alert as an Events API v2 event correlated on the
  alert's stable `dedup_key`, so a repeating condition folds into one incident
  and a `recovery` auto-resolves it — keep `pagerduty_severities = "all"` so the
  `resolve` event reaches PagerDuty. `pagerduty_url` targets any
  PagerDuty-Events-compatible endpoint.
- **Slack / Discord** post a human-readable message; Discord reuses Slack's
  payload dialect via its `/slack`-suffixed endpoint. Both require an absolute
  `https` webhook URL (enforced at runtime and by `autumn doctor`).
- **Per-channel severity routing** — `*_severities = "critical"` sends only
  firing alerts (recoveries suppressed); `"all"` (default) sends both. An alert
  below a channel's threshold is never delivered to it (`AlertChannel::accepts_severity`).
- Outbound alert sends stay off the request path (dispatched best-effort on a
  background runtime task). Slack/Discord webhook URLs must be absolute `https`,
  but the sends only validate URL shape and do not run through the SSRF
  deny-list, so restrict alert destinations to trusted operator-configured URLs.
  A native transport counts as a destination, so configuring one
  satisfies `autumn doctor --strict` without `[alerts] email`/`webhook_url`.
- `autumn alert test [--channel <name>]` fires a synthetic alert through each
  configured outbound-HTTP channel (email is excluded) and reports per-channel
  success/error.

(issue #1630). See `docs/guide/operator-alerts.md`.

## Observability defaults

Published 0.5.0 behavior:

- Structured per-request access log is **on by default**; disable with
  `log.access_log = false`, tune exclusions with `log.access_log_exclude`
  (probes/static excluded out of the box).
- Request-scoped log context auto-tags every log line in a request; add
  custom fields with `autumn_web::log::context::with_log_field("order_id", id)`
  (prelude re-export).
- `actuator.prometheus` exposes the Prometheus scrape endpoint independently
  of sensitive actuator mode.

### Runtime log levels (unreleased — trunk-dev)

`PUT /actuator/loggers/{name}` now changes the **live** `tracing` subscriber,
not just an in-memory map — raise/lower verbosity in production without a
redeploy. The default telemetry init installs a `tracing_subscriber` reload
layer and hands the handle to `LogLevels`, so a level change rebuilds the
combined `EnvFilter` directive (global level + per-target overrides) and pushes
it to the running subscriber on the next event. Examples (sensitive actuator
mode required):

```bash
curl -X PUT .../actuator/loggers/root -d '{"level":"debug"}'          # global
curl -X PUT .../actuator/loggers/my_app::orders -d '{"level":"trace"}' # per-target
curl -X PUT .../actuator/loggers/root -d '{"level":"info"}'            # revert
```

The response now carries `"applied": true` and `"status":"ok"` only when the
change actually reached a reload-capable subscriber; otherwise it reports
`"status":"recorded"` / `"applied": false` rather than a false-positive `ok`.
Overrides stay ephemeral — a restart resets to the configured `log.level`.
Invalid levels still return `400`. `GET /actuator/loggers` keeps reporting
`current_level` + overrides, now matching real emission.

### Build & git provenance on `/actuator/info` (unreleased — trunk-dev)

`GET /actuator/info` now reports which commit/build is running, for
deploy/rollback verification. Apps scaffolded by `autumn new` get this with
**zero action**: the generated `build.rs` bakes `AUTUMN_BUILD_*` values and
`#[autumn_web::main]` reads them (plus the app's own `CARGO_PKG_NAME` /
`CARGO_PKG_VERSION`) at the app's compile time. That also fixes the old
`app.version = "unknown"` regression — the value is now baked in, correct even
in a `--release` binary with the cargo env unset at runtime. Sample payload:

```json
{
  "app":     { "name": "my_app", "version": "1.4.2" },
  "autumn":  { "version": "0.6.0", "profile": "prod" },
  "runtime": { "uptime": "3h 12m" },
  "build": {
    "version": "1.4.2",
    "timestamp": "2026-07-09T12:34:56Z",
    "git": {
      "commit": "9f3c1a7e…",
      "commit_short": "9f3c1a7",
      "branch": "main",
      "dirty": false
    }
  }
}
```

Outside a git checkout (tarball / CI cache) the `git.*` fields degrade to
`null` while `build.timestamp` + `version` stay present. The block exposes only
commit / branch / time / version / dirty — never remote URLs or an env dump.
Hand-rolled apps opt in by adding the generated `build.rs` provenance stanza
and using `#[autumn_web::main]`.

**Known limitation:** apps built from the scaffolded Dockerfile currently report
null git provenance because the Docker build context excludes `.git` (tracked in
#1676).
Unreleased (trunk-dev) — `Server-Timing` response header:

- Opt-in via `[observability] server_timing = true` (or
  `AUTUMN_OBSERVABILITY__SERVER_TIMING=true`). Defaults **on in `dev` /
  `development`** profiles, **off everywhere else** — so prod never leaks
  timings to anonymous clients without explicit opt-in.
- Emits `total;dur=…` (whole-request wall time, same clock as access-log
  `duration_ms`) and, when at least one instrumented query ran,
  `db;dur=…;desc="N queries"` for N+1 visibility.
- SSE responses (`text/event-stream`) get `total`-only; header is
  best-effort — never turns absent timing data into an error.
- The `db` metric installs autumn's own Diesel connection instrumentation on
  measured checkouts only (nothing is installed when `server_timing` is off, so
  an app's `diesel::connection::set_default_instrumentation` is untouched).
  While enabled, autumn's timer replaces an app-provided default rather than
  composing with it — documented limitation; keep `server_timing` off where you
  rely on your own instrumentation.
- Doc + browser DevTools walk-through:
  `docs/guide/observability/server-timing.md`.

## Resilience: load shedding (unreleased — trunk-dev)

Admission control caps concurrent in-flight requests; excess is shed
immediately with `503` + `Retry-After` before the handler runs. Disabled by
default:

```toml
[server]
max_concurrent_requests = 256   # unset/0 = unlimited
```

Probes (`/health`, `/live`, `/ready`, `/startup`, actuator) are never shed;
sheds increment `autumn_requests_shed_total`. See `docs/guide/resilience.md`
and `docs/adr/0009-adopt-overload-protection-load-shedding.md`.

## Sharding (unreleased — trunk-dev, not in published 0.5.0)

Framework-native horizontal sharding: declare `[[database.shards]]` (each a
full primary/replica topology), route by key → logical slot → shard. Prelude
extractors: `ShardedDb` (auto-routed handle — resolves the routing key from
the request, checks out the owning shard's primary, derefs to the connection
like `Db`) and `Shards` (explicit routing: `db_for`/`read_for`/`db_on`, plus
bounded `each_shard` fan-out); install custom routing with
`AppBuilder::with_shard_router`; build repositories per shard with
`from_shard(&ShardedDb)`. `autumn migrate` gains `--shard <name>` /
`--control-only`; a boot-time shard-map guard fails fast on config drift.
There are no cross-shard queries or transactions by design. See
`docs/guide/sharding.md` and `examples/bookmarks-sharded`.

## Per-tenant memory cells (unreleased — trunk-dev, issue #1766)

Row-level tenancy scopes a tenant's *rows*; per-tenant memory cells bound a
tenant's *in-process memory*. Each resolved tenant gets a `TenantCell` — a
byte-accounting boundary with a soft quota and an owned scratch buffer — minted
lazily by the process-wide `TenantCellRegistry` on the first call to
`current_tenant_cell()` (returns `Option<Arc<TenantCell>>`; `None` when tenancy
is disabled or no tenant is bound, so a route outside a tenant context degrades
gracefully). Reach for it in a handler, charge the memory you want bounded, and
let the RAII guard release it:

```rust
use autumn_web::prelude::*;
use autumn_web::tenant_cell::current_tenant_cell;

#[post("/reports")]
async fn build_report() -> AutumnResult<String> {
    // Over-quota tenants get a 503 here via `?`; the charge releases on drop.
    let _charge = match current_tenant_cell() {
        Some(cell) => Some(cell.try_charge(512 * 1024)?),
        None => None, // tenancy disabled / no tenant bound
    };
    Ok(expensive_render().await?)
}
```

- `try_charge(n)` reserves *before* you allocate and hands back a `Charge`
  guard; the per-tenant scratch store
  (`scratch_insert`/`scratch_get`/`scratch_remove`) is charged by allocation
  **capacity** (not length) plus a fixed per-entry overhead
  (`TenantCell::scratch_entry_overhead()`).
- A charge over quota fails **only that tenant's** request — `QuotaExceeded`
  converts to `AutumnError` as HTTP **503**; every other tenant's counter is
  independent, so one whale degrades only its own traffic.
- Configure under `[tenancy]`: `quota_bytes` (soft quota; `0`, the default,
  disables it), `max_cells` (LRU cap on resident cells) and `idle_ttl_secs`
  (idle eviction) — both `0` by default, both enforced lazily on cell insert.
  The quota is stored atomically and refreshed on every access (retune-ready;
  no config-reload path exists yet).
- The whole `[tenancy]` section is settable from the environment via
  `AUTUMN_TENANCY__*` (e.g. `AUTUMN_TENANCY__QUOTA_BYTES`,
  `AUTUMN_TENANCY__MAX_CELLS`, `AUTUMN_TENANCY__IDLE_TTL_SECS`).
- Eviction — manual `TenantCellRegistry::evict(tenant_id)` or automatic
  (`max_cells`/`idle_ttl_secs`) — reclaims tracked bytes on `Drop`; an in-flight
  request keeps its own cached `Arc<TenantCell>` to completion, so evicting
  mid-request never resets a running request's state. This is a tracked-bytes
  accounting boundary via `tracked_bytes()` / `total_tracked_bytes()`, **not**
  RSS — allocations made outside the cell API are invisible by design.

`TenantCell`/`TenantCellRegistry`/`current_tenant_cell` live in
`autumn_web::tenant_cell` (not in the prelude). Orthogonal to sharding: sharding
picks the *database*, cells bound *process memory*. See
docs/guide/tenant-cells.md (issue #1766).

## Error handling

JSON errors are standardized as RFC 7807-style Problem Details.
Handlers should return `AutumnResult<T>` and use the typed constructors:

```rust
Err(AutumnError::not_found_msg("post not found"))?;
Err(AutumnError::bad_request_msg("invalid input"))?;
Err(AutumnError::unprocessable_msg("validation failed"))?;
Err(AutumnError::unauthorized_msg("login required"))?;
Err(AutumnError::forbidden_msg("not allowed"))?;
Err(AutumnError::conflict_msg("duplicate delivery"))?;
Err(AutumnError::service_unavailable_msg("queue unavailable"))?;
```

Clients that prefer JSON receive `application/problem+json` with `type`,
`title`, `status`, `detail`, `instance`, `code`, `request_id`, and `errors`.

## Canary deploys

Autumn provides framework primitives a canary controller drives — it does not
own the load-balancer traffic split (that stays a platform concern).

**Label the canary replica** (env var, no code change):
```bash
AUTUMN_DEPLOY_VERSION=canary   # explicit label (any string)
# …or the boolean shorthand:
AUTUMN_CANARY=true             # resolves to version="canary"
```

Stable replicas leave both unset → `version="stable"`.

**Prometheus metrics** are tagged with the `version` label so a controller can
compare cohorts:
```
autumn_http_requests_total{version="canary"} 412
autumn_http_responses_total{version="canary",status="5xx"} 3
autumn_http_request_duration_seconds{version="canary",quantile="0.99"} 1.2
```

**`CanaryRoute` extractor** (in `prelude::*`) — lets a handler see whether the
LB routed this specific request to the canary (`X-Canary: true`):
```rust
async fn handler(canary: CanaryRoute) -> String {
    if canary.routed_to_canary { "canary".into() } else { "stable".into() }
}
```

**Rollback** — when a controller decides the canary is unhealthy:
```bash
autumn canary rollback --reason "p99 latency exceeded" --by ci-controller
# The replica flips /ready → 503 and drains cleanly (same as SIGTERM).
autumn canary status    # check flag state
autumn canary promote   # clear the rollback flag after traffic is moved
```

The rollback flag file lives at `tmp/autumn-canary-rollback.json`. A controller
that cannot exec into the replica can write it directly. The flag is sticky
across restarts — clear it with `autumn canary promote` once traffic has moved.

## CLI

```bash
cargo install autumn-cli --version 0.5.0

autumn new my-app
autumn setup
autumn dev
autumn build
autumn migrate check
autumn migrate --with-maintenance
autumn task --list
autumn task <name> -- --arg value
autumn generate model Post title:String body:Text
autumn generate migration add_posts
autumn generate scaffold Post title:String body:Text --api
autumn generate auth User --oauth github,google --totp --passkeys
autumn generate admin Post title:String body:Text published:bool
autumn generate mailer User
autumn generate system-test todo_flow
autumn generate pwa
autumn routes --format json --user-only
autumn doctor --strict --json
autumn config list
autumn flags list
autumn experiments list
autumn maintenance on --message "Migrating database"
autumn canary rollback --reason "p99 latency exceeded"
autumn canary status
autumn canary promote
autumn webhook sim generic http://localhost:3000/webhooks/test --secret mysecret --payload '{"ok":true}'
autumn dev-loop-bench --dry-run
autumn plugin-check --plugin-name autumn-admin-plugin --prefix /admin
```

**(unreleased)** trunk-dev-only CLI additions — NOT in the published 0.5.0
`autumn-cli`; do not suggest them to users on the published release:

```bash
autumn serve --daemon            # non-watch local daemon; also: serve stop|status|restart
autumn serve --bundled-pg        # managed local Postgres (managed-pg-bundled feature)
autumn destroy scaffold Post title:String   # cleanly reverses generate; --dry-run supported
autumn generate scaffold Post title:String 'status:enum{draft,published}' 'price:decimal{10,2}' author:references email:String:unique
autumn generate scaffold Post title:String --live --live-validation
autumn generate tauri            # desktop sidecar project (cargo tauri build)
autumn generate plugin my-plugin # installable/conformant plugin crate
autumn token issue service:ci --name ci --scope posts:write   # scoped tokens; also list, rotate
autumn i18n check                # compare t!/t(...) keys vs i18n/*.ftl; --strict, --format json
autumn test                      # provision/target an isolated *_test DB, migrate, then cargo test
autumn test --reset -- --nocapture   # drop+recreate the test DB; forward args to the harness
autumn db backup --keep 7        # dump control DB + shards to ./backups/<profile>/<ts>/; db restore <artifact> reverses it
autumn db backup --upload --keep 7   # + upload each verified run offsite (S3/MinIO/R2); db offsite list; db restore offsite:<profile>/latest  # (unreleased — trunk-dev)
autumn seed --count 50 --model Post  # generate+insert 50 faked rows via the model's factory (both flags together)
autumn serve --role worker       # run only workers + scheduler (web/worker split); also --role web|combined
autumn console                   # data playground: scaffolds src/bin/playground.rs (pre-wired config+pool), then builds and runs it; alias `autumn c`
autumn console --force           # regenerate the playground from the template (never overwritten otherwise)
autumn console --scaffold-only   # scaffold + wire Cargo.toml, then stop
```

`autumn console` is Autumn's `rails console` equivalent. Rust has no stable
`eval`, so it follows loco.rs's edit-and-run model instead of shipping a REPL:
the playground is an ordinary Rust file you edit, and the command owns the
compile-and-run loop. It resolves the database exactly like `autumn seed` and
`autumn dev` (`AUTUMN_DATABASE__PRIMARY_URL` → `AUTUMN_DATABASE__URL` →
`DATABASE_URL` → profile-aware `autumn.toml`) by bootstrapping through
`autumn_web::seed::SeedContext`.

Two things to know when advising on it:

- The playground declares the app's `schema` / `models` / `repositories` /
  `policies` modules with `#[path = "../…"]`, because a Cargo bin target is its
  own crate and a generated Autumn app has no `src/lib.rs`. Users add their own
  `use` lines (and can add more `#[path]` lines for other modules).
- Its `[[bin]]` is gated behind `required-features = ["playground"]`, so
  `cargo build`, `cargo test`, `autumn dev`, and `autumn build` skip it
  entirely. Only `autumn console` compiles it. Never suggest removing that
  gate: without it, a playground that fails to compile would break the app's
  default build.

`autumn i18n check` scans `**/*.rs` for string-literal keys passed to
`t!(...)`, `.t(...)`, and `.t_with(...)`, loads every `i18n/<locale>.ftl` via
the runtime `Bundle` loader, and reports per locale: **Missing** (referenced in
code but absent from that locale's resolved fallback chain — the correctness
failure, non-zero exit), **Untranslated** (present in the default locale but
not a non-default one), and **Unused** (defined in a `.ftl` with no call site).
Untranslated/Unused are warnings unless `--strict`. `--format json` feeds CI.
Runtime-built keys like `t(&format!(...))` are listed as "dynamic — not
checked" rather than flagged.

**Known heuristic limits.** The scanner is a best-effort token/grammar
heuristic over source text, not a type-resolved AST. Any key it cannot
statically resolve to a string literal is treated as *dynamic — not checked*,
so unsupported shapes are never falsely flagged as Missing/Unused and cannot
break CI:

- Only whole-key string literals (optionally in leading borrows/parens spanning
  the entire key argument) are checked precisely. A literal transformed by a
  chained method or operator (`t(" nav.home ".trim())`, `t("a" + b)`) is treated
  as dynamic, not as the literal.
- Dynamic keys are reported as "dynamic — not checked". A whole-key
  `format!("status.{state}")` contributes its static `"status."` prefix so
  matching `.ftl` keys aren't marked Unused; a fully-dynamic key (bare variable,
  `format!("{x}")`) suppresses Unused reporting entirely.
- Keys built through concatenation, helper functions, non-`format!` macros, or
  deeper transformations are treated as dynamic and not validated — such `.ftl`
  entries aren't checked for Missing/Unused.
- No type resolution: it can't distinguish an unrelated local helper named
  `t`/`t_with` from the real translation API beyond its syntactic
  `t!(...)` / `receiver.t(...)` / `Type::t(...)` (real `::` path) checks; a bare
  free-function `t("...")` is left alone.
autumn migrate baseline          # record content hashes for legacy applied migrations (issue #1203)
autumn migrate baseline --force <version>  # escape hatch: overwrite one version's stored hash (WARN-logged)
```

### Migration content checksums (issue #1203)

The framework hashes every migration's `up.sql` (SHA-256, with `\r\n`/`\r`
→ `\n` normalisation and `trim_end()`) into `autumn_migration_checksums`
the first time it's applied, and re-validates the hash before every
subsequent `autumn migrate` run and startup auto-migrate. Editing an
already-applied migration flips its state to `changed` and the next
`autumn migrate` refuses to run:

```
migration <version> checksum mismatch: recorded <hex-a> but on-disk
content hashes to <hex-b>. Migrations must never be edited after being
applied — add a new migration instead, or run the documented re-baseline
command if this change was deliberate.
```

Rule of thumb: **never edit an applied migration.** Add a new one. If the
user asks you to edit an applied migration, push back with this rule and
propose a follow-up migration. `autumn migrate status` reports each
applied migration's state as `ok`, `changed`, or `unrecorded`. Use
`autumn migrate baseline` (additive, idempotent) to record hashes for
legacy migrations applied before the checksum feature existed; use
`autumn migrate baseline --force <version>` only when a deliberate edit
is intended and the fork risk is accepted.

### `autumn test` — isolated test DB (unreleased — trunk-dev, issue #1056)

`autumn test` resolves the test DB URL with the same precedence as
`autumn migrate` (`AUTUMN_DATABASE__PRIMARY_URL` → `AUTUMN_DATABASE__URL` →
`DATABASE_URL` → autumn.toml — env wins over TOML), derives a
`_test`-suffixed name, creates it if missing, runs all pending app + framework
migrations, exports `AUTUMN_ENV=test` + the resolved `DATABASE_URL`, then runs
`cargo test` (exiting with its code). `--reset` drops and recreates the test DB
first; trailing `-- <args>` forward to the harness. It refuses to run against a
non-`_test` database.

### `autumn db backup` / `db restore` (unreleased — trunk-dev, issue #1595)

`autumn db backup [--dir DIR] [--format custom|plain] [--keep N] [--shard NAME]
[--control-only]` dumps the control DB and every shard to
`<dir>/<profile>/<timestamp>/` (default `./backups`) with a `manifest.json`,
integrity-checks each artifact with `pg_restore --list` before reporting success
(a partial artifact is removed and the command exits non-zero), and `--keep N`
prunes to the newest N runs. `autumn db restore <ARTIFACT> [--shard NAME]
[--force]` verifies every artifact before touching a database and is gated by the
same production guard as `db drop` (refuses non-dev/test without `--force`). See
`docs/guide/deployment.md`.

### Offsite S3 backups (unreleased — trunk-dev, issue #1619)

`autumn db backup --upload` (or `[backup.offsite] auto_upload = true`) uploads
each completed local run to an S3-compatible offsite destination (AWS S3,
`MinIO`, Cloudflare R2, Backblaze B2, Garage) **after** local verification
passes, then HEAD/GET-verifies every remote object matches before reporting
success; a local-good / upload-failed run exits non-zero with a split-outcome
message and leaves the local artifact intact. Configure it in `autumn.toml`:

```toml
[backup.offsite]
auto_upload = true          # upload after every `autumn db backup`, no --upload
prefix = "db"               # objects keyed {prefix}/{profile}/{timestamp}-{token}/{file}
keep = 30                   # independent remote retention (prune after verify)
# allow_shared_bucket = true  # opt-in to reuse the app's [storage.s3] bucket

[backup.offsite.s3]
bucket   = "myapp-db-offsite"
region   = "auto"
endpoint = "https://<accountid>.r2.cloudflarestorage.com"
force_path_style = true
access_key_id_env     = "AUTUMN_OFFSITE_ACCESS_KEY_ID"    # names, not values
secret_access_key_env = "AUTUMN_OFFSITE_SECRET_ACCESS_KEY"
```

Every key has an `AUTUMN_BACKUP__OFFSITE__*` override (e.g.
`AUTUMN_BACKUP__OFFSITE__S3__BUCKET`) and honors profile overlays
(`[profile.prod.backup.offsite]`). Credentials are **env-var indirection** only
— config names the env vars the secrets are read from; the values never live in
config, argv, logs, or errors. The remote run id appends a short unique token to
the timestamp (`{timestamp}-{token}`) so same-second backups of one profile from
different hosts never collide in the bucket. `autumn db offsite list [--profile
P]` shows the offsite runs for the active profile (printing the full
`{timestamp}-{token}` run id); `autumn db restore
offsite:<profile>/<run-id|latest>` (or `--offsite`) downloads a run to a temp dir
and applies the same integrity verification and production `--force` guard as a
local restore. The selector accepts the full `{timestamp}-{token}` run id, a bare
`{timestamp}` (only when it uniquely matches one run — otherwise it errors and
lists the candidates), or `latest` (newest complete run). The transfer client is a dependency-light synchronous `SigV4`
client streamed end-to-end (a single `PutObject` sends a server-side
`x-amz-checksum-sha256`; above 64 MiB — S3 caps a single `PutObject` at 5 GiB —
the artifact uploads via multipart, hashed locally and verified after
`CompleteMultipartUpload` via HEAD/GET). `autumn
doctor` adds an `offsite_backup` check that fails on an invalid configured
destination and warns on unready credentials (see the `doctor` skill),
and a failed upload raises a `ScheduledTaskFailure` operator alert only when an
outbound-HTTP `[alerts]` channel (PagerDuty / Slack / Discord / signed webhook)
is configured — an email-only `[alerts]` config is not notified (issue #1743).
Pointing the offsite bucket at the app's
own `[storage.s3]` bucket at the same endpoint needs `allow_shared_bucket = true`. See
`docs/guide/daemon.md`.

`autumn destroy` mirrors `autumn generate` argument-for-argument and never
touches a database — it only reverses generated files/migrations.

`autumn doctor --strict` is the deployment sanity check. It reports unsafe
production defaults, missing primaries, stale replica migrations, missing
signing secrets, and other config problems without printing credentials.

## 0.5.0 release traps

- `AUTUMN_SECURITY__SIGNING_SECRET` is required in `prod` / `production`.
- Use `autumn-storage-s3 = "0.5"` and `autumn-cache-redis = "0.5"`; these
  are companion crates, not `autumn-web` feature names.
- Repository-generated APIs require a policy in production unless
  `security.allow_unauthorized_repository_api = true` is explicit.
- `Mailer::deliver_later` requires a durable queue in production unless
  `mail.allow_in_process_deliver_later_in_production = true` is explicit.
- Signed webhook replay protection should use Redis in multi-replica prod.
- OAuth2/OIDC social-login scaffolding is present. If release notes disagree,
  verify `autumn-cli/src/generate/auth.rs`, `docs/guide/oauth.md`, and current
  branch history before summarizing the release.

## Design invariants

- Postgres only for database-backed apps.
- Diesel + diesel-async only; do not replace core data access with SQLx.
- Stable Rust only.
- Server-rendered HTML first; htmx is the interactivity layer.
- Single binary; external infrastructure is opt-in through config/plugins.
- No GraphQL, DI framework, or deployment tooling in core.
- Primary keys are `i64`; UUIDs are secondary columns only.

## Release and PR workflow

- Base branch is `trunk-dev`, not `trunk`.
- Release tag for this line is `v0.5.0`.
- Published crates are released together at the same workspace version:
  `autumn-macros`, `autumn-web`, `autumn-cli`, `autumn-admin-plugin`,
  `autumn-storage-s3`, `autumn-cache-redis`, and `autumn-search`.
- The publish gate checks crate metadata, package dry-runs, full docs,
  semver compatibility, release-note alignment, and downstream smoke tests.
- Use `docs/release-checklist.md`, `docs/guide/docs-smoke.md`,
  `CHANGELOG.md`, `RELEASE_NOTES.md`, and `STABILITY.md` for release work.

## Local verification gates

Before pushing an Autumn PR:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo test -p autumn-web --doc --all-features
cargo test -p autumn-cli --test cli_tests
```

There is no separate `repo_hygiene` test target anymore — it is consolidated
into `cli_tests` (`autumn-cli/tests/integration/repo_hygiene.rs`). Integration
tests live in consolidated binaries (`autumn` → `integration_tests`,
`autumn-cli` → `cli_tests`); new tests go in `tests/integration/<name>.rs` +
a `mod` line in `tests/integration/mod.rs`, not new `[[test]]` targets.

CI also runs a feature-combination compile gate (35 `autumn-web` feature
combos via `cargo hack`), a generator-conformance gate, and a plugin
freshness gate (`scripts/check-plugin-freshness.sh` — user-facing changelog
entries must ship matching Claude-plugin updates).

For docs or generated-app changes, also run the docs smoke procedure in
`docs/guide/docs-smoke.md`. For public API changes, run doctests for the
touched crate so examples compile from an external-consumer perspective.

## Gotchas

- `examples/*/static/css/autumn.css` are generated Tailwind artifacts; ignore
  dirty changes after running examples.
- Proc macros must emit paths through `::autumn_web::...` or
  `::autumn_web::reexports::*`. Do not delegate to upstream macros that emit
  hard-coded transitive dependency paths.
- Workspace builds can hide transitive dependency mistakes. External examples,
  doctests, and downstream smoke tests catch what local `cargo check` misses.
- `CHANGELOG.md` drift between `trunk` and `trunk-dev` can be expected around
  releases; do not propose churn-only back-sync PRs.

## Primary docs

- `README.md`
- `CHANGELOG.md`
- `RELEASE_NOTES.md`
- `STABILITY.md`
- `docs/migrations/0.4.0.md`
- `docs/release-checklist.md`
- `docs/guide/getting-started.md`
- `docs/guide/docs-smoke.md`
- `docs/guide/cloud-native.md`
- `docs/guide/oauth.md`
- `docs/guide/mcp.md`
- `docs/guide/feature-flags.md`
- `docs/guide/experiments.md`
- `docs/guide/runtime-config.md`
- `docs/guide/signed-webhooks.md`
- `docs/guide/storage.md`
- `docs/guide/jobs.md`
- `docs/guide/state-machines.md`
- `docs/guide/repositories.md`
- `docs/guide/pagination.md`
- `docs/guide/transactions.md`
- `docs/guide/hooks-and-transactions.md`
- `docs/guide/events.md`
- `docs/guide/cache-stampede.md`
- `docs/guide/sharding.md`
- `docs/guide/daemon.md`
- `docs/guide/resilience.md`
- `docs/guide/generators.md`
- `docs/guide/widget-styling.md`
- `docs/guide/tabs.md`
- `docs/guide/maintenance-mode.md`
- `docs/guide/dev-loop-latency.md`
- `docs/guide/system-tests.md`
- `docs/guide/testing.md`
- `docs/autumn-workflow-architecture.md`
