---
name: autumn-web
description: >
  Use when building, debugging, documenting, or upgrading Rust web applications
  with autumn-web, autumn-cli, or first-party Autumn crates; also use for
  Autumn route/model/repository/job/webhook/admin macros, AppBuilder setup,
  Maud + htmx server-rendered UI, Diesel async Postgres, and Autumn 0.7.x
  migration or release work.
---

# autumn-web - Rust Web Framework

**Repository**: https://github.com/autumn-foundation/autumn
**Branch**: `trunk-dev`
**Latest release**: 0.7.0 | **Edition**: 2024 | **MSRV**: 1.88.0

**Version identity trip wire**: the workspace on `trunk-dev` is versioned
0.7.0, and `v0.7.0` is the current release line — `autumn-web = "0.7"`,
`cargo install autumn-cli --version 0.7.0`. Features below carry the release
they arrived in — **(0.7.0)** is **absent from 0.6.x and earlier**, and
**(0.6.0)** is absent from 0.5.x: if an app pins an older line, check
[`CHANGELOG.md`](../../CHANGELOG.md) before using them. Unmarked features
predate 0.6.0. An app moving up from 0.6.x follows
[`docs/migrations/0.7.0.md`](../../docs/migrations/0.7.0.md); run
`autumn upgrade` first.

autumn-web is a Spring Boot-style web framework for Rust, built on Axum. It
assembles Axum, Diesel, Maud, htmx, Tailwind, Tokio, tracing, and production
defaults into a convention-over-configuration stack with proc-macro ergonomics.

## Read these references

This file is the quick operating guide. Load the adjacent reference files only
when their details matter:

- `references/api-reference.md` - release-line API map, proc macros,
  feature flags, AppBuilder methods, config env names, and dependency versions.
- `references/examples.md` - official 0.7.0 example patterns for minimal apps,
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
  a guarantee — the typed primitives are the compile-time proof (0.6.0,
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
| Manual per-item queries in a loop (N+1) or hand-written JOINs to fetch associations | `#[belongs_to]`/`#[has_many]`/`#[has_one]` + `repo.preload(records, Model::preload()...)` (0.6.0) |
| Hand-rolled auth, session, or token checks in handler bodies | `#[secured]` / `#[secured("role")]`, `#[authorize]`, repository `policy =`/`scope =`; `#[secured(scopes = [...])]` for service tokens (0.6.0) |
| A punishingly low global rate limit to protect one abuse-prone endpoint (login, search, export) | `#[throttle(limit = 5, per = "1m", key = "ip")]` sits alongside `#[get]`/`#[post]`; `#[throttle("login")]` reads `[security.rate_limit.named.login]` from config (0.6.0, issue #1350, see `docs/guide/rate-limiting.md`) |
| Hand-assembled `<form>` markup with manual value re-fill and error display | `autumn_web::form::form_for(&changeset, action, method)` + the `#[model]`-derived `FormModel` (0.6.0) renders the whole form — CSRF, `_method` override, one pre-filled control per column, inline errors, submit — in one call; see "Whole-form rendering" below. Compose the per-field helpers only when its escape hatches don't fit: `form_tag`, `method_input`, `text_input` (published); `number_input`, `datetime_input`, `date_input`, `checkbox_input`, `select_input` + `Changeset` 422 re-render (0.6.0) |
| `find_all()` + a loop (or raw Diesel `LIMIT`/`OFFSET` paging) to sweep a whole table in a task/job/backfill | `repo.find_in_batches(n)` / `repo.find_each(n)` (0.6.0) — bounded-memory primary-key keyset iteration. See "Generated repository surface" below and `docs/guide/pagination.md` |
| Ad-hoc `tokio::spawn` / background threads for deferred work | `#[job]` (+ retries, backends, uniqueness/concurrency caps), `#[scheduled]` for recurring, `#[task]` for operator CLI work |
| A hand-written `#[scheduled]` fn + batched `DELETE`/`UPDATE` to expire old sessions, drafts, or one-time codes | `#[repository(Model, retention(after = "30d", basis = created_at))]` (0.7.0, issue #1342) — batched, soft-delete-aware, fleet-coordinated sweep with zero SQL; `autumn retention --dry-run` to validate first. See `docs/guide/retention-sweeps.md` |
| A cron job (or nothing at all) trimming `autumn_jobs`, `autumn_job_tracking`, `autumn_experiment_assignments`, or a JSONL audit archive | `[retention]` in `autumn.toml` (0.7.0, issue #1605) — one window per framework-owned dataset, enforced by a fleet-coordinated in-process sweep; `autumn db retention --dry-run` reports the effective policy and eligible rows. See `docs/guide/data-retention.md` |
| Hand-written memoization or cache-aside code | `#[cached]` on functions; `cache::get_or_compute` / `get_or_compute_with` for stampede-safe read-through fills (0.6.0) |
| Hand-written transaction retry loops for serialization failures | `Db::tx(...)`; `Db::tx_with(TxOptions::serializable(), ...)` auto-retries 40001 (0.6.0) |
| Hand-rolled HMAC verification for Stripe/GitHub/Slack callbacks | `SignedWebhook` extractor + `[webhooks.<name>]` config |
| Hand-rolled pager markup (page-number windows, prev/next links) | `pagination_nav(&page, &PagerOptions::new("/posts"))` / `cursor_pagination_nav` (0.6.0) |
| Hand-rolled cross-module notifications (calling every reaction inline) | `#[event]` + `#[listener]` typed event bus, `.listeners(listeners![...])` (0.6.0) |
| Hand-rolled cards, tabs, modals, delete-confirm dialogs, method-override links | `autumn_web::widgets`: `card`/`stat_card`, `tabs`, `modal`/`confirm_action`, `link_to`/`button_to` + `ui::WIDGETS_CSS_PATH` stylesheet (0.6.0) |
| Hand-built file-download responses (manual `Content-Disposition`/`Content-Type`/`Content-Length` headers, byte-buffered blob reads) | `autumn_web::download::Download` — `from_bytes` / `from_stream` / `from_async_read` / `from_blob(&store, key).await?` + `.filename(...)` / `.content_type(...)` / `.inline()`; RFC 5987 filenames, injection-safe, streams blobs without buffering (0.6.0) |
| Hand-parsed `Range` headers / manual `206 Partial Content` / `Content-Range` / `416` for seekable media or resumable downloads | `autumn_web::range` (`resolve` + `partial_bytes_response`) and `Download::into_response_ranged(&headers).await` — RFC 7233 single-range parsing, multi-range single-range collapse, `If-Range` via `.etag(..)`/`.last_modified(..)`, blob slices via `BlobStore::get_range` (no whole-object buffering) (0.6.0) |
| Shelling out to `wkhtmltopdf`/headless Chrome, or hand-rolling a PDF library, to turn a view into a downloadable invoice/receipt/report | `autumn_web::pdf::Pdf` (`pdf` Cargo feature) — `Pdf::from_markup(markup)` / `Pdf::from_html(html)` + `.filename(...)` / `.inline()`; renders headings/paragraphs/tables/lists/bold/italic with the PDF base-14 fonts, no system browser or embedded fonts required. Test with `TestResponse::assert_pdf_contains(&self, &str)`. See `docs/guide/pdf-downloads.md` (0.7.0) |
| Hand-written RSS/Atom XML strings for a `/feed.xml` or podcast/blog feed | `feed::Feed::atom(..)` / `feed::Feed::rss(..)` + `feed::FeedEntry` — builds the XML, implements `IntoResponse` with the right `application/atom+xml`/`application/rss+xml` type, XML-escapes text, and `Feed::conditional(&headers)` reuses the `etag` layer for `304`s (0.6.0). See `docs/guide/conditional-get.md` |
| A hand-rolled `AtomicU64` + a `MetricsSource` impl (or a whole second `prometheus`/`metrics` crate exporter) just to count something in a handler | `autumn_web::metrics` — `metrics::counter("checkout_completed_total").with_label("status", "paid").increment(1)`, plus `gauge`, `histogram` and `timer(..).start()` (a guard that records on drop, so early `?` returns and panics are covered) / `time` / `time_async`. Registers itself on first use and lands on the stock `/actuator/prometheus` and `/actuator/metrics` (`app` key) with zero `AppBuilder` wiring; caps cardinality (100 *labeled* series/instrument) instead of leaking series (0.7.0, issue #1378). `describe_*` and `set_histogram_buckets` do not register anything, so startup calls work in either order; gauges and histograms take `usize`/`u64`/`i64` directly (`set(queue.len())`). `MetricsSource` is still the answer when a subsystem already owns the numbers. See `docs/guide/metrics.md` |
| Reproducing a production 500 by copying the request into a test and guessing at the database state it saw | `[failure_capture] enabled = true` writes a redacted **failure capsule** (request + `PostgreSQL` wire traffic + clock readings + outcome, one JSON file) for every caught panic/5xx; `autumn replay <capsule>` re-runs it offline against an in-process stub DB — exit 0 reproduced / 1 mismatch / 2 refused. A capsule also carries every framework effect the run produced — outbound HTTP (webhooks included), job enqueues, cache reads/writes, mail, the resolved tenant and every random draw — and replay serves each from the capsule: no socket is opened, no job is queued, no mail is delivered, and a minted UUID/session id/CSRF token reappears byte-for-byte. A failure *inside a job* records a job-scoped capsule that `autumn replay` dispatches. Capsules are production data: read the security section of `docs/guide/failure-capsules.md` before enabling (0.7.0, #1598/#1634) |
| Triaging the same production bug twice because the first fix had no test pinning it | `autumn capsule test <capsule>` converts a capsule into a committed regression test: it copies the capsule's bytes **verbatim** into `tests/capsules/` (so whatever redaction removed stays removed), generates a `#[tokio::test]` beside it, registers both in `tests/integration/mod.rs`, and scaffolds a `capsule_support::router` hook once. The test drives the same replay engine `autumn replay` does and runs under plain `cargo test` with **zero live dependencies** — no network, DB, queue or Docker. `autumn capsule verify` replays the whole committed corpus, which doubles as an upgrade gate: run it against a new Autumn before deploying that version. Job capsules are refused here (no request to drive) — replay those with `autumn replay`. See `docs/guide/failure-capsules.md` (0.7.0, #1634) |
| Proving a retry path survives "the 3rd DB checkout fails" or "the 2nd `send_invoice` execution fails" with a real-clock test that can only hope for the timing, or with `Chaos` rates that never reproduce the exact failure | `autumn_web::sim::FaultPlan` — an **authored**, seed-deterministic fault scenario attached with `TestApp::with_fault_plan(plan)`: `FaultPlan::from_seed(seed).fail_db_checkout(3).fail_job("send_invoice", 2)` fails exactly those effects through the existing interceptor seams (no app code changes), `only_between(from, to)` gates faults on the injected clock, `random_*_faults(n, 1..=k)` picks ordinals from the seed. `client.fault_outcome().await` returns a serializable `FaultOutcome` (`fired` / `suppressed` / `unfired` / `server_errors` via reporting / `final_state`); `to_json_string()` is byte-identical on every replay of a seed under `#[sim_test]`. Drain jobs with `Sim::run_to_idle` (not `perform_enqueued_jobs`, which bypasses `intercept_execute`). See `docs/guide/simulation-testing.md` → "Authored fault scenarios" (#1680) |
| Hand-assembled `Cache-Control` header strings on a handler | `etag::cache_for(Duration)` → `CacheControl`; attach as a tuple `(cache_for(dur).public(), html!{…})` or `.wrap(resp)`. Chain `public`/`private`, `max_age`, `s_maxage`, `stale_while_revalidate`, `no_store`, `no_cache`, `must_revalidate`, `immutable`; `header_value()` renders a deterministic value. Defaults to `private` (a secured page can't be silently made public); composes with `fresh_when` — the directives ride the `200` and the preserved `304` (0.6.0, issue #1344). See `docs/guide/conditional-get.md` |

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

Renaming the `autumn-web` dependency in `Cargo.toml` (`web = { package =
"autumn-web" }`) — e.g. mid-upgrade, to depend on two versions in one crate —
just works: every macro resolves the real crate name automatically. For the
rare case where a crate hosts two differently-keyed copies at once (automatic
detection is then ambiguous), pass an explicit override to any attribute
macro: `#[get("/x", crate = "autumn_web_05")]`.

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

> **Never hand-create a directory under `migrations/`.** Run `autumn generate
> migration <Name>` (or `autumn schema diff --write-migration`) and put your SQL
> in the `up.sql`/`down.sql` it creates — including when you are writing every
> line of that SQL yourself. The generator's job is picking the version.
>
> App, framework and plugin migrations share one version space, keyed on the
> 14-digit prefix, so a hand-typed `20260831000000` collides with whatever
> anyone else authored that day and one of the two silently never runs. The
> generator mints a full `YYYYMMDDHHMMSS` from the clock. CI rejects a `000000`
> time component, a version that isn't a real UTC timestamp, and any duplicate
> (`scripts/check-migration-versions.sh`).

## Cargo.toml

```toml
[package]
name = "my-app"
version = "0.1.0"
edition = "2024"

[dependencies]
autumn-web = { version = "0.7", features = ["db", "htmx", "maud"] }
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
autumn-web = { version = "0.7", features = [
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
| `markdown` | Markdown rendering with frontmatter and static-site support, plus the safe user-submitted rich-text path (`render_user_content`, `rich_text_area`) — see [rich text](../../docs/guide/rich-text.md) |
| `telemetry-otlp` | OpenTelemetry OTLP export |
| `test-support` | Testcontainers-backed `TestApp`, `TestClient`, and `TestDb` |
| `i18n` | Locale extractor, compile-time checked translations, and opt-in locale-prefixed routing |
| `storage` | `BlobStore`, local storage, `Blob` columns, signed URLs |
| `mail` | Transactional email, mailer macros, previews, deferred delivery |
| `seed` | `SeedContext` for seed binaries |
| `system-info` | Optional system information in actuator surfaces |

For S3 storage add `autumn-storage-s3 = "0.7"`; `storage-s3` is no longer an
`autumn-web` feature. For a shared Redis cache add `autumn-cache-redis = "0.7"`.
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

`#[autumn_web::main]` takes optional arguments that tune the tokio runtime it
builds — `flavor` (`"multi_thread"`, the default, or `"current_thread"`),
`worker_threads`, `max_blocking_threads`, `thread_name`, `thread_stack_size`,
`thread_keep_alive = "30s"`, and `configure = path::to::fn`, a
`fn(&mut tokio::runtime::Builder)` run after the others as the escape hatch for
`Builder` methods the list doesn't name (unreleased). Numeric arguments take
expressions, not only literals. Reach for them only with a measurement in hand:
with no arguments the runtime is tokio's defaults, and the job runner,
scheduled tasks, and mailer share it, so an undersized worker count throttles
those too. An unknown, repeated, or zero-valued argument — or `worker_threads`
under `flavor = "current_thread"`, where it would do nothing — is a compile
error. See `docs/guide/getting-started.md` "Tuning the Tokio runtime".

## AppBuilder API

| Method | Purpose |
|---|---|
| `.routes(routes![...])` | Register route handlers |
| `.static_routes(static_routes![...])` | Register `#[static_get]` routes for `autumn build` |
| `.tasks(tasks![...])` | Register scheduled `#[scheduled]` work |
| `.jobs(jobs![...])` | Register request-triggered `#[job]` work |
| `.one_off_tasks(one_off_tasks![...])` | Register operational `#[task]` commands |
| `.migrations(MIGRATIONS)` | Register embedded Diesel migrations |
| `.plugin_migrations(name, MIGRATIONS)` | Register a plugin's embedded Diesel migrations (named; version collisions with other sources auto-resolve) |
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
| `.with_mail_suppression_store(store)` | Install a durable bounce/complaint suppression backend (0.6.0; in-memory default auto-wired otherwise) |
| `.with_audit_sink(sink)` | Install structured audit sink |
| `.listeners(listeners![...])` | Register `#[listener]` event listeners (0.6.0) |
| `.static_gate(layer)` | Middleware that also guards `#[static_get]` pre-render (0.6.0; `has_static_gate::<L>()`, `get_static_gate_types()`, `TestApp::static_gate` mirror it) |
| `.with_shard_router(router)` | Install a shard router for `[[database.shards]]` (0.6.0) |
| `.seo_source(source)` | Register a `SitemapSource` for dynamic `/sitemap.xml` entries (0.7.0) |
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

**Declare the `Content-Type` on a `#[static_get]` route that is not HTML
(unreleased, #1832).** `autumn build` records the type each handler's response
declares into `dist/manifest.json`, and the static-first middleware serves that
value verbatim — it no longer guesses from the route slug, which it had to do
because every non-root route is stored as `<route>/index.html`. So the handler's
own header is the whole contract:

```rust
#[static_get("/sitemap.xml")]
async fn sitemap() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/xml")], build_sitemap())
}

#[static_get("/feed")]                       // extensionless — no extension to infer from
async fn feed() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/rss+xml")], build_feed())
}
```

Only a type you *declare* is recorded. axum attaches one to every response, but
for `String` (`text/plain; charset=utf-8`) and `Vec<u8>`
(`application/octet-stream`) it comes from the return type, not from you — so on
a route whose own extension says otherwise (`/theme.css`, `/logo.png`) those two
exact values are ignored and the extension wins, which is what stops a `->
String` stylesheet from being served as plain text and dropped by `nosniff`.
The two are indistinguishable in the response, so if you genuinely want one of
them on such a route, declare it distinctly (bare `text/plain`, or
`application/octet-stream` with a parameter) and it is recorded; to force a
download prefer `Content-Disposition: attachment`. Extensions outside Autumn's
asset table (`.pdf`, `.zip`) are unaffected.

Return `Markup` (or `Html<String>`) for HTML pages: on an extensionless route a
handler returning a bare `String` declares `text/plain; charset=utf-8`, which is
what will now be served.
A `dist/` built before this release records nothing and keeps its previous derived
type until the next `autumn build`. ISR refuses a regeneration whose handler
declares a *different* type than the manifest recorded, so a stale-but-correctly
-typed page is served instead of fresh bytes under the wrong header — re-run
`autumn build` after changing a route's type.

**Route-level SEO defaults (issue #1182, 0.7.0):** declare
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

**`sitemap.xml` and `robots.txt` (0.7.0):** set `[seo] base_url =
"https://example.com"` in `autumn.toml` and the framework mounts `GET
/robots.txt` and `GET /sitemap.xml`. `robots.txt` defaults by profile
(`dev`/`test` → `Disallow: /`, `prod` → `Allow: /`); override with `[seo.robots]
allow_all`, add lines with `additional_rules`, and pin the `Sitemap:` URL with
`sitemap_url` (otherwise derived from `base_url`). The sitemap gets one entry
per concrete `#[static_get]` path for free; every other URL comes from a
`SitemapSource` registered with `.seo_source(...)`:

```rust
use autumn_web::seo::{SitemapEntry, SitemapSource};

impl SitemapSource for PostSitemap {
    fn entries(&self) -> Pin<Box<dyn Future<Output = Vec<SitemapEntry>> + Send + '_>> {
        Box::pin(async {
            vec![SitemapEntry::new("https://example.com/posts/hello")
                .lastmod("2026-05-01")
                .changefreq(autumn_web::seo::SitemapChangefreq::Weekly)]
        })
    }
}
```

Traps: `entries()` is awaited **once**, while the router builds, and the body is
cached — the sitemap is a start-up snapshot, so an app needing live entries must
register its own `/sitemap.xml` route (the framework detects the collision, warns,
and mounts neither of its own SEO routes). A source cannot use the `Db` extractor
(no `AppState` yet) — build a pool with `autumn_web::db::create_pool(&config.database)`.
`seo(robots = "noindex")` drops a route from the sitemap only for `#[static_get]`
paths the framework derived itself; `SitemapSource` entries are never filtered, so
omit the URL from the source instead. `sitemap_xml` truncates past 50,000 URLs.
`autumn build` writes both files into `dist/`. Guide: `docs/guide/seo.md`; runnable
example: `examples/reddit-clone` (`src/seo.rs`, `autumn.toml`).

**Locale-prefixed routing (issue #1251, 0.7.0):** set
`[i18n] locale_prefix_enabled = true` in `autumn.toml` (default `false`) and
every route registered via `routes![...]` becomes reachable under
`/{locale}/...` for each `supported_locales` entry — zero duplicated route
definitions. An unknown `{locale}` 404s; a bare, non-prefixed path
308-redirects to the negotiated locale's prefixed path, preserving the query
string; the URL segment outranks cookie/session/`Accept-Language` for the
existing `Locale` extractor (no handler changes). `[i18n]
locale_prefix_exclude = ["/api", "/actuator"]` keeps machine routes
unprefixed. Use `locale_switcher(path, locale.tag(), supported_locales)` (and
the lower-level `localized_path(path, locale)`) from
`autumn_web::widgets` to render a path-preserving language switcher, and
`SeoMeta::hreflang_alternates(locale_alternates(base_url, path,
default_locale, supported_locales))` to emit `hreflang` `<link>` tags for the
current page's localized variants.

**Duplicate-route preflight (issue #1012, 0.6.0):** two
handlers that resolve to the same `(method, path)` after `.scoped(...)` prefix
resolution — including `#[repository]`-generated API routes — fail app build
with `RouterBuildError::DuplicateUserRoute { method, path, existing, incoming }`
BEFORE any router mounts, instead of the previous `axum::MethodRouter::merge`
startup panic. Distinct methods on the same path (`GET /admin` + `POST /admin`)
still merge cleanly. Opaque `.merge(...)`/`.nest(...)` routers can't be
introspected — a non-empty opaque table emits a `tracing::warn!`
("check skipped") rather than a false pass. See
`docs/guide/getting-started.md` "Route collision diagnostics".

### Markdown-backed pages + SSG (feature `markdown`)

`MarkdownRegistry` parses `.md` files with `+++` TOML frontmatter (`title`
required; `description`/`order` default to `""`/`0`) and pairs with
`#[static_get]` so one handler serves live requests in dev and pre-renders in
`autumn build`. Build it once in a `OnceLock` from `from_embedded(&[MarkdownSource
{ slug, content: include_str!(...) }])` or `from_dir(path)` (non-recursive);
pages sort by `(order, slug)`.

```rust
async fn doc_params(_router: axum::Router) -> Vec<StaticParams> {
    docs().static_params()            // one entry per page, keyed "slug"
}

#[static_get("/docs/{slug}", params = doc_params)]
async fn show(Path(slug): Path<String>) -> AutumnResult<Markup> {
    let page = docs().get(&slug).ok_or_else(AutumnError::not_found)?;
    let out = render(&page.body, RenderOptions::default());
    Ok(layout(&page.frontmatter.title, html! { (PreEscaped(&out.html)) }))
}
```

`static_params()` keys every entry `"slug"`. If the route names its parameter
anything else — `#[static_get("/docs/{page}", …)]` — use
`static_params_for("page")` (0.7.0, issue #743); a mismatched
key leaves `{page}` unsubstituted and the SSG build panics on the invalid URI.

`render` returns `{ html, toc }` and injects heading anchors. Anchors are
document-unique: each heading keeps the slug its own text produces, and only
*repeats* are suffixed `-1`, `-2`, … A suffix never takes a slug another heading
owns by name, so `## Example` / `## Example` / `## Example 1` yields `example`,
`example-2`, `example-1` — deep links stay put. Headings with no alphanumeric
characters get no `id` at all.

`render` is for **trusted, build-time content only** — it applies no URL-scheme
allowlist. For anything a request body carried in, use `render_user_content`
(see [rich text](../../docs/guide/rich-text.md)). The framework ships no docs
theme; compose `out.html`/`out.toc` into your own Maud layout. Worked example:
`examples/wiki` (`src/routes/docs.rs` + `content/*.md`).

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

### Associations and preloading (0.6.0)

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

**Never hand-roll a votes/likes table.** `#[votable(by = <Reactor>)]` on a
`#[model]` declares a `(reactor, target)`-unique edge table plus a
denormalised aggregate column on the target **(0.7.0, #1362)**:

```rust
#[autumn_web::model]
#[votable(by = User, aggregate = sum)]   // or: aggregate = count, name = like
                                         // — always BELOW #[model]
pub struct Post {
    #[id]
    pub id: i64,             // must be i64
    pub score: i64,          // the aggregate column, must be i64
}
```

`aggregate = sum` (default) → edge `value SMALLINT`, target `score =
SUM(value)`; `aggregate = count` → no value column, target `{name}_count =
COUNT(*)`. Defaults: `name = vote`, `table = pluralize(name)`, `reactor_fk =
{snake(by)}_id`, `target_fk = {snake(Model)}_id`, `value_column = value`,
`column = score` / `{name}_count` — each overridable. At most one `#[votable]`
per model. The edge table is the app's migration to write, and its composite
`UNIQUE (reactor_fk, target_fk)` is **load-bearing** (it is the `ON CONFLICT`
arbiter), as is a `CHECK` on the value column — `react()` does **not** validate
`value`, so never bind it from a request. The aggregate column must be `BIGINT
NOT NULL DEFAULT 0`; both FKs should be `BIGINT NOT NULL` (a nullable target FK
is tolerated — the unique constraint then covers only the non-`NULL` rows,
which are exactly the ones `react()` writes).

The model emits a `{Model}Reactions` trait blanket-implemented for that
model's repository — import it as `_`, no repository attribute needed:

```rust
use crate::models::PostReactions as _;

let r = posts.react(user_id, post_id, 1).await?;  // count mode: no `value` arg
r.value;      // Option<i16> — this reactor's reaction AFTER the call
r.aggregate;  // i64 — the newly persisted score, exact as of commit
r.outcome;    // ReactionOutcome::{Inserted, Flipped, Removed}
let mine: Option<i16> = posts.reaction_of(user_id, post_id).await?;
```

`react()` is a race-safe toggle: same value again removes the edge, a
different value flips it, a new one inserts it — and the aggregate is
recomputed from ground truth and persisted in the **same transaction** under a
`FOR NO KEY UPDATE` lock on the target row, so concurrent reactions converge to
at most one edge per pair and a reader never sees edge/aggregate disagreement.
A toggle is not idempotent — never blindly retry a timed-out call.
Soft-deleted targets are `NotFound`. `reaction_of()` is a read and is
replica-eligible (no read-your-writes pin — re-render from the `Reaction` the
write returned). **Like the m2m helpers, `react()` takes its own pooled
connection — never hold a `Db` extractor across the call, or the handler needs
two connections at once and deadlocks at pool-size concurrency.** Pair it with
the `reaction_controls` widget (see the widgets section), threading the CSRF
token so the no-JS form POST works. See `docs/guide/votable.md` and `examples/reddit-clone`
(`src/routes/votes.rs`).

**Never hand-roll a comments table.** `#[commentable]` on a `#[model]` is
Autumn's fifth association kind — the **polymorphic** one **(0.7.0, #1367)**. `belongs_to`/`has_many`/`has_one`/`through` pin the child
to one parent table; this does not:

```rust
#[autumn_web::model]
#[commentable(by = User, author_name = username)]   // always BELOW #[model]
pub struct Post {
    #[id]
    pub id: i64,              // must be i64
    #[default]
    pub comment_count: i64,   // the counter column, must be i64
}
```

One `comments(commentable_type, commentable_id, parent_id, author_id, body,
created_at, deleted_at)` table serves EVERY commentable model — adding comments
to a second model is that attribute plus a `comment_count` column, nothing
else. Defaults: `type_name = <Rust type name>`, `table = comments`,
`counter_cache = comment_count` (or `false`), `max_depth = 5` (top level is
depth 0), `max_body = 10000` bytes; `author_name` is unset by default (the
framework will not guess a column, and a scaffolded `User` carries an `email`).
Renaming the struct changes `type_name` and orphans existing rows — pin it
first. `autumn generate scaffold post title:string comments:commentable`
emits the table (once per project), the column, and the attribute.

The model emits a `{Model}Comments` trait blanket-implemented for that model's
repository — import it as `_`:

```rust
use crate::models::PostComments as _;

let c = posts.add_comment(post_id, author_id, "first!", None).await?;
let r = posts.add_comment(post_id, author_id, "…", Some(c.id)).await?;  // reply
let thread: Vec<CommentNode> = posts.comment_thread(post_id).await?;    // nested
let removed: usize = posts.delete_comment(post_id, c.id).await?;        // + subtree
let live: i64 = posts.recompute_comment_count(post_id).await?;          // drift repair
```

`comment_thread` is ONE query whatever the depth (nested in Rust, stable
`(created_at, id)` order, soft-delete aware). `add_comment` probes and
row-locks the parent first — `commentable_id` has no foreign key, so that
probe IS the referential check: an unknown/soft-deleted/foreign-tenant parent
is `404`, a `reply_to` on a different record or past `max_depth` is `422`, and
`comment_count` moves via the counter-cache primitive in the **same
transaction**. `delete_comment` is idempotent and decrements by the rows it
actually removed. **Like `react()`, these take their own pooled connection —
never hold a `Db` extractor across the call.**

Mount the routes ONCE for the whole app; the registry dispatches on the type
segment, so a third commentable model needs no route:

```rust
.nest("/comments", autumn_web::commentable::router(Default::default()))
// GET/POST /comments/{commentable_type}/{parent_id}
```

The router authorizes the **tenant**, never the record — an app with private or
role-gated records MUST set `CommentsConfig::authorize(|access| ...)`, or skip
the router and call the helpers from its own authorized handlers. Build a host
page's own thread with `commentable::thread_dom_id`/`thread_action`, or the
router's re-render lands on a different element and every htmx swap after the
first one misses.

Render with the no-JS `comment_thread` widget (see the widgets section),
threading the CSRF token and `return_to`. See `docs/guide/commentable.md` and
`examples/reddit-clone` (`Post` **and** `Subreddit`, zero comment routes).

**Never hand-write `count + 1` on a parent.** `counter_cache` on a child's
`#[belongs_to]` maintains a denormalised `{child}_count` column on the parent
**(0.7.0, #1325)**:

```rust
#[autumn_web::model]
#[belongs_to(Post, counter_cache)]              // -> posts.comment_count
pub struct Comment {
    #[id]
    pub id: i64,
    pub post_id: i64,
}
```

The child's repository then maintains it on `save`, `update`, `delete_by_id`,
`restore`, `purge`, every bulk variant, and `dependent(...)` cascades — **inside
the same transaction as the row mutation**, with a single atomic `UPDATE posts
SET comment_count = comment_count + $1`. Never a read-modify-write, so N
concurrent inserts yield exactly N. Soft-deleting decrements (the count reflects
live rows) and `restore` puts it back; reassigning the foreign key moves the
count from the old parent to the new one; a leg whose FK did not change issues
no statement at all.

The column defaults to `{snake(child)}_count` — **singular**, matching
`#[votable(aggregate = count)]` — and is overridable with `counter_cache =
"<column>"`. `counter_cache_tenant = "<column>"` confines every delta to the
caller's tenant (both tables must carry that column); without it no tenant
predicate is emitted. It is a **`belongs_to`** option only — on a `has_many`, on
a `through =` join table, with a non-identifier column, with two legs resolving
onto one parent column, or on a composite `#[id]`, it is a directed compile
error.

The column is the app's migration to write (`ALTER TABLE posts ADD COLUMN
comment_count BIGINT NOT NULL DEFAULT 0` — `NOT NULL DEFAULT 0` is load-bearing,
since `NULL + 1` is `NULL`), or `autumn generate scaffold … --belongs-to Post
--counter-cache` emits it. Every counter-cached repository also gains
`recompute_counter_caches()` / `recompute_counter_caches_for(parent_id)` — an
idempotent rebuild from the source of truth, which is both the backfill for a
table adopting the column and the repair for drift. Applications that insert
with raw Diesel can opt in with
`counter_cache_after_insert_by_id(conn, Comment::counter_caches(), id)`. Two
paths are **not** yet maintained: a derived `delete_by_<field>` and
`find_or_create_by_<field>` — `recompute` is the remedy. See
`docs/guide/counter-cache.md`.

**For a filtered or weighted count, use `#[derivation]`** on the child, below
`#[model]` **(0.7.0, #1769)**: `#[derivation(Post, column =
"published_comment_count", filter = published)]`, or `#[derivation(Post, column
= "visible_score", transform = sum(score), filter = published && score > 0)]`.
It is a superset of `counter_cache` (which is the unfiltered case) and rides the
same mutation paths and same-transaction, atomic set-based SQL, including bulk
paths, soft delete/restore, cascades, re-parenting and a filter flip on an
unchanged parent. The filter grammar is `field`, `!field`, `field OP <int|bool|
string literal>`, `field.is_some()`/`is_none()`, `a && b` and parentheses over
`bool`/integer/`String` fields and their `Option` forms; each filter is lowered
to both Rust and SQL, and string ordering comparisons and float literals are
compile errors. Other keys: `fk`, `tenant`, `name`. The parent column is the
app's migration (`BIGINT NOT NULL DEFAULT 0`); the `_autumn_derivations` state
table ships as a framework migration and is applied automatically. Each
derivation is content-addressed, so a changed filter enqueues a resumable,
idempotent backfill at boot (`run_backfill`, `BackfillOptions`), and
`GET /actuator/derivations` (sensitive-gated) reports hashes, backfill state,
checkpoint and drift, with `recompute(conn, name)` as the repair. See
`docs/guide/derivations.md`.

Deleting a parent can cascade to its children in one transaction — declare
`dependent(...)` on the parent's `#[repository]` instead of hand-writing the
child cleanup **(0.6.0)**:

```rust
#[autumn_web::repository(Post,
    dependent(CommentRepository, fk = "post_id", on_delete = destroy))]
pub trait PostRepository {}
```

`on_delete` = `destroy` (soft-delete-aware, fires each child's hooks; since
0.6.0 this **recurses** into each child's own `dependent`, cascading
into grandchildren — see the recursive/bulk/model-side note below) |
`delete_all` (bulk delete, no child hooks) | `nullify` (set the FK null) |
`restrict` (probe for referencing rows *before* mutating; errors `cannot delete:
dependent N row(s) …` if any still exist). The generated `delete_by_id` loads and
locks the parent, applies every declared action, then deletes the parent — all in
a single transaction (Part of #1369). See `docs/guide/repositories.md`.

Since 0.6.0 the cascade is recursive and bulk-aware, and can be declared on
the model instead of the repository **(0.6.0, issues #1738,
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
| `from_shard(&ShardedDb)`, `with_pool_untracked(pool)` | **(0.6.0)** shard-scoped construction. `with_pool_untracked` is the 0.6.0 rename of `with_pool`; 0.5.x repositories had **no** pool constructor at all. An app carrying the older `with_pool` name is migrated by `autumn upgrade --apply` (codemod `0.6.0-repository-with-pool-untracked`, issue #1629) rather than by hand |
| `find_in_batches(batch_size)`, `find_each(batch_size)` | **(0.6.0)** Bounded-memory whole-table iteration via a primary-key keyset cursor (`WHERE id > last ORDER BY id ASC LIMIT batch_size` — never `LIMIT`/`OFFSET`), generated on every repository. `find_in_batches` returns a `FindInBatches` handle — drive with `while let Some(chunk) = b.next_batch().await?`; `find_each` returns `FindEach` yielding one model per `next().await?`. Inherits soft-delete filtering, tenant scoping, and read routing like `find_all`; errors are retryable (cursor advances only on success; `Ok(None)` always means completion); `batch_size == 0` errors instead of spinning; `batch_size` is **not** clamped to `MAX_PAGE_SIZE`; sharded repos reject cross-shard `across_tenants()` iteration (iterate per shard via `from_shard`). Handle types: `autumn_web::batches::{FindInBatches, FindEach, BatchSource}` (not in the prelude). See "Batched iteration" in `docs/guide/pagination.md` |
| `find_or_create_by_<field>[_and_<field>...](<field>, &new)` | **(0.6.0)** Race-safe get-or-insert; declare `fn find_or_create_by_slug(slug: String);` (lookup fields only) to generate an inherent `find_or_create_by_slug(&self, slug: String, new: &NewModel) -> AutumnResult<(Model, bool)>`. Reads on the read path first (tenant/soft-delete aware), else inserts on the primary with `ON CONFLICT DO NOTHING` — under concurrency exactly one row is created, exactly one caller sees `created == true`, and no `23505` escapes. `before_/after_create` + commit hooks fire only on the created path; works on hooked repos (unlike `upsert_many`). **Requires a unique constraint on the lookup column(s)** (`_or_` is rejected). See "Race-safe get-or-insert" in `docs/guide/repositories.md` |
| `ledgered = true` / `ledgered(valid_time = "col")` (attr) | **(0.7.0, issue #1699)** Makes the entity bitemporal and tamper-evident: every insert, update and soft-delete appends an immutable, hash-chained revision carrying a **full row snapshot** to `_autumn_ledger_revisions`. Adds `ledger_as_of(id, at)`, `ledger_as_of_at(id, LedgerAsOf)`, `ledger_diff(id, from, to)`, `ledger_revisions(id)`, `ledger_verify(id)`, `ledger_head(id)`, `ledger_high_water(id)` and `ledger_pin(id)` (both at once, from one snapshot — what an audit posture pins outside the database). Implies `versioned = true` and **requires `soft_delete`** (a hard DELETE would erase the row the ledger reconstructs); `purge` is not generated, and `#[version_history(sensitive = [...])]` / `no_versioned_record_impl` are compile errors. See "Ledgered entities" below |
| `retention(after = "30d", basis = created_at)` / `retention(purge_deleted_after = "90d")` (attr) | **(0.7.0)** Declarative data-retention: reach for this instead of hand-writing a `#[scheduled]` cleanup fn for expiring sessions, drafts, one-time codes, or other transient rows. Compiles to a batched (`batch_size`, default 500), cursor-paginated sweep auto-registered with fleet coordination — no `tasks![...]` entry needed. On a `soft_delete` repository, `after` soft-deletes (never re-touching an already-deleted row) and `purge_deleted_after` hard-purges (re-checking `deleted_at` at delete time so a concurrent `restore()` survives); without `soft_delete`, `after` hard-deletes. Sweeps run across **all** tenants on a `tenant_scoped` repository (no per-tenant opt-out) and are not supported on `sharded` repositories (compile error). `autumn retention --dry-run [--model NAME]` reports rows-that-would-be-swept without deleting. Emits `retention_sweep_rows_total` / `retention_sweep_duration_seconds` metrics + a structured log line per run. See `docs/guide/retention-sweeps.md` |

Read routing: with `database.replica_url` set, all generated reads use the
replica automatically; writes always hit the primary. See
`docs/guide/repositories.md` and `docs/guide/pagination.md`.

### Ledgered entities — time travel + tamper evidence (0.7.0, issue #1699)

Reach for this when an entity's *past* is part of the product: regulated
records, anything an auditor will ask about, anything you may need to undo.

```rust
#[repository(Invoice, soft_delete, ledgered = true)]
pub trait InvoiceRepository {}

let then  = repo.ledger_as_of(id, last_tuesday).await?;   // Option<Invoice>
let delta = repo.ledger_diff(id, last_tuesday, now).await?;
let proof = repo.ledger_verify(id).await?;                // LedgerVerification
```

The marker is the only per-model change — do **not** hand-write revision
bookkeeping. `ledgered` implies `versioned`, so every write path version history
already covers (hand-written handlers, generated `api = "…"` endpoints, `#[job]`
/ `#[mailer]`, bulk saves, upserts, `find_or_create_by`, dependent cascades)
appends a revision automatically.

What to know when writing app code against it:

- **`soft_delete` is mandatory** and `purge` does not exist. `delete_by_id`
  records a delete revision; `restore` records the undelete. Both keep the
  ledger and the table in agreement.
- **As-of is byte-for-byte** what a plain query would have returned. It resolves
  soft-deleted state too, so check `deleted_at` exactly as against the table.
- **Two time axes.** `recorded_at` is transaction time; `valid_from` defaults to
  it, or comes from your own column via `ledgered(valid_time = "effective_at")`
  (`DateTime<Utc>` / `NaiveDateTime` / `Option` of either). Query both with
  `LedgerAsOf::{transaction, valid, bitemporal}` via `ledger_as_of_at`. Both
  bounds *filter*; the newest surviving revision wins.
- **`ledger_verify` is the audit answer.** Beyond the hash chain it
  cross-checks a per-record **high-water mark** kept outside the deletable
  revision rows (`_autumn_ledger_high_water`, issue #2323) and the live row. A
  deleted revision therefore leaves a permanent `MissingRevision` gap rather
  than a window an ordinary write closes, a wholly erased chain is
  distinguishable from a row that predates ledgering, and tampering with the
  mark itself reports `HighWaterBehind` / `HighWaterMismatch` /
  `HighWaterMissing`. A write that reached the table without appending a
  revision is still `LiveStateMismatch`. Appends **refuse** when the mark and
  the chain are in a state no framework path produces, so traffic cannot launder
  evidence away. Pin `ledger_head(id).hash` outside the database to catch a
  wholesale rewrite — the hashing rule is open source, so in-database evidence
  cannot cover that.
- **Transaction time comes from the database** (`clock_timestamp()` /
  `strftime(…, 'now')`), clamped so it is non-decreasing along a chain by
  construction; `RecordedAtRegression` reports a chain where it is not.
- **Reads are tenant- and shard-scoped.** `across_tenants()` and cross-shard
  ledger reads are rejected (a chain is per `(tenant, record)`); read inside a
  tenant scope.
- **Cost is real**: one indexed `SELECT` (chain head, high-water mark and the
  database clock together) plus the revision `INSERT` and the mark's upsert per
  write inside the same transaction — a fourth statement on a delete — per row
  on bulk paths, and a full row snapshot per revision. Don't ledger a high-churn
  table by reflex.

`_autumn_ledger_revisions` and `_autumn_ledger_high_water` arrive with
`autumn migrate` (Postgres and SQLite); migrate before rolling out the binary.
See `docs/guide/ledgered-entities.md`.

### JSON API endpoints — page envelope + write validation (0.6.0)

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

### Partial-update validation — the effective merged model (0.6.0, issue #1778)

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

**`#[validate(nested)]` hazard (issue #1751):** `validator_derive`'s `nested`
codegen calls a field's value with a bare `.validate()`, which collides with
this crate's own `ValidateExt` (also named `validate`, blanket-implemented for
every `Validate` type and re-exported from `autumn_web::prelude`) whenever the
struct's own defining module ALSO imports the prelude (or `ValidateExt`
directly) — a cryptic `E0034: multiple applicable items in scope` pointing
into the derive expansion, on create as much as on update, and on `#[model]`
structs as much as hand-rolled ones. It doesn't matter what a downstream
handler module imports — only the struct's own module's imports matter. Avoid
it by keeping a `#[validate(nested)]` struct's module free of the prelude/
`ValidateExt` import (use fully-qualified `Db`/etc. paths there instead), or by
replacing `nested` with `#[validate(custom(...))]` calling the nested value's
`validator::Validate::validate` explicitly.

### Transactions

`Db::tx(f)` runs a READ COMMITTED transaction. Since
**0.6.0**, `Db::tx_with(opts, f)` adds isolation levels and automatic
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

**(0.6.0)** Scoped service tokens: mint named,
optionally-expiring API tokens carrying flat scopes via `IssueTokenSpec` +
`issue_scoped_api_token`; gate handlers with
`#[secured(scopes = ["posts:write"])]` (no session required — default-deny,
403 when missing) or `#[secured("admin", scopes = [...])]` for both; check in
policies with `PolicyContext::has_scope/has_any_scope/has_all_scopes`; manage
with `autumn token issue <principal> --name ... --scope ...` / `list` /
`rotate` / `revoke` — all four ship in 0.7.0. `list` and `rotate` arrived in
0.6.0; a 0.5.x CLI has only `issue` / `revoke`.

Active session management ships with `autumn generate auth` (0.5.0):
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

### Admin impersonation (unreleased, issue #1394)

"Log in as this user" without breaking the audit trail. Never hand-roll it
with `session.insert("user_id", target)` — that makes every subsequent version
row and audit event claim the *customer* did it.

`autumn_web::auth::impersonation::begin_impersonation(&state, &session, target)`
swaps the session's **effective** user and records the real admin separately
under a reserved `impersonator_id` key, so `#[secured]` / `RequireAuth` /
`PolicyContext` see the impersonated user while the ambient current actor
(`Current::actor`, which seeds `#[repository(versioned)]` rows and audit
events) stays the **real impersonator**. `end_impersonation(&state, &session)`
reverts. `impersonator_id(&state, &session)` is the separate accessor;
`impersonation_state(&state, &session)` returns both ids, and the
`Impersonation` extractor is the handler-side form. Register the gate with
`AppBuilder::impersonation_gate(...)`.

Default-deny: it needs an `ImpersonationGate` in `AppState` —
`ImpersonationGate::allow_roles(["admin"])`, or `::custom(policy)` with an
`ImpersonationPolicy` (the seam for tenancy checks and for
`target_role`, which resolves the impersonated session's role **server-side**;
never accept a role from request input). Missing gate or a refusal is `403`.
Both edges rotate the session id and write `auth.impersonation.begin` /
`.end` audit events carrying `{impersonator_id, target_id}`; a begin is refused
when the audit write fails **or when no audit sink is installed at all** — wire
`AppBuilder::with_audit_sink(...)` before enabling it. The operator's step-up
claim is stashed and dropped for the duration (so a `#[step_up]` action cannot
be run on the target's account on the operator's re-auth) and restored on
revert; the record is bound to the user it names *and* to the session
generation that created it, so a login (which rotates the session id) retires
it — a forgotten impersonation cannot be inherited by the next user, not even by
the impersonated customer signing in as themselves. Call
`impersonation::clear(&session)` from any login flow to drop it outright. No nesting (`409`), no self-impersonation, and
the admin's own step-up claim is preserved rather than refreshed.

For the UI, `AdminPlugin::with_impersonation(gate)` mounts
`POST {prefix}/impersonate` (behind the role gate + the `ImpersonationGate`)
and `POST {prefix}/impersonate/stop` — deliberately *outside* the role gate, so
an operator impersonating a non-admin is never trapped — and renders a
persistent "Viewing as … — Stop impersonating" banner on every admin page. Put
the same banner in the app's own layout with
`autumn_admin_plugin::impersonation_banner_for(&state, &session, "/admin",
csrf_token, csrf_form_field)` plus `IMPERSONATION_BANNER_CSS`. Session-based
auth only. See `docs/guide/authentication.md` and `docs/guide/admin.md`.

### Cookie consent (0.7.0, issue #1214)

`autumn new` scaffolds a cookie-consent banner and a real consent gate by
default — no third-party tracker, no JS. `autumn_web::consent::Consent` is an
extractor read straight off the request's `Cookie` header (no middleware
dependency); `consent.allows("analytics", POLICY_VERSION)` is the actual
enforcement gate (returns `false` for every category except `"necessary"`
until a decision is recorded under the current policy version).
`accept_all_cookie` / `reject_non_essential_cookie` / `expire_consent_cookie`
build the `Set-Cookie` value recording categories + policy version +
timestamp. `inject_consent_banner` (behind the `maud` feature; mirrors the
dev-mode live-reload injector) auto-splices the banner into every HTML
response — no change to the shared `layout()` signature is needed. Takes the
CSRF cookie name and form-field name as explicit parameters
(`DEFAULT_CSRF_COOKIE_NAME` / `DEFAULT_CSRF_FORM_FIELD` when unconfigured)
rather than reading them off request/response state, since `CsrfLayer` always
sits inner to user layers. Skips injection entirely for an internal `autumn
build` / ISR render (`static_gen::RenderDeadlineExempt`) rather than baking
the banner into the static file written to `dist/`. Bump the scaffolded
`CONSENT_POLICY_VERSION` constant to invalidate prior consent and re-show the
banner. Strictly-necessary cookies (session, CSRF) are never routed through
the gate.

### Audit actor attribution (0.6.0)

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

OAuth2/OIDC social login has shipped since the 0.5.0 line. Do not repeat the stale
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

`autumn generate webhook <provider> <Name>` (0.7.0, #1366)
scaffolds the whole endpoint: the `#[post]` handler over `SignedWebhook`, an
event-type dispatch skeleton, the `[[security.webhooks.endpoints]]` config
(`secret_env`, replay protection on — CSRF/submit-token/CAPTCHA exemptions are
derived from that block by the framework, so none are written), and tests
asserting 200/400/401/409 for valid/missing/wrong-signature/replayed
deliveries. Presets: `stripe`, `github`, `slack`, `generic`.

Read `docs/guide/signed-webhooks.md` and `examples/signed-webhooks/`.

## Mail CSS inlining — render styled in Gmail/Outlook (0.6.0)

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

## Mail suppression — stop emailing bounced addresses (0.6.0)

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

## In-app notifications (0.7.0, issue #1148)

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

### Web Push (tab closed)

`autumn_web::push` delivers to a subscribed browser even when the app is
closed — the leg neither the in-app feed nor `channels` can cover. The
developer writes **zero** crypto: VAPID signing (RFC 8292) and payload
encryption (RFC 8291) are the framework's.

- **Setup is three steps:** `VapidKey::generate()` once (offline) → `[push]
  private_key`/`subject` in `autumn.toml` → `autumn generate pwa` (which mounts
  `autumn_web::push::router()`, emits the SW `push`/`notificationclick`
  handlers and the client subscribe snippet, and scaffolds the
  `push_subscriptions` migration). Then `push.send(user_id,
  &PushMessage::new(title, body).url(target))`.
- **`WebPush` is an extractor** (like `Session`/`Db`/`Notifications`):
  `send`, `send_many`, `subscribe`, `unsubscribe`, `vapid_public_key`.
- **`PushPrincipal` accepts both id shapes** — `i64` (as the notification feed
  uses) and `&str`/`String` (as auth tokens carry) — so composing with #1148
  needs no conversion.
- **Storage resolution** mirrors the feed's: `with_push_subscription_store(...)`
  → `DbPushSubscriptionStore` (the generated `push_subscriptions` table) →
  `MemoryPushSubscriptionStore`. `endpoint` is UNIQUE and the identity: a
  re-subscribe updates the row, and a re-subscribe under a different principal
  MOVES it (shared device, second user signs in).
- **Composing with #1148:** await the `notify(...)` write (durable record),
  then `push.send(...)` best-effort and log a failure — the same posture
  `notify_with_push` takes with its channel broadcast. Never let a push failure
  fail the notification write.
- **Never a silent no-op:** an unusable `[push] private_key` fails the BOOT;
  `send` with no key is `PushError::NotConfigured` raised before dispatch;
  `GET /push/vapid-public-key` answers 503 when unconfigured.
- **Pruning:** 404/410 removes the subscription (`report.pruned`); 5xx/429/
  transport errors are counted in `report.failed` and LEFT in place — pruning
  on a transient failure would unsubscribe everyone during an outage.
- **Subscribe is an SSRF boundary:** the endpoint is client-supplied and later
  POSTed to, so it must be `https` with a non-loopback domain host (IP literals
  refused). The built-in routes resolve the principal server-side and 401 when
  they cannot; unsubscribe is scoped to the caller.
- **Testing:** `RecordingPushTransport` captures requests (assert endpoint,
  headers, encrypted body) and `.responding_with(endpoint, 410)` drives the
  pruning path. Wire it with `TestApp::with_web_push(...)`.
- Guide: `docs/guide/web-push.md`. Out of scope by design: native mobile push
  (APNs/FCM), notification actions/images/badges, preferences/quiet hours.

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

Job attributes beyond `name`/`max_attempts`/`backoff_ms` (0.5.0):
`#[job(unique)]` dedupes on an args hash, `unique_by = "field"`,
`unique_window = "running"|"pending"`, `unique_for_ms = N` (debounce),
`concurrency = N` + `concurrency_key = "field"` caps simultaneous runs. A
coalesced enqueue is a no-op `Ok(())`.

**(0.6.0)** jobs additions:

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

**(0.6.0)** Events & listeners — a typed domain event bus so one action
can fan out without inline coupling: `#[event]` on a struct, publish via the
`Events` extractor (`events.publish(UserSignedUp { .. }).await?`), react with
`#[listener(UserSignedUp)]` (sync, in-request) or
`#[listener(UserSignedUp, durable, max_attempts = 5)]` (jobs-backed), and
register with `.listeners(listeners![...])`. Do not also list durable
listeners in `jobs![...]`. See `docs/guide/events.md`.

## Distributed locks (0.6.0)

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

## The plugin API stability contract (unreleased, issue #1601)

**When advising a plugin author, start here.** Autumn declares which
plugin-facing APIs are stable and which are experimental, and a plugin declares
which `autumn-web` versions it supports.

The registry is `autumn_web::plugin_contract::PLUGIN_SURFACES`; the rendered
table lives in [`docs/plugins.md`](../../docs/plugins.md#the-plugin-api-contract).
**stable** follows [`STABILITY.md`](../../STABILITY.md)'s SemVer promise and a
break ships with a migration-guide *Plugin authors* section; **experimental**
(today: `AppBuilder::with_edge_kv`, `autumn_edge::host`) may change in any
release, patch included.

A plugin declares its range by implementing `Plugin::contract` — optional, and
`None` (the default) keeps today's behaviour exactly:

```rust
fn contract(&self) -> Option<PluginContract> {
    Some(
        PluginContract::new(env!("CARGO_PKG_NAME"))
            .plugin_version(env!("CARGO_PKG_VERSION"))
            .autumn_web("0.7")            // Cargo requirement: "0.7", ">=0.6, <0.9", "=0.7.1"
            .uses_experimental("AppBuilder::with_edge_kv"),  // only if you actually do
    )
}
```

Three things to know before advising on it:

- **A mismatch panics at registration**, not later, with a message naming both
  versions and both remedies (`cargo update -p <plugin>`, or pin
  `autumn-web = "<declared>"`). An unparseable requirement only warns at
  runtime — but `autumn plugin-check` fails on it, which is where the author
  sees it.
- **`autumn plugin-check` gained two checks, and one of them is a break for
  existing plugins.** `plugin-contract` **fails** when the plugin under check
  declares no usable range — the plugin still compiles and runs unchanged, but
  its CI goes red until `Plugin::contract` is implemented. `experimental-surface`
  *reports* what it declares, failing only on a name that does not resolve to
  an experimental entry in the registry. Both skip against a host binary built
  before the contract existed; `--deny-experimental` fails closed in that case
  rather than becoming a no-op. `autumn generate plugin` scaffolds the contract,
  so new plugins are green out of the box.
- **`--plugin-name` resolves against either identity.** A contract names the
  *crate*; route attribution keys on `Plugin::name()`, which defaults to
  `std::any::type_name`. The CLI matches both, so neither choice hides a plugin
  from the check.
- **A mismatch has an escape hatch.** The registration panic names
  `AUTUMN_PLUGIN_CONTRACT=warn`, which downgrades it to a `WARN`. Reach for it
  when a plugin's declared range is merely stale; the fix still belongs in the
  plugin.
- **The framework side is gated by compilation.** `autumn-plugin-reference`
  calls every declared stable surface and is built by the `plugin-contract` CI
  job, so a stable-surface break is a red check on the PR that causes it. Do
  not add a registry entry without a matching call site there —
  `scripts/check-plugin-surface.sh` fails on it, and on a `docs/plugins.md`
  table that has drifted from the registry.

## Installing a plugin — `autumn plugin add` (unreleased, issue #1606)

**Reach for this instead of hand-editing `Cargo.toml` and the builder chain.**
One command adds the dependency at a version compatible with the app's
`autumn-web`, mounts the plugin in the `autumn_web::app()` chain, and prints
the post-install steps (config keys, follow-up generators):

```bash
autumn plugin list                      # name, description, compatible version
autumn plugin list --json --offline     # machine-readable; skip the crates.io lookup
autumn plugin add autumn-admin-plugin   # dependency + mount + next steps
autumn plugin add autumn-cache-redis --dry-run
```

`list` covers the five first-party crates (`autumn-admin-plugin`,
`autumn-cache-redis`, `autumn-media-plugin`, `autumn-search`,
`autumn-storage-s3`) plus community crates found on crates.io under the
documented `autumn-plugin-<name>` convention.

Four behaviours worth knowing before advising on it:

- **Idempotent.** A second `add` reports "already installed" and changes
  nothing. It also detects a mount written by hand behind a `use` import, so
  it will not splice a second, default-constructed mount over a configured one.
- **Version-gated before any write.** Installing into an app on an
  incompatible `autumn-web` fails naming both versions, with the app
  byte-identical. First-party plugins ship in lockstep with `autumn-web` and
  the CLI, so the version installed is the CLI's — an app on an older line
  needs the matching CLI, or `autumn upgrade`.
- **It degrades rather than guessing.** If the `autumn_web::app()` chain
  cannot be found unambiguously inside `async fn main` — a builder factored
  into a helper, a one-line chain, two candidate lines — it writes *nothing*
  and prints the dependency line and mount snippet on stderr, exiting 2. It
  never leaves an app that does not compile.
- **Community crates get the dependency only.** The `<Name>Plugin` mount is
  derived from the naming convention and printed, not spliced: nothing outside
  that crate can verify it exposes one.

It writes no `features = [...]` onto the app's `autumn-web` dependency — each
plugin crate already carries the features its mount needs and Cargo unifies
them. `autumn plugin-check` is the separate, author-facing conformance gate;
`autumn generate plugin` scaffolds a new plugin crate.

## Removing a plugin, and installing one on day zero (unreleased, issue #1631)

The lifecycle runs both ways, and a plugin can be wired at scaffold time:

```bash
autumn new my-app --with autumn-admin-plugin --with autumn-search  # repeatable
autumn plugin remove autumn-admin-plugin
autumn plugin remove autumn-media-plugin --dry-run                 # every consequence, no writes
autumn plugin remove autumn-media-plugin --drop-data --yes         # destructive, opt-in
```

`remove` is the exact reverse of `add` and refuses in the same spirit:

- **It never touches the database.** A plugin that declares migrations or owns
  tables gets them listed with a statement that they are still there.
  `--drop-data` reverts them in one transaction, printing the statements and
  confirming *before any file is written*; a non-interactive stdin without
  `--yes` is a refusal, never an assumed yes.
- **It declines rather than guesses.** A mount it cannot read as a single
  builder call (built into a variable, sharing a line, or nested inside another
  plugin's constructor) changes nothing and prints the lines to delete, exit 2.
  So does a dependency not written as a plain `name = "…"` line.
- **It keeps a dependency the app still uses**, naming the file under `src/`,
  `tests/`, `benches/`, `examples/` or `build.rs` that kept it.
- **Idempotent**, like `add`: removing what is not installed says so and
  changes nothing.
- **Exit codes**: `0` removed or nothing to do, `1` refused, `2` nothing was
  changed automatically (apply the printed lines by hand), `3` `--dry-run`
  found something a real run would change.

`autumn new --with` resolves and version-checks every name *before* the
scaffold writes a byte, so a typo leaves no half-built project. `autumn doctor`
reports leftovers as `plugin_residue` — see the doctor skill.

## File storage and cache plugins

For local or pluggable file storage:

```toml
autumn-web = { version = "0.7", features = ["storage", "multipart"] }
autumn-storage-s3 = "0.7" # when storage.backend = "s3"
```

```rust
let store = autumn_storage_s3::S3BlobStore::from_config(&config.storage.s3)
    .await
    .expect("S3 store");
autumn_web::app().with_blob_store(store).run().await;
```

For keyword **and** vector search with an index that stays in sync:

```toml
autumn-search = "0.7"
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
autumn-web = { version = "0.7", features = ["redis"] }
autumn-cache-redis = "0.7"
```

```rust
autumn_web::app()
    .plugin(autumn_cache_redis::RedisCachePlugin::new())
    .run()
    .await;
```

**(0.6.0)** Cache stampede protection — do not hand-roll cache-aside
fills: `cache::get_or_compute(cache, key, Some(ttl), fill)` runs `fill` once
per process for concurrent callers; `get_or_compute_with` +
`GetOrComputeOptions::new().distributed_fill_lock(true)` (Redis lock) and
`.stale_while_revalidate(grace)` add cross-replica single-fill and
serve-stale; `cache::jittered_ttl(base, fraction)` de-synchronizes mass
expiry. A failed fill never poisons the key. See
`docs/guide/cache-stampede.md`.

**(0.6.0)** Upload content-type validation — multipart uploads are
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

## View helpers and widgets (0.6.0)

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
| `bulk_actions_form(&BulkActionsConfig, csrf_token, csrf_field, submit_token, submit_field, content)` / `bulk_select_checkbox(id, &cfg)` / `bulk_actions_toolbar(&cfg)` | No-JS bulk-select + "Delete selected" over a list (#1312): wrap the list in `bulk_actions_form` (a `POST` form carrying the hidden CSRF and one-time submit-token fields plus the submit button), and put one `bulk_select_checkbox` — `name="ids"`, `aria-label="Select row <id>"` — in each row's first cell. Keep page furniture ("New …" link, search box) *outside* the form. Always pass the submit-token pair on a destructive form: a tokenless request passes through `SubmitTokenLayer` unguarded, so a double-click would re-run the whole batch. `BulkActionsConfig::new(action)` + `.field_name(..)`/`.submit_label(..)`/`.select_label(..)`. The toolbar emits no confirmation prompt: inline `onclick` confirms are blocked by the default `script-src 'self'` CSP, and `confirm_action` submits its own form so it cannot carry the selection — to confirm a batch, post it to an interstitial page that lists the rows and asks for a second submit. `autumn generate scaffold` wires all of this automatically |
| `comment_thread(&cfg, &CommentView::from_thread(&nodes))` / `CommentThread::new(dom_id, action)` | The view half of `#[commentable]` (#1367): nested `<ol>` comment thread with a `<details>`-disclosed inline reply form on every node. Ordinary `<form method="post">` (works with scripting off) that also carries `hx-post`/`hx-target`/`hx-swap="outerHTML"` to swap the thread in place. Thread `.csrf_token(...)` and `.return_to(path)` for the no-JS round trip, `.max_depth(n)` so the UI never offers a reply the write path would `422`, `.read_only(Some(prompt))` for a signed-out visitor |
| `autumn_web::ui::WIDGETS_CSS` / `WIDGETS_CSS_PATH` | One shipped stylesheet backing every `autumn-*` widget class — link `href=(WIDGETS_CSS_PATH)` instead of copying widget CSS into `input.css`. Accent now follows `var(--primary)` (violet), not the old hardcoded indigo (`docs/guide/widget-styling.md`) |

### Whole-form rendering — `form_for` (0.6.0)

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
line items in one submit — use `autumn_web::nested_form` (0.6.0, #1915) instead of hand-wiring indexed field names: implement
`NestedChild` on the child new-model, render the child rows with
`inputs_for(&nested, &InputsForOptions, |row| …)` (add/remove rows + a
`destroy_checkbox` for deletes) off a `NestedChangesetForm<Parent, Child>`, and
decode the flat submission back into parent + children with
`decode_nested_urlencoded`, so the parent and its children validate and persist
as one transaction.

## Resumable SSE streams (0.6.0, issue #1356)

Don't hand-roll `Last-Event-ID` bookkeeping or a manual replay buffer for
server-sent events. Since 0.6.0 the `ws`-gated channels backend keeps a
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

### Process roles — web/worker split (0.6.0)

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

### Per-queue worker pools, pinning & `ProcessRole` on `AppState` (0.6.0)

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
  (an `ERROR` if it would claim nothing). `autumn doctor`'s
  `jobs_queue_coverage` check reports informationally when no fleet topology is
  declared; declare every tier's pin under `[jobs.fleet] tiers` and it hard-fails
  (exit 1, in every mode — not only `--strict`) on a queue drained by no tier
  (issues #1623, #1756). `autumn serve --pin <queues>` sets the pin as a flag,
  forwarding `AUTUMN_JOBS__PIN`, and `serve restart` restores it.
- **Per-queue actuator gauges** — `<actuator-prefix>/jobs` adds a `queues` key
  with per-queue `depth` and `oldest_waiting_age_ms` alongside the existing
  per-job-type gauges (per-process approximations on multi-process backends).
- **Backend-derived queue gauges (0.6.0, issue #1752)** — on
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

## Operator alerts (0.6.0, issue #1610)

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
  **(0.6.0, issue #1743)**.

Delivery is best-effort and off the request path (background tick every
`eval_interval_secs`, default 30), reuses your existing mailer + outbound-webhook
machinery, and the webhook is signed exactly like other Autumn webhooks
(`Autumn-Signature: t=…,v1=<hmac-sha256>`). `enabled = false` is the master off
switch (also silences custom channels). Add your own transport (PagerDuty,
Slack, …) by implementing `AlertChannel` and registering it with
`AppBuilder::with_alert_channel`. `autumn doctor` warns (in production) on a
missing or unusable destination — see the `doctor` skill. See
`docs/guide/operator-alerts.md`.

### Native transports (0.6.0, issue #1630)

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

## Supply chain — SBOM + provenance (unreleased, issue #1615)

Every autumn release asset carries a CycloneDX SBOM and a keyless SLSA
build-provenance attestation, and `autumn release init` makes the same posture
the default for a scaffolded app. Reach for this when asked "what is in this
build?", "where did this binary come from?", or for compliance/audit evidence.

- `autumn sbom` is deterministic on purpose — no wall-clock timestamp, and a
  `serialNumber` derived from the document's content — so `--verify` can be a
  real gate. Do not add a random serial or a timestamp.
- `autumn sbom --binary <path>` answers "which crate versions are in this
  binary?" with no source tree and no lockfile, reading the `.dep-v0` section
  `cargo-auditable` embeds. A binary built without it gets an error naming the
  fix, never an empty list.
- The generated production Dockerfile compiles through `cargo auditable`,
  fetches Tailwind via the checksum-verifying `autumn setup` (never a bare
  `curl`), and bakes an SBOM at `/usr/share/autumn/sbom.cdx.json` behind the
  `io.autumn.sbom.path` label.
- Verify a published artifact with
  `gh attestation verify <asset> --repo autumn-foundation/autumn`. Always pass
  `--repo`: without it any repository's attestation would satisfy the check.

See `docs/guide/supply-chain.md` for the end-to-end walkthrough, including the
negative case (tamper with a byte, watch verification fail).

## Dependency advisories — the audit gate (unreleased, issue #1600)

Every app `autumn new` generates audits its whole dependency tree on each push
and pull request: the scaffolded `.github/workflows/ci.yml` installs a pinned
cargo-deny and runs `cargo deny check advisories` against a `deny.toml` at the
project root. A known RustSec advisory that `deny.toml` does not waive fails
the build.

- **Never "fix" a failing audit by weakening the gate.** `continue-on-error`,
  `|| true`, and deleting the step turn a security control into a decoration —
  and an integration test in the framework fails if the generated workflow does
  any of them. If asked to get CI green, fix or waive the advisory instead.
- Prefer a real fix: `cargo update -p <crate>` when a patched version is
  compatible, else bump the direct dependency that pulls it in. Read the
  dependency path cargo-deny prints bottom-up — the crate to bump is the
  highest entry in that chain the app controls.
- Waive only when no fix exists ("no safe upgrade is available") or the
  vulnerable path is unreachable, by adding to `deny.toml`:
  `{ id = "RUSTSEC-…", reason = "why this is acceptable here; review-by <date>" }`.
  A waiver lets exactly one id through; the gate stays on and every other
  advisory still fails.
- A fresh scaffold ships one waiver already — RUSTSEC-2023-0071 (`rsa`, reached
  through the unconditional `jsonwebtoken` dependency, no patched release
  exists) — which is why day-one CI is green. Do not remove it without checking
  whether a fixed `rsa` has shipped. `--bundled-pg` apps get a second waiver
  (RUSTSEC-2024-0384, `instant`, via the embedded-Postgres build stack); other
  flavors deliberately do not, so an unused-waiver warning still means one of
  the app author's own waivers went stale.
- An app upgraded from a release before this gate existed receives the workflow
  but not `deny.toml` (the policy is the app's, so `autumn upgrade` never
  writes it). The audit step says so and stops; copy the file from a freshly
  generated app rather than removing the step.
- When the advisory database is unreachable the gate **fails closed**: the
  fetch is a separate step retried 3× with backoff, and the audit then runs
  `--offline` against it, so a failure in the audit step is always a real
  advisory rather than a network blip.
- The framework gates itself the same way in PR CI and the Publish Gate:
  `./scripts/check-advisories.sh` (and `--self-test`, which proves the gate can
  still reject an injected known-vulnerable dependency).

`docs/guide/supply-chain.md` "Part 3a" has the failure-reading walkthrough.

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

### Runtime log levels (0.6.0)

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

### Build & git provenance on `/actuator/info` (0.6.0)

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
  "autumn":  { "version": "0.7.0", "profile": "prod" },
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
0.6.0 — `Server-Timing` response header:

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

## Resilience: load shedding (0.6.0)

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

The ceiling no longer has to be a guess. `autumn calibrate` measures what the
build sustains and writes a committed `capacity.lock`; `autumn calibrate
--check` gates rebuilds against it in CI, and pointing the runtime at it
sources the ceiling from the proven envelope:

```toml
[server]
capacity_contract = "capacity.lock"   # explicit max_concurrent_requests still wins
```

Each route in the contract also carries a resource shape (`db-bound`,
`io-bound`, `compute-bound`) derived statically from the extractors its handler
declares. Every contract failure — missing file, malformed document, a contract
measured on a different host class — falls back to *unlimited*, never to a
ceiling. See `docs/guide/capacity-contracts.md`.

## Sharding (0.6.0)

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

## Per-tenant memory cells (0.6.0, issue #1766)

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

## Shadow (differential) deploys

Canary decides from **cohort metrics** over traffic the new build really serves.
Shadow decides from a **per-request diff** and serves nobody: Autumn mirrors
sampled `GET`/`HEAD` traffic to a candidate build you run alongside production
and compares the two responses. Use it to catch the subtly-wrong-but-`200`
regression — a dropped JSON field, a reordered list, an off-by-one total — that
cohort metrics cannot see. It composes with canary; it does not replace it.

Off by default. Requires the `http-client` feature (on by default).

```toml
# autumn.toml
[shadow]
enabled          = true
target           = "http://127.0.0.1:9091"  # the candidate build (you run it)
sample_rate      = 0.05    # of ELIGIBLE traffic. Default 1.0 — start low.
routes           = ["/api/*"]  # empty (default) = every eligible route
timeout_ms       = 2000    # bounds the shadow request AND the primary wait
max_in_flight    = 8       # excess mirrors are dropped, never queued
max_body_bytes   = 262144  # larger responses are not compared, either side
max_records      = 50      # divergences kept for the actuator
max_sample_bytes = 2048    # per recorded JSON sample, before truncation
```

Every key has an env override (`AUTUMN_SHADOW__ENABLED`,
`AUTUMN_SHADOW__TARGET`, `AUTUMN_SHADOW__SAMPLE_RATE`, …).

**What it guarantees.** The client never waits on the mirror (detached task; the
primary body is teed, not buffered) and never receives a candidate byte. The
candidate is dialed at `target` but sees the `Host` the live build accepted, and
pages served from an SSG/ISG static cache are mirrored too. Only
`GET`/`HEAD` are mirrored, and that is a constant, not a config key — replaying
a `POST` needs effect virtualization, which does not exist yet. Requests the
live build refuses (`429`/`503`) are not mirrored. Actuator and probe paths are
never mirrored. Every mirrored request carries `X-Autumn-Shadow: 1`, so
mirroring cannot recurse.

**What it compares.** Status class (`200` vs `201` is not a divergence; `200` vs
`500` is) and a normalized body: JSON object key order is normalized away,
**array order is not**. Headers, latency, and fuzzy JSON tolerance are out of
scope.

**Reading the results** — `{actuator-prefix}/shadow`, sensitive-gated like
`/actuator/tasks`:

```bash
curl -s localhost:3000/actuator/shadow | jq '.stats, .divergences[0]'
```

Plus two built-in metric families on `/actuator/prometheus`:

```
autumn_shadow_comparisons_total{version,route,outcome}  # match|diverged|error|
                                                        # timeout|skipped|dropped|
                                                        # refused|incomplete
autumn_shadow_divergences_total{version,route,kind}     # the series to alert on
```

**Tell the user before they enable it**: the candidate receives live cookies and
`Authorization` headers (a candidate that cannot authenticate makes every diff
noise), its own side effects are real so it must be contained by environment
(scratch database / writes disabled), and recorded samples are redacted by a
**key-name allowlist** (`[log] filter_parameters`) — not a PII classifier — so
any other *keyed* field of a response body is stored verbatim on that actuator
endpoint. (Bodies with unkeyed scalars record only a digest.)

**One configuration step is easy to miss**: the candidate honours the forwarded
client identity only if it trusts the mirroring host as a proxy. Add that host
to the *candidate's* `[security.trusted_proxies] ranges`, or accept that routes
reading `ClientAddr`/`ClientScheme` — and candidate-side per-IP rate limits —
will see the mirror rather than the real client.

See `docs/guide/staged-deploys.md`.

## Sandboxed plugins (0.7.0, issue #1609, feature `plugin-sandbox`)

Two plugin lanes now. The native `Plugin` trait is unchanged and full-trust: its
`build(self, app)` gets the whole `AppBuilder`. A **sandboxed** plugin is the
trade for code nobody audited — it ships as a `.autumn-plugin` artifact (a
`wasm32-wasip1` module plus a manifest) and the runtime enforces the manifest.

Non-default feature. Experimental: everything in `autumn_web::plugin_sandbox`
is outside SemVer (see `STABILITY.md`).

```toml
# plugin.toml — the whole review surface
name = "autumn-plugin-hello"
version = "0.1.0"
wire_version = 1
prefix = "/hello"
# `http-request` plus, since #1632: kv, http-outbound, db, jobs, render.
capabilities = ["http-request", "kv", "http-outbound", "db", "jobs", "render"]
sha256 = "0000…"                    # stamped by `autumn plugin package`

[grants]                  # what each capability is scoped to; BOTH ways enforced
hosts     = ["api.example.com"]   # http-outbound may call exactly these
tables    = ["orders"]            # db owns exactly these, tenant-scoped
job_types = ["reindex"]           # jobs may enqueue exactly these
slots     = ["order-summary"]     # render may fill exactly these

[quotas]                  # per request, operator-tunable; conservative defaults
kv_reads = 64
outbound_calls = 4

[[routes]]
method = "GET"
path = "/hello/{name}"

[limits]
fuel = 200_000_000        # CPU: instructions AND host-side bytes copied (64B/unit)
memory_bytes = 33_554_432 # per-request instance; footprint × concurrency ≤ 1 GiB
max_request_body_bytes = 1_048_576
max_response_bytes = 4_194_304
max_concurrency = 8       # requests shed with 503, never queued
request_body_timeout_ms = 5_000  # a dribbled body cannot pin a permit (408)
```

```bash
autumn plugin package --manifest plugin.toml --module hello.wasm --out hello.autumn-plugin
autumn plugin inspect hello.autumn-plugin            # the consent screen; exits 1 if unfit
autumn plugin inspect hello.autumn-plugin --format json
# Review an upgrade AS an upgrade: prints what the new manifest asks for that
# the approved one did not, and exits non-zero when authority grew (#1632).
autumn plugin inspect new.autumn-plugin --against installed.autumn-plugin
```

```rust
use std::sync::Arc;
use autumn_web::plugin_sandbox::{
    CapabilityServices, KvStore, MemoryKvStore, RenderSlots, SandboxedPlugin,
};

let hello = SandboxedPlugin::from_file(std::path::Path::new("plugins/hello.autumn-plugin"))?
    // Capabilities are honoured against backends YOU wire. Anything unwired is
    // answered `unavailable` and recorded — a refusal, never a silent success.
    // The `as Arc<dyn KvStore>` is load-bearing: unsizing does not pass through
    // `Option`, so `Some(MemoryKvStore::new())` will not coerce on its own.
    .with_services(CapabilityServices {
        kv: Some(MemoryKvStore::new() as Arc<dyn KvStore>),
        ..CapabilityServices::none()
    });

// Render slots need both halves: the manifest names the slots the plugin will
// fill, the app declares the slots that exist. `with` fails at boot on a
// mismatch; `render` returns a String and never breaks the page.
let slots = RenderSlots::declaring(["order-summary"]).with(Arc::new(hello.clone()))?;
let fragment: String = slots.render("order-summary", &[]).await;

// "What did this plugin do in the last hour" — one surface, shapes not values.
// `Display`: the summary already knows its plugin and its window.
let summary = hello.activity().summary("autumn-plugin-hello", std::time::Duration::from_secs(3600));
println!("{summary}");

autumn_web::app().plugin(hello).run().await;
```

**Capabilities are data, not code (#1632).** A plugin granted every capability
imports exactly what a plugin granted none imports — `fd_read` and `fd_write`.
The guest asks over the NDJSON channel it already answers requests on:
`{"op":"call","call":"kv-get","id":1,"key":"cart"}` in, `{"op":"call_result",
"status":"ok"|"denied",…}` back, then its `response` frame. A refusal is a frame
the guest can read (`capability-not-granted`, `not-in-grant`, `quota-exceeded`,
`malformed`, `unavailable`, `backend-error`), never a trap — so a plugin over a
ceiling degrades instead of 502ing.

Scoping is **derivation, not checking**: the guest names a logical key, table,
host or job type and the host derives `plugin-kv:<plugin>:<tenant>:<key>` /
`plugin_<hex-escaped plugin>__<table>` from the manifest and the active tenant
(the escape is injective, so a plugin *named* `shop_orders` owning `v2` cannot
land on the table `shop` owning `orders_v2` already has). There is no
field in the protocol where another tenant, another plugin, a host-app table or
SQL would go, so cross-tenant access is unspellable rather than refused. Render
hooks return a fragment **tree** the host renders (no HTML parser, so no parser
differential; nothing needs `unsafe-inline`), and a hook that traps or overruns
`render_bytes` omits the fragment rather than breaking the page.

**What the runtime enforces.** The guest's entire authority is the WASI shim's
import list: no filesystem, no network, no environment, no database, each
attempt answered `ENOTCAPABLE`/`EBADF` and logged as a denial. An import the
shim does not define is refused at load. The manifest's `[[routes]]` *are* the
router, so an undeclared path under the prefix is a 404 the guest never sees.
Fuel bounds CPU (including host-side copying done on the guest's behalf), a
store limiter bounds memory, both per request against a fresh instance; the
interpreter runs on a blocking worker with the concurrency permit held for its
actual lifetime. A trap, `proc_exit`, blown budget, malformed frame or missing
answer is a 502/503/504 **on the plugin's own prefix** — nothing reaches the
rest of the app.

**Boundary gotchas worth knowing before authoring one.**
- Request headers are an **allowlist** (`accept*`, `content-type`,
  `content-length`, `cache-control`, `if-*`, `range`, `user-agent`). Everything
  else is dropped — a denylist could never name every proxy's identity header.
- Response headers are a **closed allowlist** (`content-type`, `cache-control`,
  `etag`, `location`, `vary`, …) with **no** `x-` hatch: `X-Accel-Redirect` /
  `X-Sendfile` would borrow the reverse proxy's filesystem. `set-cookie`,
  `strict-transport-security`, framing headers and the host's own
  `x-autumn-sandboxed` / `x-content-type-options` are stripped and logged.
- A declared `GET` also serves `HEAD` (HTTP requires it, axum dispatches it);
  the guest sees `method: "HEAD"` and `route_infos()` reports the extra row —
  unless the manifest declares HEAD itself, in which case that route mounts
  alone (two overlapping method routes on one path is an axum panic at boot).
- Load refuses a module whose WASI import *signatures* disagree with the shim,
  whose `_start` is not `() -> ()`, which exports no `memory` (or one already
  over the ceiling), or which carries >4096 / >16 MiB of data and element
  segments (every request re-instantiates it).
- Response **content types** are an allowlist too: `text/plain`, `text/csv`,
  `application/json`, `application/octet-stream`, and raster images. HTML,
  SVG, JavaScript and CSS are refused — a document or script from your own
  origin carries your origin's authority.
- `SystemTime::now()` is a fixed instant; `random_get` is deterministic, seeded
  from the request. Neither is entropy.
- One frame in, one frame out, NDJSON over stdio — except a `call` frame, which
  suspends rather than ends the exchange (the answer arrives on stdin). A frame
  must end with `\n` (`println!`, not `print!`) or the host reports a partial
  frame; a guest that writes calls without reading the answers is stopped.
- A `[grants]` list and its capability must agree **both** ways: a `hosts` list
  without `http-outbound` is refused, and so is `db` with no `tables`.
- Outbound hosts are matched by exact equality — no suffix, no wildcard — and a
  URL with userinfo, a non-http(s) scheme or an IP literal is refused outright.
  Redirects are not followed on the plugin's behalf.
- A plugin row may not carry `tenant_id` (it would choose its own tenant); a
  `row_id` echoed back from `db-get` is stripped, so read-modify-write works.

Guide: `docs/guide/sandboxed-plugins.md`. Trust model: `docs/plugins.md`.


## CLI

```bash
cargo install autumn-cli --version 0.7.0

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
autumn generate webhook stripe Payments
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
autumn calibrate                 # measure the capacity envelope -> capacity.lock
autumn calibrate --check         # CI gate: fail when a rebuild leaves the envelope
autumn plugin-check --plugin-name autumn-admin-plugin --prefix /admin
autumn plugin list
autumn plugin add autumn-admin-plugin
```

**(0.6.0)** CLI additions — absent from a 0.5.x `autumn-cli`, but present in
every published release since 0.6.0, so they are safe to suggest unless the
user pins 0.5.x:

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
autumn db backup --upload --keep 7   # + upload each verified run offsite (S3/MinIO/R2); db offsite list; db restore offsite:<profile>/latest  # (0.6.0)
autumn db replica restore --force        # SQLite tier: rebuild from the continuous replica; --timestamp <RFC3339> for PITR, --overwrite to replace an existing file  # (0.7.0)
autumn db replica status --json          # replica generation, segments, and current replication lag; db replica verify proves it restorable  # (0.7.0)
autumn db scrub --artifact backups/prod/<run> --force   # restore a prod backup into staging, then anonymize every PII column; --check for CI
autumn db retention --dry-run    # per-dataset retention window, its source, and rows eligible for purge; --purge to enforce now (needs --force outside dev/test), --dataset X, --json  # (0.7.0)
autumn seed --count 50 --model Post  # generate+insert 50 faked rows via the model's factory (both flags together)
autumn serve --role worker       # run only workers + scheduler (web/worker split); also --role web|combined
autumn console                   # data playground: scaffolds src/bin/playground.rs (pre-wired config+pool), then builds and runs it; alias `autumn c`
autumn console --force           # regenerate the playground from the template (never overwritten otherwise)
autumn console --scaffold-only   # scaffold + wire Cargo.toml, then stop
autumn release init --target azure-container-apps   # Terraform scaffold: main.tf/variables.tf/outputs.tf/terraform.tfvars.example (ACR, Container Apps, Postgres Flexible Server, Key Vault-backed secrets, opt-in Redis) + .github/workflows/azure-deploy.yml (#1278). Same --force/collision guard as the fly/docker-compose targets; see docs/guide/deployment.md.
autumn release init --target aws-app-runner      # Fast/minimal AWS path: main.tf/variables.tf/outputs.tf/terraform.tfvars.example (ECR, App Runner behind a VPC connector, RDS Postgres, Secrets Manager). No CI workflow (#1279); see docs/guide/deployment.md.
autumn release init --target aws-ecs             # Production AWS path: main.tf/variables.tf/outputs.tf/terraform.tfvars.example (VPC, ALB+ACM DNS-validated HTTPS, ECS Fargate w/ circuit-breaker rollback, Application Auto Scaling, RDS, opt-in Redis) + .github/workflows/aws-deploy.yml (#1279); see docs/guide/deployment.md.
autumn release init --target gcp-cloud-run       # GCP path: main.tf/variables.tf/outputs.tf/terraform.tfvars.example (Artifact Registry, Cloud Run, Cloud SQL Postgres behind a VPC connector, Secret Manager, opt-in Memorystore Redis) + .github/workflows/gcp-deploy.yml (#1280); see docs/guide/deployment.md.
autumn sbom                      # CycloneDX 1.5 SBOM for this source tree, to stdout (deterministic: no timestamp, content-derived serialNumber) (unreleased, issue #1615)
autumn sbom --output sbom.cdx.json --locked        # write it; --locked fails when Cargo.lock disagrees with the manifests
autumn sbom --verify sbom.cdx.json --expect-version 0.8.0   # regenerate + compare component-by-component, and pin the root version; exit 1 with a named diff on drift
autumn sbom --binary /usr/local/bin/my-app         # crate versions compiled INTO a binary (cargo-auditable `.dep-v0`; ELF/Mach-O/PE) — no source tree, no lockfile
autumn build --auditable         # compile through `cargo auditable` so the binary carries its own dependency list (the generated release Dockerfile passes this) (unreleased, issue #1615)
autumn upgrade                   # preview BOTH halves as per-file diffs, writing nothing: each release's mechanical app-code migrations (renames), and drift between the project's framework-owned files and this release's scaffold (#1629, #1593)
autumn upgrade --apply           # take them; --from/--to override the codemod range, --list-migrations shows what ships
autumn upgrade --check           # scaffold files only, writes nothing, exit 3 on drift — the CI gate for scaffold freshness (#1593)
autumn upgrade --accept <PATH>   # record a framework-owned file as the developer's own: never offered or written again, and not drift (#1593)
autumn deploy check              # SSH/secret/DB/migrate-safety preflight per configured host; `doctor --online` runs the same graders
autumn deploy plan               # dry-run: representative unit + ordered steps (+ fleet rollout order when `[deploy] hosts` is set)
autumn deploy up                 # real deploy over SSH; with `[deploy] hosts` a serial rolling deploy across the fleet (#1621)
autumn deploy up --only web-2 --no-rollback   # narrow to a subset (repair lever, warns about a mixed fleet) / halt-and-freeze on failure
autumn deploy rollback           # previous release; with `[deploy] hosts` the whole fleet, newest first (`--only <HOST>` for one)
autumn deploy status --json --strict          # read-only per-host state + version/state drift; --strict exits non-zero on drift (#1621)
autumn deploy maintenance on --message "…"    # maintenance mode on EVERY deploy host over SSH; `off` reverses (#1621)
autumn plugin list               # installable plugins + the version compatible with this app's autumn-web; --json, --offline (#1606)
autumn plugin add autumn-admin-plugin   # dependency + builder-chain mount + post-install steps; idempotent, version-gated, --dry-run (#1606)
autumn plugin remove autumn-admin-plugin   # reverses both wires; never touches the database (--drop-data does, with confirmation); --dry-run exits 3 when it would change something (#1631)
autumn new my-app --with autumn-admin-plugin   # scaffold with plugins already wired; repeatable, resolved and version-checked before any file is written (#1631)
autumn data-flow                 # classified-data flow manifest: one row per `#[classified]` column and every sink a declared declassification boundary releases it to; empty reachable set = the column cannot leave the process (#1654)
autumn data-flow --manifest data-flow-manifest.json --check data-flow-manifest.json   # write it, and fail on drift from the committed copy so a new release edge is reviewed
autumn data-flow --release --check data-flow-manifest.json   # audit the profile you deploy: a boundary behind `#[cfg(not(debug_assertions))]` exists only in the release build, so a debug-built manifest would certify edges the shipped binary does not have (and miss the ones it does)
autumn agents manifest           # agent authority manifest: one row per `#[agent_operable]` action with its grant, proved effects (writes, unbounded writes, cross-tenant reach, outbound, webhooks, jobs), provenance, and unused grant entries; plus every MCP-exposed tool with no envelope (#1691)
autumn agents manifest --manifest agent-authority.json --check agent-authority.json   # write it, and fail on drift, on any ungoverned mutating MCP tool (`--allow-ungoverned`), or on an unaudited deployment that can act irreversibly (`--allow-unaudited`)
autumn graph show                # the application architecture graph the macros declare: a node per `#[route]`/`#[static_get]`, `#[model]`, `#[repository]` and `#[job]`/`#[scheduled]`/`#[task]`, each route with its mounted path and declared auth requirement, plus edges for repository→model and every model/table a route or job names (#1747)
autumn graph touches posts       # which routes and jobs reach a model, table, or repository — transitively, so a handler that only takes `PgPostRepository` is included
autumn graph impact Post         # what a change to a model would affect: the repositories over it, and every route and job reaching it directly or through one
autumn graph show --manifest architecture-graph.json --check architecture-graph.json   # write it, and fail on drift, naming the node, edge or auth posture that moved
autumn graph impact Post --json  # every verb honours --json; `show --json` emits the whole document
```

Reach for `autumn graph` before reading a codebase to answer a structural
question. It is derived from the macros at compile time and embedded in the
binary, so it cannot drift from the code: `/actuator/graph` serves the same
document from a running app (sensitive-gated, like `/actuator/env`). Edges from
a route or job are read off that item's own tokens and are deliberately
over-reported; the document carries its own `limits` section, and the biggest
one is that a call into a helper in another module is not followed. See
`docs/guide/architecture-graph.md`.

### Upgrading an app across releases — `autumn upgrade` (0.7.0, issues #1629 and #1593)

**Reach for this before hand-editing call sites out of a migration guide, or
hand-diffing a throwaway `autumn new` project.** One run covers both halves of
an upgrade:

1. **The app's own Rust source.** For each release between the `autumn-web`
   version `Cargo.toml` records and the target, it applies that release's
   *mechanical* migrations — API renames — to `src/`, `tests/`, `examples/`,
   `benches/` and every workspace member. First shipped: 0.6.0's `with_pool`
   → `with_pool_untracked`.
2. **The project's framework-owned files.** `Dockerfile`, `.dockerignore`,
   `build.rs`, `autumn.toml`, `.gitignore`, `.env.example`,
   `.github/workflows/ci.yml`, `rust-toolchain.toml`, `rustfmt.toml`,
   `clippy.toml`, and (fullstack only) `tailwind.config.js` +
   `static/css/input.css`, reconciled against this release's scaffold.
   Bumping `autumn-web` updates the library, not the project skeleton, so
   without this an app scaffolded on 0.5 keeps 0.5-vintage project files
   forever.

Three things to get right when advising on it:

- **Run it before bumping the dependency.** The release it migrates *from* is
  the one the manifest still records, so bumping `autumn-web` first leaves
  nothing in range and the command reports "nothing to change". If the bump
  already happened, pass `--from <previous-version>`.
- **Preview is the default.** A bare `autumn upgrade` prints a per-file diff
  plus a count of affected sites and writes nothing; `--apply` is the write
  step. Tell users to commit or stash first — `git diff` is how they check
  its work.
- **It reports what it will not touch.** A call site inside a macro
  invocation, a `macro_rules!` body, or an attribute, and a reference that is
  never called (`.map(Repo::with_pool)`), are listed with `file:line` and a
  guide link under `manual` — those are the ones a human (or you) still has to
  edit. `--json` gives the same report for scripting.

Every breaking change in `docs/migrations/*.md` carries an `**Automation:**`
label — `auto` (the codemod does it), `review` (it rewrites, and flags each
site), or `manual` (read the guide section). See `docs/guide/upgrading.md`.

For the scaffold half, five more things (issue #1593):

- **`src/` is out of bounds.** The set is a fixed allowlist with no `src/`
  entry, and `Cargo.toml`, `README.md`, `tests/`, `migrations/`, `i18n/`,
  `config/credentials/` and the vendored `static/js/` assets are not
  framework-owned either. Application code is the *first* half's business.
- **`.autumn/scaffold.toml` is the baseline, and must be committed.**
  `autumn new` records the scaffolding release, the flags the project was made
  with, and a digest of every framework-owned file as Autumn wrote it. That
  digest is the only thing that can distinguish "the template moved" from "the
  developer edited this". Verdicts: `add` (this release's scaffold has it and
  the project does not — including files a later release introduced),
  `update` (template moved, the copy is provably untouched), `conflict`
  (never written), `removed` (deleted on purpose, never restored), `pinned`
  (accepted as the developer's).
- **A project with no manifest still upgrades, best-effort.** Every app
  predating this feature is in that state: missing files are still offered,
  and everything that differs is a conflict, because "untouched" cannot be
  proven. Never an error.
- **A conflict has to be able to conclude.** Where a file is deliberately the
  team's — a `Dockerfile` with their own base image — `autumn upgrade --accept
  <path>` records it in the manifest's `pinned` list so `--check` can go green
  again. Removing a line from that list brings the file back.
- **Inside a Cargo workspace, member crates are not offered `clippy.toml`,
  `rustfmt.toml`, `rust-toolchain.toml` or a CI workflow.** Those resolve from
  the nearest ancestor, so a crate-local copy *shadows* the workspace's rather
  than adding to it, and GitHub never runs a workflow from a subdirectory.

`--to` selects which codemods run; the scaffold half always reconciles to the
release the CLI itself ships templates for — downgrades and historical
scaffolds are out of scope. `--json` carries the scaffold report under a
`scaffold` key in both modes, splitting `writable` (the plan) from `written`
(what reached disk).

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

### `autumn test` — isolated test DB (0.6.0, issue #1056)

`autumn test` resolves the test DB URL with the same precedence as
`autumn migrate` (`AUTUMN_DATABASE__PRIMARY_URL` → `AUTUMN_DATABASE__URL` →
`DATABASE_URL` → autumn.toml — env wins over TOML), derives a
`_test`-suffixed name, creates it if missing, runs all pending app + framework
migrations, exports `AUTUMN_ENV=test` + the resolved `DATABASE_URL`, then runs
`cargo test` (exiting with its code). `--reset` drops and recreates the test DB
first; trailing `-- <args>` forward to the harness. It refuses to run against a
non-`_test` database.

### `autumn db backup` / `db restore` (0.6.0, issue #1595)

`autumn db backup [--dir DIR] [--format custom|plain] [--keep N] [--shard NAME]
[--control-only]` dumps the control DB and every shard to
`<dir>/<profile>/<timestamp>/` (default `./backups`) with a `manifest.json`,
integrity-checks each artifact with `pg_restore --list` before reporting success
(a partial artifact is removed and the command exits non-zero), and `--keep N`
prunes to the newest N runs. `autumn db restore <ARTIFACT> [--shard NAME]
[--force]` verifies every artifact before touching a database and is gated by the
same production guard as `db drop` (refuses non-dev/test without `--force`). See
`docs/guide/deployment.md`.

### `autumn db scrub` (issue #1602)

`autumn db scrub [--artifact ARTIFACT] [--output DIR] [--config PATH] [--check]
[--dry-run] [--force]` turns a production database — or an `autumn db backup`
artifact restored into the resolved target — into an anonymized copy safe for
staging/dev. It rewrites every PII-classified column with deterministic,
constraint-valid fake values, resolving the target(s) exactly like `db backup`
(control plus every shard) and refusing non-dev/test profiles without `--force`
(the same guard as `db drop`).

Classification is **fail-closed and schema-driven** — the column universe comes
from introspecting the live database, not from a config file — in precedence
order:

1. `[tables.<t>.pii]` in `scrub.toml` (the explicit declaration + strategy);
2. `#[encrypted]` model columns (PII by construction — a `safe` declaration may
   NOT override one);
3. tables registered with the GDPR anonymize strategy
   (`ModelRegistration::anonymize("t")`, read statically) — every non-key column
   of that table, narrowable with `safe`.

Anything left over aborts the scrub, listing the columns and printing a
paste-ready `scrub.toml` stanza. So a newly added column can never silently
carry real data into staging.

```toml
# scrub.toml
[defaults]
safe_columns = ["id", "created_at", "updated_at"]   # safe in EVERY table

[tables.users]
safe = ["role", "locale"]          # reviewed, kept verbatim

[tables.users.pii]
email = "email"                    # scrubbed+<token>@example.invalid
full_name = "name"
phone = "phone"
bio = "redact"

[framework]
# `autumn_*` tables are outside the classified universe; empty the ones whose
# rows carry app payloads (opt-in; framework-owned names only).
purge = ["api_tokens", "autumn_jobs", "autumn_sync_rows"]
```

Strategies: `auto` (derived from the column type), `email`, `name`, `phone`,
`redact`, `null`, `uuid`, `bytes`, `json`, `zero`, `epoch`. Each value derives
from a hash over the row's primary key salted with the column name (a doubled
`md5` for a column that must stay unique), so a `UNIQUE` column stays unique, two columns
of one row never collide, and a re-run is idempotent.

`#[encrypted]` columns are **re-encrypted** under the target's own key (same
deterministic/randomized mode as the model), never overwritten with a plain
string — that would make every later read of the row fail as malformed
ciphertext — so the scrub needs the target's `active_record_encryption`
credentials and refuses before writing if they are missing.

`NULL`s stay `NULL`. PII is refused on a primary key, on **either side** of any
foreign key (composite components included), on `CHECK`-constrained columns, and
on generated columns; `null` is refused on `NOT NULL` and under `NULLS NOT
DISTINCT`; a constant-valued strategy is refused on any column in a unique index
(partial, composite, and expression-index inputs); a `varchar(n)` bound narrows the token or
refuses. Statements are `public`-qualified with a pinned `search_path` and the
planned tables are locked; non-`public` schemas, tables the role
cannot see, and RLS-enabled tables are refused rather than silently
under-scrubbed; materialized views are refreshed in
dependency order. Every target is classified before any is written, and each
database's statements run in one transaction.

`--check` classifies and writes nothing, exiting non-zero on any unclassified
column — run it in CI after the migrate step. `--dry-run` prints the exact SQL.
`--output DIR` re-dumps the scrubbed database as a fresh backup run. See
`docs/guide/data-scrubbing.md`.

### `[retention]` + `autumn db retention` (0.7.0, issue #1605)

Autumn's *own* tables and stores grow forever by default. `[retention]` in
`autumn.toml` declares one window per framework-owned dataset and the framework
enforces it on a recurring, fleet-coordinated in-process sweep — no external
cron. **This is separate from `#[repository(..., retention(...))]`, which covers
*your* models**; reach for that one for app data and this one for Autumn's.

```toml
[retention]
sweep_interval         = "1h"    # default
job_history            = "90d"   # terminal rows in autumn_jobs
commit_hooks           = "30d"   # terminal autumn_repository_commit_hooks rows
job_tracking           = "7d"    # autumn_job_tracking
idempotency            = "2d"    # stored Idempotency-Key responses
experiment_assignments = "365d"  # autumn_experiment_assignments
webhook_replay         = "3d"    # inbound replay markers
sessions               = "30d"   # server-side session records
audit_archives         = "400d"  # JSONL audit archive entries
```

Every window is unset by default, and unset registers **no sweep task at
all** — an app that never writes `[retention]` behaves exactly as before.
Each key has an `AUTUMN_RETENTION__*` env override; an empty value clears one.

Three enforcement mechanisms, reported per dataset rather than conflated:
`sweep` (batched `DELETE`, 500 rows/batch, cutoff resolved by the database's
own clock, `truncated` reported when a run hits its per-run cap), `backend ttl`
(the window *caps* the record's TTL at write time — idempotency, sessions), and
`archive rewrite` (the JSONL audit archive rewritten atomically, keeping any
line it cannot parse).

**Precedence:** where a per-subsystem knob already exists
(`jobs.tracking.ttl_secs`, `idempotency.ttl_secs`, `session.max_age_secs`, a
webhook endpoint's `replay_window_secs`) the **shorter bound wins**. Those knobs
keep their exact meaning; adding `[retention]` can never make data live longer
than it does today. `retention.webhook_replay` shorter than a protected
endpoint's `replay_window_secs` **fails boot** rather than weakening a security
control through a compliance knob.

**Safety rails the sweep enforces:** `job_history` matches only terminal rows
(`completed`/`failed`/`discarded`) with a `finished_at`, and never one still
holding a `#[job(unique, unique_for_ms = N)]` dedup key (deleting it would run
the job twice); `experiment_assignments` only sweeps concluded/archived/orphaned
experiments, never a running one's sticky assignments. Data whose table is
registered under a GDPR legal hold (`ModelRegistration::retain`) is never
removed — the hold vetoes the whole dataset. Every real sweep writes an audit
record (`action = "retention.sweep"`) carrying the dataset, cutoff and rows
removed.

```bash
autumn db retention                       # effective policy + rows eligible now
autumn db retention --dry-run             # what a sweep would remove
autumn db retention --purge --dataset job_history   # enforce now (--force outside dev/test)
autumn db retention --json                # machine-readable, for CI/compliance
```

It compiles and runs the app binary, so the report reflects the app's own
resolved config, GDPR registrations and audit sinks — report and enforcement
share one code path. See `docs/guide/data-retention.md`.

### Offsite S3 backups (0.6.0, issue #1619)

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

### Continuous SQLite replication + point-in-time restore (0.7.0, issue #1628)

On the zero-ops **SQLite** tier (#1614), `[replication]` ships the write-ahead
log to an offsite destination continuously **from inside the running process** —
no sidecar, no external tools. The contract is *at most `rpo_secs` (default 10)
of committed writes lost* when the machine dies, versus one whole backup interval
with snapshots alone. It composes with `autumn db backup`; it does not replace it.
Postgres targets are refused at boot (use WAL-G / pgBackRest there).

```toml
[replication]
enabled = true
rpo_secs = 10               # the contract; the loop ships every rpo_secs / 2
snapshot_interval_secs = 3600   # how often a fresh base snapshot is taken
max_wal_bytes = 16777216    # WAL size that forces a checkpoint (next WAL index)
retention_hours = 168       # how far back a point-in-time restore can reach
verify_interval_secs = 21600    # 0 disables the periodic restore verification
# path = "/mnt/backup-disk/replica"   # a directory destination instead of S3

[replication.s3]
bucket   = "myapp-replicas"
region   = "auto"
endpoint = "https://<accountid>.r2.cloudflarestorage.com"
force_path_style = true
access_key_id_env     = "AUTUMN_REPLICA_ACCESS_KEY_ID"    # names, not values
secret_access_key_env = "AUTUMN_REPLICA_SECRET_ACCESS_KEY"
```

Same destination conventions as `[backup.offsite]` (#1619): own section, profile
overlays, an `AUTUMN_REPLICATION__*` override for every key, env-var-indirected
credentials, and a refusal to share the app's `[storage.s3]` bucket **and**
endpoint without `allow_shared_bucket = true`.

Recovery on a fresh box that has only the binary, `autumn.toml` and the
credentials:

```bash
autumn db replica status --json   # generation, segments, current-as-of, lag
autumn db replica restore --force # latest state (--force clears the prod guard)
autumn db replica restore --timestamp 2026-09-02T14:29:00Z --force --overwrite
autumn db replica verify          # prove the replica restorable, touching nothing
```

`--force` gates the **profile**; `--overwrite` gates **replacing an existing
database file** — deliberately separate, so a drill that always passes `--force`
cannot destroy a live database. Restore refuses rather than best-efforts: a hole
in the segment sequence, a digest mismatch, a discontinuous segment, a generation
without its commit marker, or a rebuilt database that fails `PRAGMA
integrity_check` is an error, and nothing is published until every check passes.
A `--timestamp` outside the retention window is refused with the oldest reachable
instant named, never silently rounded.

Operationally: enabling replication makes the replicator the **only** component
that checkpoints (pooled connections get `PRAGMA wal_autocheckpoint = 0`), so an
unreachable destination stalls checkpointing and grows the `-wal` — it costs disk,
never data — and the tier's single-host/single-writer contract stops being
advisory. Lag, the current generation and the last successful verification appear
on `/actuator/health` under the `sqlite-replication` indicator; the indicator goes
`DOWN` past three RPOs of lag or on a failed verification, which the existing
`[alerts]` pipeline escalates (see `AlertCondition::HealthIndicatorDown`).
Verification is a **real restore** on an interval, not a checksum. See
`docs/guide/sqlite-in-production.md`.

### VPS deploys and fleets — `autumn deploy` (0.6.0; fleets 0.7.0, issues #1607/#1621)

`autumn deploy {check | plan | up | rollback | status | maintenance on|off}`
takes a project to a live, zero-downtime service on Linux servers the user owns
— no Dockerfile, no registry, no PaaS. Configure the target in `autumn.toml`:

```toml
[deploy]
host = "203.0.113.10"                            # ONE server
# hosts = ["10.0.0.1", "10.0.0.2", "10.0.0.3"]   # …or a FLEET (#1621)
```

The only target-host precondition is **key-based SSH as a root-equivalent user**
(#1607). `deploy up` PREPARES the host: it probes read-only for a working
kamal-proxy and, on a host that has none, installs the digest-pinned build at
`/usr/local/bin/kamal-proxy` — which also installs, and leaves behind, any of
`curl` and a container runtime the host lacks (kamal-proxy ships only as an
image). A host that already has a working proxy is untouched; one whose proxy
responds but has drifted is never replaced (hard refusal). Preparation is
Debian/Ubuntu-only and needs outbound HTTPS. Tell users to set `[deploy]
install_proxy = false` (or `AUTUMN_DEPLOY__INSTALL_PROXY=false`) if they
provision the proxy themselves or are on another distro. In a fleet it runs as
the first op of each host's own turn, never during the all-hosts probe phase.

`host` and `hosts` are **mutually exclusive** (both set = refused), blank and
duplicate entries are refused, and **the `hosts` order IS the rollout order**.
`AUTUMN_DEPLOY__HOSTS` is the CSV env override and replaces the whole list. Env
wins over TOML for BOTH spellings: a non-empty `AUTUMN_DEPLOY__HOSTS` clears a
TOML `[deploy] host` (and vice versa), so an env retarget never produces the
mutually-exclusive refusal. Setting BOTH env spellings non-empty is still refused
(ambiguous rollout order — an operator error, not a precedence question), and an
empty/blank value still means *unset*, leaving the TOML spelling alone.

With `hosts`, `deploy up` rolls the fleet **one host at a time** in declaration
order — each host runs the unchanged per-host blue/green cutover against its own
kamal-proxy and must finish before the next starts, so the rest of the fleet
keeps serving. One release id per run. **Migrations run exactly once**, on the
**first host in rollout order** whatever its mode, before its cutover — so the
schema always moves before ANY host takes traffic on the new release. A first
deploy migrates too (#1607), so a brand-new fleet migrates on host 1 rather than
migrating nowhere: there is no out-of-band `autumn migrate` step to tell users
about, and rollout order is no longer load-bearing for migration safety. A
failure **halts**
the rollout and (by default) rolls the already-cut-over hosts back in reverse
order — except post-cutover housekeeping failures (`record-proxy-options`,
`drain-old`, `prune`), which leave the host live and healthy so the rollout warns
and continues, and hosts whose rollback target is unprovable, which are reported
for manual recovery rather than guessed at. **Rollback restores binaries only —
no migration is ever rolled back**, so tell users to write expand/contract
migrations. The closing `Fleet state:` summary says which of THREE things is
true, gated on the migration having been reached (never on whether the fleet
compensated): a host still on the new release → `the schema has moved …`; the
fleet actually restored a host or removed its just-completed first deploy → `the
compensating rollback restored BINARIES only …` (a compensation that only FAILED
leaves that host forward, so it takes the first note, and both can print
together); nothing forward and nothing compensated, because the migrating host
failed after `migrate` but before its cutover and tore its own candidate down →
`no host is serving the new release, but the migration that already ran was NOT
rolled back …`. A rollout that died BEFORE its migration (a failed host
preparation or upload) prints none of them. **A failed SINGLE-host deploy prints
no summary and so warns about none of this** (known gap, #2276) — if a user's
one-host `deploy up` failed, tell them to check `autumn migrate status` before
assuming nothing was applied. That now includes a failed FIRST deploy, which
migrates before it starts the release.
`--only <HOST>` (repeatable, `up` and `rollback`) is a repair lever
that warns about a mixed fleet; `--no-rollback` halts and freezes instead.
`--only` narrowed to ONE host takes the single-host path: `deploy rollback --only
<host>` prints no fleet state table and keeps the HARD preflight gate (a
multi-host rollback downgrades a per-host `ssh_reachability` failure to a
reported row and continues; a one-target run does not). Only the selected hosts
are reachability-graded, but the topology refusals below key on the *configured*
host count, so `--only` never unlocks them.

No host is ever **drained** by a rollout: `/ready` never goes 503 for the
rollout's sake and no host leaves the LB pool — each host is replaced in place
(candidate on the idle loopback slot, `/ready`-gated there, atomic kamal-proxy
flip, old slot drained after). Never describe a fleet rollout as draining hosts.

`[jobs] backend = "local"` and `[scheduler] backend = "in_process"` are the
per-process defaults; on a fleet each host runs its own copy (work never
balances, queued work dies with the old slot, `unique`/`concurrency` stop being
fleet-wide, scheduled tasks fire once PER HOST). `deploy up` prints a loud ⚠️
naming the key(s) in effect whenever more than one host is configured — a
warning, never a refusal, and nothing else says it (`deploy check`, `deploy plan`,
`deploy status` and `autumn doctor` are all silent). Tell users to move to
`postgres`/`redis` before relying on background work across a fleet.

Fleet-unsafe topologies fail closed in the prologue, before any remote command:
`sqlite://` databases (every host gets the same URL → N independent files),
`[media.mediamtx] enabled = true` (no teardown path), and `[deploy.tls] enabled
= true` (each host would ACME the same hostname from behind the LB — terminate
TLS at the load balancer instead). `[database] auto_migrate` on a fleet is a loud
warning, not a refusal.

`autumn deploy status [--json] [--strict]` is read-only and safe mid-incident:
one row per host (mode, release from the `current` symlink, live slot, `/ready`
code, maintenance flag, proxy port, last deploy result, drift reasons) plus
`version_drift` (hosts on different releases) and `state_drift` (per-host marker
damage that fails the NEXT deploy closed). An unreachable host is a row, not an
abort; an unreadable release is never **version** drift — but a REACHABLE host
with a `current` symlink that resolves to no readable release IS state drift and
exits non-zero under `--strict`.
`last deploy` is the last action that host COMPLETED (`deployed` / `rolled back`
/ `torn down` + the host's UTC time, `?` when unreadable) — a deploy that failed before cutover
never rewrites it, so it is never a verdict on the last rollout, and it is
reported, not drift.

The **maintenance cell is three-valued** — `maintenance ON` / `maintenance off` /
`maintenance ?` — and reports the flag file the host's RUNNING slot unit polls,
resolved on the host from that unit's `Environment=AUTUMN_MAINTENANCE_FLAG_FILE`
(falling back to its `WorkingDirectory` + the legacy relative
`tmp/autumn-maintenance.json`), not the shared path unconditionally. Never
describe it as the app's in-memory state — it is which file the unit polls plus
whether that file exists. Two state-drift reasons come from this probe (both only
on a `deployed` host): the live slot unit could not be read (cell reads `?`,
nothing is guessed), and the host's
app polls a release-local flag rather than the shared one — a unit predating
`AUTUMN_MAINTENANCE_FLAG_FILE`, whose remedy is to **redeploy that host**. So a
host deployed before this feature reports its release-local flag until redeployed.

`--strict` exits non-zero on any drift (cron-alertable);
`--json` is a stable contract: `hosts[]` with `host`, `reachable`, `mode`,
`release`, `live_slot`, `ready`, `maintenance`, `proxy_port`, `last_deploy`
(`{result, at}` or null — `result` is `"deployed"`, `"rolled back"` or
`"torn down"`), `drift[]`, plus `version_drift`, `state_drift[]`, `drifted`.
`maintenance` is `true` / `false` / **`null`** (null = the CLI could not prove
which flag file that host's running unit polls); both `false` and `null` are
falsy, so an existing `maintenance == true` check is unaffected.

Unlike `check`/`up`/`rollback`, `status` does NOT abort when the app config fails
to validate under the deploy profile: it prints a caveat on **stderr** (text and
`--json` alike, so stdout's shape is untouched) naming the config error and the
DECLARED `[server] port` it probes against, then reports the fleet. `check`, `up`
and `rollback` still refuse, deliberately — they grade and upload runtime values
(signing secret, DB URL), so an invalid config must stop them.

`autumn deploy maintenance on|off` fans maintenance mode out to every configured
host over SSH (same flags and wire format as the local `autumn maintenance on`,
which only writes THIS machine's working directory). Best-effort-and-aggregate:
every host attempted, non-zero if any failed, changed hosts NOT reversed (the
"Changed anyway: …" line lists only FULLY changed hosts).
**Maintenance does not drain a host from a load balancer** — `/ready` stays 200
by design — so never tell a user maintenance mode removes a host from rotation.
Deploy-managed hosts read `{app_dir}/shared/autumn-maintenance.json` because the
slot units carry `AUTUMN_MAINTENANCE_FLAG_FILE`; that shared path is written
FIRST (authoritative — a current unit reacts within 500 ms even if the next write
fails). For a host whose unit predates that override, a second write goes to the
file that unit polls, resolved from the host's **live slot unit** — never from
the `current` symlink, which is rewritten after the proxy flip and so can name a
release nothing is running. Two rows to recognise: `shared flag only — no release
is promoted on this host` is a SUCCESS (nothing running polls anything else), and
`PARTIAL — shared flag written, but the file this host's RUNNING unit polls was
NOT` is a FAILURE with a non-zero exit (unit unreadable or that write failed) —
never tell a user such a host is maintained; `on` may have left it serving
traffic and `off` may have left it gated. Like `status`, `deploy maintenance`
does NOT abort on a config that fails to validate under the deploy profile: same
stderr caveat, then it continues against the DECLARED `[server] port`, used only
to identify which slot unit each host runs (`deploy status --json`'s shape is
unchanged by any of this). The local `autumn maintenance`
has no override for that path — it always writes cwd-relative
`tmp/autumn-maintenance.json` — so running it ON a deploy-managed host writes a
file the app never reads and exits 0 with no warning. Always route users to
`autumn deploy maintenance` for deploy-managed hosts.

See `docs/guide/fleet-deploys.md`, `docs/guide/deployment.md`, and
`docs/guide/maintenance-mode.md`.

`autumn destroy` mirrors `autumn generate` argument-for-argument and never
touches a database — it only reverses generated files/migrations.

`autumn doctor --strict` is the deployment sanity check. It reports unsafe
production defaults, missing primaries, stale replica migrations, missing
signing secrets, and other config problems without printing credentials.

## System tests (browser, feature `system-tests`)

`SystemTest` boots the app on an ephemeral port, launches managed headless
Chromium, and hands back a `Page` with htmx-aware auto-waiting assertions. Add
`autumn-web = { features = ["system-tests"] }` as a **dev-dependency** only.

```rust
let runner = SystemTest::new()
    .routes(routes![index, create_todo])
    .state(state)                                       // real pool/policies
    .layer(axum::middleware::from_fn(scope_to_tenant))  // 0.7.0
    .build().await.unwrap();

let page = runner.page().await.unwrap();
page.visit("/").await?;
page.click("Add").await?;          // auto-waits for htmx settle
page.expect_text("Saved").await?;
page.expect_no_console_errors().await?;
```

- `.layer(...)` (**0.7.0**) takes the same layers as `AppBuilder::layer`
  and puts them in the same stack position, so middleware that reads the
  request ID or session behaves as in production. Use it whenever the routes
  under test need global middleware — mapping layers onto individual handlers
  instead tests a stack the real app never serves.
- `expect_*` assertions poll to a deadline and ignore the transient CDP errors
  a mid-poll navigation (e.g. a redirecting form submit) produces; `evaluate`
  is a single raw call that does not.
- Generated tests are `#[ignore]`d — run with `-- --include-ignored`. Failures
  write a `.png` + `.html` to `target/system-tests/<test>/`.
- `autumn doctor` reports whether a usable browser was found, using the same
  resolution as the harness (`AUTUMN_CHROMIUM` overrides it).

Full API: `skills/autumn-web/references/api-reference.md`; guide:
`docs/guide/system-tests.md`.

## 0.7.0 release traps

- `AUTUMN_SECURITY__SIGNING_SECRET` is required in `prod` / `production`.
- Use `autumn-storage-s3 = "0.7"` and `autumn-cache-redis = "0.7"`; these
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
- Release tag for this line is `v0.7.0`.
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
- `docs/migrations/README.md` (per-release upgrade guides; `next.md` is the
  rolling draft for unreleased breaking changes)
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
- `docs/guide/sandboxed-plugins.md`
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
- `docs/guide/seo.md`
- `docs/guide/forms.md`
- `docs/guide/extractors.md`
- `docs/guide/cookie-consent.md`
- `docs/guide/middleware.md`
- `docs/guide/widget-styling.md`
- `docs/guide/tabs.md`
- `docs/guide/maintenance-mode.md`
- `docs/guide/deployment.md`
- `docs/guide/fleet-deploys.md`
- `docs/guide/hot-upgrades.md`
- `docs/guide/staged-deploys.md`
- `docs/guide/dev-loop-latency.md`
- `docs/guide/system-tests.md`
- `docs/guide/testing.md`
- `docs/autumn-workflow-architecture.md`
