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

**(unreleased)** Events & listeners — a typed domain event bus so one action
can fan out without inline coupling: `#[event]` on a struct, publish via the
`Events` extractor (`events.publish(UserSignedUp { .. }).await?`), react with
`#[listener(UserSignedUp)]` (sync, in-request) or
`#[listener(UserSignedUp, durable, max_attempts = 5)]` (jobs-backed), and
register with `.listeners(listeners![...])`. Do not also list durable
listeners in `jobs![...]`. See `docs/guide/events.md`.

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

## View helpers and widgets (unreleased — trunk-dev, not in published 0.5.0)

Trunk-dev ships framework view widgets — prefer these over hand-rolled Maud
for the common cases:

| Helper | Purpose |
|---|---|
| `link_to(label, href)` / `link_to_with(..., &LinkToOptions)` | Escaped GET anchors; auto `rel="noopener"` on `target="_blank"` |
| `button_to(label, href, Method, csrf_token)` / `button_to_with` | Single-button form for state-changing actions; CSRF is a required arg; non-GET emits hidden `_method` override |
| `card(&body, &CardConfig::new().title("..."))` / `stat_card(label, value, link)` | Titled panels and metric tiles (`autumn_web::widgets`, prelude re-export) |
| `tabs(id, &[(id, label, markup)])` | No-JS CSS-only tab switcher (`docs/guide/tabs.md`) |
| `modal(id, title, &body, &ModalConfig)` / `confirm_action(...)` | Native `<dialog>` confirm for destructive actions — replaces `hx-confirm`/`window.confirm()` |
| `pagination_nav(&page, &PagerOptions::new("/posts"))` / `cursor_pagination_nav` | Accessible, filter-preserving, htmx-opt-in pager from a `Page`/`CursorPage` (prelude re-export) |
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
```

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
  `autumn-storage-s3`, and `autumn-cache-redis`.
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
