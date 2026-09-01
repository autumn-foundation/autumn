# Design: Hybrid Rendering for Autumn

**Date:** 2026-03-26
**Author:** markm
**Status:** Draft
**Relates to:** `docs/architecture-autumn-2026-03-20.md`, `docs/prd-autumn-2026-03-20.md`

---

## Executive Summary

This document proposes adding a hybrid rendering model to Autumn, enabling routes to opt into static site generation (SSG) alongside the existing server-side rendering (SSR) mode. Inspired by Next.js ISR and Astro's per-route rendering decisions, but designed for Rust's compile-time strengths, this feature would make Autumn the first Rust web framework to offer integrated static generation with server rendering in a single coherent stack.

The core idea: a new `#[static_get]` proc macro that marks routes for pre-rendering at build time via `autumn build`. Routes using the existing `#[get]` continue to render per-request. An optional `revalidate` parameter enables Incremental Static Regeneration (ISR) — serving cached static HTML while periodically refreshing it in the background.

This positions Autumn uniquely in the competitive landscape: Loco, Rocket, and Cot offer no static generation. Zola and Cobalt are pure SSGs with no server rendering. Autumn would be the bridge.

---

## Motivation

### The Problem

Autumn currently renders every page on every request. This is correct for dynamic content (dashboards, authenticated pages, database-driven listings), but wasteful for content that changes infrequently: blog posts, about pages, marketing copy, documentation. These pages hit the database, render Maud templates, and produce identical HTML every time.

### The Opportunity

Autumn's architecture is uniquely positioned for hybrid rendering because:

1. **Maud templates are pure functions.** They take data in, produce `Markup` out, with no side effects. This makes them trivially reproducible at build time.
2. **The `routes![]` macro already has full visibility** into every route's path and handler at compile time.
3. **The `autumn-cli` crate already exists** as the place for build-time tooling (`autumn new`, `autumn setup`). Adding `autumn build` is a natural extension.
4. **Tailwind CSS is already compiled at build time** via `build.rs`. Extending the build step to also render static pages is architecturally consistent.

### Competitive Position

| Framework | SSR | SSG | Hybrid | ISR |
|-----------|-----|-----|--------|-----|
| **Autumn (proposed)** | **Yes** | **Yes** | **Yes** | **Yes** |
| Loco | Yes | No | No | No |
| Rocket | Yes | No | No | No |
| Zola | No | Yes | No | No |
| Cobalt | No | Yes | No | No |
| Next.js | Yes | Yes | Yes | Yes |
| Astro | Yes | Yes | Yes | No* |

*Astro supports on-demand rendering but not time-based ISR natively.

Autumn would be the **only Rust framework** offering hybrid rendering with ISR.

---

## Design Principles

These principles are inherited from the existing architecture doc and extended for this feature:

1. **Opt-in, not opt-out.** All routes are server-rendered by default. Static generation is explicitly requested per-route. No global mode switch that changes all behavior.

2. **Thin wrapper, not deep rewrite.** The `#[static_get]` macro generates the same route registration code as `#[get]`, plus metadata that the build system reads. The handler function itself is unchanged.

3. **Failure must be loud.** If `autumn build` cannot render a static route (e.g., it requires a `Db` extractor), it fails at compile time with a clear error, not at runtime with a 404.

4. **`cargo build` stays pure.** Static page rendering happens in `autumn build`, not in `cargo build` or `build.rs`. This preserves the existing contract that `cargo build` is deterministic and offline-capable.

5. **Progressive enhancement.** An Autumn app works identically whether or not `autumn build` has been run. Static files are an optimization layer, not a requirement.

6. **Static means public.** Static routes are served directly from disk, bypassing all request-scoped middleware (authentication, rate limiting, CORS, session handling, access logging). This is by design — it's what makes them fast. But it means `#[static_get]` routes must never require authentication or per-user logic. This contract is enforced at compile time: if a `#[static_get]` handler accepts auth-scoped extractors (`Session`, `AuthUser`, `CookieJar`, `Extension<CurrentUser>`, etc.), the macro emits `compile_error!("Static routes are served without authentication middleware. Use #[get] for authenticated routes.")`.

---

## API Surface

### New Proc Macro: `#[static_get]`

```rust
use autumn_web::prelude::*;

/// Rendered once at build time. Served as static HTML.
#[static_get("/about")]
async fn about() -> Markup {
    html! {
        h1 { "About Autumn" }
        p { "An opinionated web framework for Rust." }
    }
}
```

**Expansion:** Identical to `#[get("/about")]` plus a companion metadata function:

```rust
#[doc(hidden)]
pub fn __autumn_static_meta_about() -> autumn_web::static_gen::StaticRouteMeta {
    autumn_web::static_gen::StaticRouteMeta {
        path: "/about",
        name: "about",
        revalidate: None,
        params: None,
    }
}
```

At runtime, `#[static_get]` routes behave identically to `#[get]` routes. The static optimization is applied only when `autumn build` is run.

### ISR: Time-Based Revalidation

```rust
/// Served as static HTML. Regenerated in background every 60 seconds.
#[static_get("/", revalidate = 60)]
async fn index(mut db: Db) -> AutumnResult<Markup> {
    let posts = Post::published(&mut db).await?;
    Ok(layout("Home", html! {
        @for p in &posts { (post_card(p)) }
    }))
}
```

When `revalidate` is set:
- The page is pre-rendered at build time (or on first request).
- Subsequent requests serve the cached HTML immediately.
- After `revalidate` seconds have elapsed, the next request triggers a background re-render.
- The stale page is served until the fresh version is ready (stale-while-revalidate pattern).

### Dynamic Path Parameters: `params`

For routes with path parameters, a `params` function supplies the set of values to pre-render:

```rust
#[static_get("/posts/{slug}", params = post_slugs)]
async fn show(slug: Path<String>, mut db: Db) -> AutumnResult<Markup> {
    let post = Post::find_by_slug(&slug, &mut db).await?;
    Ok(layout(&post.title, render_post(&post)))
}

/// Called at build time to enumerate all slugs to pre-render.
async fn post_slugs(mut db: Db) -> Vec<StaticParams> {
    let posts = Post::published(&mut db).await.unwrap_or_default();
    posts.iter()
        .map(|p| static_params! { "slug" => &p.slug })
        .collect()
}
```

If a request arrives for a slug that wasn't pre-rendered (e.g., a new post), Autumn falls back to server-rendering and optionally caches the result (if ISR is enabled). This is the "fallback" behavior, matching Next.js's `fallback: "blocking"` semantics.

### Configuration: `autumn.toml`

```toml
[static]
# Output directory for pre-rendered HTML (default: "dist/")
output_dir = "dist"

# Number of routes to render concurrently during `autumn build` (default: 8).
# Bounded by the database pool size when routes use Db.
concurrency = 8

# Whether to also pre-render #[static_get] routes that return Json<T>
# as .json files in the output directory (default: true)
json_routes = true

# Global fallback behavior for parameterized static routes when a
# request arrives for a param value that wasn't pre-rendered.
# "server" = render on demand via SSR, cache result if ISR enabled (default)
# "404" = return 404 immediately for unknown params
# This is a global default; per-route override planned for Phase 3.
fallback = "server"
```

### CLI: `autumn build`

```bash
# Pre-render all #[static_get] routes
$ autumn build

  Loading autumn.toml...
  Connecting to database...
  Collecting static routes...

  Rendering /about            → dist/about/index.html          (0.2ms)
  Rendering /                 → dist/index.html                (4.1ms, revalidate: 60s)
  Rendering /posts/hello-world → dist/posts/hello-world/index.html (3.8ms)
  Rendering /posts/rust-2026   → dist/posts/rust-2026/index.html   (3.2ms)
  Rendering /api/posts         → dist/api/posts.json            (2.1ms)

  ✓ 5 pages rendered in 13.4ms
  ✓ Output: dist/
```

`autumn build` performs the following steps:

1. Compiles the application binary (calls `cargo build`).
2. Starts a lightweight runtime that boots the app (config, database pool, etc.) without binding a network port.
3. Invokes `#[static_get]` handlers **concurrently** (configurable via `[static] concurrency` in `autumn.toml`, default: 8). Each route is an independent async task; the database pool naturally throttles connection usage.
4. **Validates each response:** if a handler returns a non-2xx HTTP status code, the build fails for that route with a clear error (e.g., `ERROR: /posts/hello-world returned 500 Internal Server Error — not writing to dist`). This prevents error pages from being silently written to `dist/` as if they were real content.
5. Writes rendered HTML to a **staging directory** (`{output_dir}.staging/`, e.g., `dist.staging/`) with clean URL structure (`/about` → `dist.staging/about/index.html`).
6. Copies static assets (`static/css/`, `static/js/`) into the staging directory.
7. **Atomic swap:** On complete success, renames `{output_dir}.staging/` → `{output_dir}/` (removing previous output first). On any failure, the previous `dist/` is left intact and the staging directory is cleaned up. This prevents partial builds from producing a half-deployed site.

If any routes fail, `autumn build` exits with a non-zero status and prints a summary:

```
  ✗ 2 of 5 routes failed:
    /posts/draft-post  → 404 Not Found
    /posts/broken      → 500 Internal Server Error
  ✓ 3 routes rendered successfully (not written — build failed)
  Previous dist/ left intact.
```

### Routes Macro Integration

Static routes are registered alongside dynamic routes in the existing `routes![]` macro with no syntax change:

```rust
autumn_web::app()
    .routes(routes![
        index,        // #[static_get("/", revalidate = 60)]
        about,        // #[static_get("/about")]
        show,         // #[static_get("/posts/{slug}", params = post_slugs)]
        admin_list,   // #[get("/admin")] — dynamic, not pre-rendered
        create,       // #[post("/admin")] — dynamic
    ])
    .run()
    .await;
```

---

## Architecture

### Build-Time Pipeline

```
┌─────────────────────────────────────────────────────────────────────────┐
│                          autumn build                                    │
│                                                                         │
│  1. cargo build --release                                               │
│     └─ build.rs runs Tailwind CSS compilation                           │
│                                                                         │
│  2. Boot application (no network listener)                              │
│     ├─ Load AutumnConfig from autumn.toml                               │
│     ├─ Create database pool (if configured)                             │
│     └─ Collect StaticRouteMeta from all #[static_get] routes            │
│                                                                         │
│  3. For each static route (concurrently, up to [static].concurrency):    │
│     ├─ If parameterized: call params function to get parameter sets      │
│     ├─ Construct synthetic Request for each path                        │
│     ├─ Invoke handler through Axum's router (full middleware stack)      │
│     ├─ Validate response: non-2xx = build failure for this route        │
│     ├─ Capture Response body as HTML string                             │
│     └─ Write to {output_dir}.staging/ with clean URL structure           │
│                                                                         │
│  4. Copy static assets (css, js, images) to {output_dir}.staging/       │
│  5. Generate manifest.json (route → file mapping + revalidation times)  │
│  6. Atomic swap: {output_dir}.staging/ → {output_dir}/ (on success)     │
└─────────────────────────────────────────────────────────────────────────┘
```

### Runtime Serving (Hybrid Mode)

When the server runs after `autumn build`, it serves requests through a layered strategy:

```
Request arrives: GET /posts/hello-world
         │
         ▼
┌─────────────────────┐
│  Static File Cache   │  Does dist/posts/hello-world/index.html exist?
│  (Tower middleware)  │
└────────┬────────────┘
         │
    ┌────▼────┐
    │  Fresh?  │  Is the file younger than `revalidate` seconds?
    └────┬────┘
         │
    Yes ─┤── Serve static HTML (zero compute, maximum speed)
         │   └─ If stale: spawn background task to re-render
         │
    No ──┤── No static file exists (new slug, or non-static route)
         │
         ▼
┌─────────────────────┐
│  Axum Router         │  Normal server-side rendering
│  (existing behavior) │
└─────────────────────┘
```

### New Middleware: `StaticFileLayer`

A Tower middleware that wraps `tower-http::ServeDir` and adds ISR-aware staleness checking:

```rust
pub struct StaticFileLayer {
    /// tower-http ServeDir instance for correct static file serving
    /// (Content-Type, conditional requests, HEAD, range requests).
    serve_dir: tower_http::services::ServeDir,
    /// Route manifest with revalidation metadata.
    manifest: Arc<StaticManifest>,
    /// Per-route ISR regeneration state.
    isr_state: Arc<IsrState>,
}
```

By building on `tower-http::ServeDir`, the middleware inherits correct handling for `Content-Type` negotiation, `If-Modified-Since` / `If-None-Match` conditional requests, `HEAD` method, and range requests — without reimplementing any of it. The `StaticFileLayer` adds only two behaviors on top: (1) checking ISR staleness via file `mtime`, and (2) spawning background revalidation tasks when content is stale.

This middleware is automatically inserted by `AppBuilder::run()` when a `dist/` directory exists. It sits above the Axum router in the middleware stack, meaning static files are served without touching the router, database, or any application code. **Important:** this also means static routes bypass all request-scoped middleware — see Design Principle #6 ("Static means public").

### Manifest File: `dist/manifest.json`

Generated by `autumn build`, consumed by the runtime (`generated_at` is
Unix-epoch seconds as a decimal string):

```json
{
  "generated_at": "1774555920",
  "autumn_version": "0.2.0",
  "routes": {
    "/about": {
      "file": "about/index.html",
      "revalidate": null,
      "content_type": "text/html; charset=utf-8"
    },
    "/": {
      "file": "index.html",
      "revalidate": 60,
      "content_type": "text/html; charset=utf-8"
    },
    "/sitemap.xml": {
      "file": "sitemap.xml/index.html",
      "revalidate": null,
      "content_type": "application/xml"
    },
    "/posts/hello-world": {
      "file": "posts/hello-world/index.html",
      "revalidate": null,
      "content_type": "text/html; charset=utf-8"
    }
  }
}
```

#### `content_type` — the intended MIME type, recorded at generation time

`url_to_file_path` stores every non-root route as `<route>/index.html`, which
throws the route's semantic type away: `/sitemap.xml` lands at
`sitemap.xml/index.html`, so the served file name says "HTML" while the route
says "XML". Reconstructing the intended type at *serve* time from those two
clues is a heuristic, and it needed three consecutive corrections during review
of #1819 — pre-compressed fonts, generated `.txt`/`.xml` routes, and HTML pages
whose slug merely contains a dot (`/posts/release.v1`,
`/users/alice@example.com`).

`render_static_routes` therefore records the `Content-Type` the handler declared
on each rendered response, and `StaticFileLayer::resolve_entry` hands it back to
the serve path verbatim (#1832). The type is known exactly once, where it is
actually known, and is never inferred again. This also lets a route be served as
a type no file extension maps to — `application/rss+xml` from `/feed`,
`text/calendar` from `/calendar` — which the extension-based heuristic could
never produce.

`content_type` is **optional**. It is absent when the manifest predates #1832
(an existing `dist/` that has not been rebuilt), when the manifest was written
by hand, or when the handler declared no `Content-Type` at all — in which case
nothing is recorded rather than a build-time guess being baked in. Whenever it
is absent, `static_gen::resolved_content_type` applies the pre-#1832 derivation
unchanged: recognized route extension first, then the served file name, then
`application/octet-stream`. The same function rejects a recorded value that is
not a legal header (CR/LF from a hand-edited or tampered manifest) and falls
back, so a bad manifest can neither inject a header nor panic the request path.

The type is a *build-time* property: ISR regeneration rewrites the file, not the
manifest (which stays immutable behind an `Arc`, with file `mtime` driving
staleness — see above). The served header is therefore fixed for the process
lifetime while the body on disk is not, so a regeneration whose handler declares
a **different** `Content-Type`, or stops declaring one, is **refused** rather
than written. The previous file stays on disk still matching its recorded type,
which degrades the route to stale-but-correct instead of serving fresh bytes
under a header that mislabels them. The refusal travels the normal regeneration
error path (logged, then throttled by the ISR cooldown); `autumn build` is what
re-records the type.

### ISR Background Regeneration

When a request hits a stale ISR page, the `StaticFileLayer` middleware:

1. Serves the stale HTML immediately (fast response).
2. Spawns a Tokio task that:
   a. Invokes the route handler through the Axum router.
   b. Captures the response body.
   c. Atomically writes the new HTML to the file (write to `.tmp`, then rename).
   d. Updates the manifest's `generated_at` timestamp.
3. Subsequent requests serve the fresh version.

This is a non-blocking operation. A per-route `AtomicBool` flag prevents duplicate regenerations: if a regeneration is already in flight, subsequent stale requests skip the spawn and just serve stale HTML. This eliminates thundering-herd issues without file locks.

**Error handling:** If regeneration fails (database down, handler panics), the error is logged via `tracing::error!` and the stale HTML remains. No data is lost. A **cooldown period** (30 seconds) is applied before the next regeneration attempt for that route — implemented via an `AtomicU64` storing the last attempt timestamp alongside the `AtomicBool` in-flight flag. This prevents error cascade tight loops where high traffic on a failing route produces a continuous stream of failed regeneration attempts and log noise.

**Startup stagger:** When the server starts with a `dist/` directory containing stale ISR pages, all routes appear stale simultaneously. To avoid a thundering herd of regeneration tasks on first requests, the middleware applies random jitter (0 to `revalidate` seconds per route) before considering a page stale on the first check after boot. This spreads regeneration load across the revalidation window rather than spiking at startup.

**Minimum revalidate interval:** The `#[static_get]` macro enforces a floor of **10 seconds** for the `revalidate` parameter. Values below this are rejected at compile time with `compile_error!("revalidate must be at least 10 seconds")`. Very short intervals create performance problems (near-continuous regeneration) that negate the benefits of static rendering.

**Concurrent writes:** Each route's HTML file is written to a unique `.tmp` path (`{path}.tmp.{pid}`), then renamed atomically via `std::fs::rename`. On POSIX systems, rename is atomic. On Windows, `rename` may fail if the target is open; the middleware falls back to SSR on write failure. The manifest is not updated per-regeneration — instead, the file's `mtime` is used for staleness checks, avoiding manifest write contention entirely.

**Ephemeral filesystem note:** ISR requires a persistent `dist/` directory. In container deployments without persistent volumes, the `dist/` directory is lost on restart, and all routes fall back to SSR until re-rendered by ISR or a new `autumn build`. This is safe (SSR fallback works) but loses the performance benefit. For containerized deployments, a persistent volume mount for `dist/` is recommended.

### Static Route Discovery Mechanism

The build system discovers static routes through **compile-time code generation**, not runtime reflection:

1. The `routes![]` macro already collects all route handler names.
2. When `#[static_get]` is used, the macro generates a companion `__autumn_static_meta_{name}()` function alongside the regular `__autumn_route_info_{name}()`.
3. The `routes![]` macro is extended to also generate a `__autumn_static_routes()` function that returns `Vec<StaticRouteMeta>` for all static routes in the list.
4. The `#[autumn_web::main]` macro is extended to recognize an `--autumn-build` CLI flag. When present, instead of starting the HTTP listener, it invokes the build pipeline using the collected metadata.

This approach requires **no** runtime reflection, `inventory` crate, or symbol table scanning. It's pure compile-time code generation, consistent with Autumn's "static dispatch everywhere" principle.

```rust
// What #[autumn_web::main] generates when static-gen feature is enabled:
#[tokio::main]
async fn main() {
    if std::env::args().any(|a| a == "--autumn-build") {
        autumn_web::static_gen::run_build(
            __autumn_static_routes(),  // generated by routes![]
            autumn_web::config::AutumnConfig::load().unwrap(),
        ).await;
        return;
    }
    // ... normal server startup ...
}
```

`autumn build` then simply runs: `cargo build --release && ./target/release/my-app --autumn-build`

### `params` Function Contract

The `params` function must:
- Be an `async fn` (or return a `Future`)
- Accept only extractors that are available at build time: `Db` and `axum::extract::State<AppState>`
- Return `Vec<StaticParams>`
- **Not** accept request-scoped extractors (`Path`, `Query`, `Form`, `Headers`, etc.) since there is no HTTP request at build time

The `params` attribute uses an **unquoted identifier** (not a string literal): `params = post_slugs`. This allows the proc macro to resolve it as an actual Rust path, enabling IDE autocomplete, go-to-definition, and compiler-native "function not found" errors rather than opaque macro errors.

`StaticParams` is a type alias for `HashMap<String, String>`:

```rust
pub type StaticParams = std::collections::HashMap<String, String>;
```

The `static_params!` macro is syntactic sugar:

```rust
// static_params! { "slug" => "hello-world" }
// expands to:
// HashMap::from([("slug".to_owned(), "hello-world".to_owned())])
```

### Path Safety

All path parameters used in file path construction are sanitized:
- Path segments are validated to contain only alphanumeric characters, hyphens, underscores, and dots
- Segments starting with `.` are rejected (prevents `..` traversal)
- Segments containing path separators (`/`, `\`) are rejected
- Invalid segments cause `autumn build` to emit a compile-time-style error and skip the route

### Manifest Loading

The manifest is loaded **once** at server startup and held in an `Arc<StaticManifest>`. It is never re-read from disk during request handling. The `StaticFileLayer` checks file existence and `mtime` on the filesystem (which the OS caches efficiently), not the manifest, for staleness decisions. The manifest is used only for the initial set of known static routes and their `revalidate` values.

### Startup Route Reconciliation

At server startup, if `dist/manifest.json` exists, the runtime compares the manifest's route list against the binary's currently registered routes. This catches stale `dist/` directories:

- **Stale route in dist:** A route exists in `manifest.json` but is no longer a `#[static_get]` in the binary (e.g., annotation was removed or route was deleted). Logged as `WARN: dist/ contains stale route /old-page — will be served as static until next autumn build`.
- **Missing route from dist:** A `#[static_get]` route exists in the binary but not in `manifest.json` (e.g., new route added without re-running `autumn build`). Logged as `INFO: static route /new-page not in dist/ — will fall back to SSR`.
- **Version mismatch:** The manifest includes an `autumn_version` field. If it doesn't match the runtime's version, logged as `WARN: dist/ was built with autumn v0.2.1 but runtime is v0.3.0 — consider re-running autumn build`.

This reconciliation is informational only (warnings, not errors). The server always starts successfully.

### Feature Flag Behavior

If a developer writes `#[static_get]` without the `static-gen` feature enabled:

```
error: the `#[static_get]` macro requires the `static-gen` feature.
       Add this to your Cargo.toml:
       autumn-web = { version = "0.1", features = ["static-gen"] }
  --> src/routes.rs:5:1
```

The `#[static_get]` proc macro is always compiled (proc macros can't be feature-gated), but when `static-gen` is disabled, it emits `compile_error!` with an actionable message.

### Database Access During Build

`autumn build` requires the same `autumn.toml` configuration as `cargo run`, including a live database if any `#[static_get]` routes use the `Db` extractor. For CI/CD pipelines, this means:

- **Docker Compose:** Add a `db` service (same as development)
- **GitHub Actions:** Use the `services` key to start Postgres
- **Offline builds:** If no static routes require `Db`, no database is needed. Routes like `#[static_get("/about")]` that return hardcoded Markup work without any infrastructure.

There is no `--offline` mode or data seeding mechanism in the initial design. If a static route requires `Db` and no database is available, `autumn build` fails with a clear error pointing at the route that needs the connection.

---

## Crate Changes

### `autumn-macros`

New additions:
- `#[static_get("/path")]` proc macro (mirrors `#[get]` internally, adds `StaticRouteMeta` companion)
- `static_params!` helper macro for building parameter sets

Changes:
- `routes_macro.rs` updated to also collect `__autumn_static_meta_*` functions when present

### `autumn` (runtime crate)

New module: `static_gen`

```
autumn/src/
├── static_gen/
│   ├── mod.rs          // StaticRouteMeta, StaticManifest, StaticParams types
│   ├── middleware.rs    // StaticFileLayer (Tower middleware)
│   └── revalidate.rs   // ISR background regeneration logic
```

Changes:
- `app.rs`: `AppBuilder` detects `dist/` directory and inserts `StaticFileLayer`
- `config.rs`: New `[static]` section in `AutumnConfig`

### `autumn-cli`

New command: `autumn build`

```
autumn-cli/src/
├── build.rs    // autumn build implementation
```

The build command:
1. Calls `cargo build` to compile the app.
2. Loads the compiled binary as a library (via a generated helper binary) or invokes it with a special `--autumn-build` flag.
3. Collects static route metadata.
4. Renders pages and writes output.

### Feature Flag

```toml
[features]
default = ["maud", "htmx", "tailwind", "db"]
static-gen = []  # Enables #[static_get], StaticFileLayer, ISR
```

The `static-gen` feature is **not** in the default set for v0.1. It's an explicit opt-in, consistent with Autumn's "magic must be earned" principle.

---

## Compile-Time Safety

### Preventing Invalid Static Routes

The `#[static_get]` macro enforces constraints at compile time:

**1. No side-effecting parameters without `params` function:**
```rust
// ✗ Compile error: static route with path parameter requires `params`
#[static_get("/posts/{slug}")]
async fn show(slug: Path<String>) -> Markup { ... }
```

**2. `params` function must exist and have correct signature:**
```rust
// ✗ Compile error: function `post_slugs` not found
#[static_get("/posts/{slug}", params = post_slugs)]
async fn show(slug: Path<String>) -> Markup { ... }
```

**3. Revalidate must be a positive integer ≥ 10:**
```rust
// ✗ Compile error: revalidate must be a positive number of seconds
#[static_get("/", revalidate = -1)]
async fn index() -> Markup { ... }

// ✗ Compile error: revalidate must be at least 10 seconds
#[static_get("/", revalidate = 3)]
async fn index() -> Markup { ... }
```

### Runtime Fallback Guarantee

If `autumn build` has not been run, the app works identically to today. The `StaticFileLayer` checks for the `dist/` directory on startup; if absent, it's a no-op. This means:

- `cargo run` during development → full server rendering (no change)
- `autumn build && cargo run` in production → hybrid rendering
- Deploy `dist/` to CDN without a server → pure static site

---

## Deployment Modes

The hybrid model enables three deployment strategies from the same codebase:

### Mode 1: Traditional Server (Current Default)

```bash
cargo run --release
```

All routes server-rendered. No `autumn build` needed. This is the existing behavior, completely unchanged.

### Mode 2: Hybrid Server (Recommended for Production)

```bash
autumn build --release
cargo run --release
```

Static routes served from `dist/`. Dynamic routes server-rendered. ISR routes refresh in background. Best balance of performance and freshness.

### Mode 3: Pure Static (CDN Deploy)

```bash
autumn build --release
# Deploy dist/ to Netlify, Vercel, Cloudflare Pages, S3, etc.
```

Only works if **all** routes are `#[static_get]`. The `autumn build` output is a complete static site. No Rust server needed at runtime.

For Mode 3, `autumn build` emits a warning if any `#[get]` (non-static) routes exist, since those routes won't be available in a static deployment.

---

## Testing Utilities

Static rendering introduces a testing gap: `cargo test` exercises routes through the Axum router (SSR), but doesn't verify that `autumn build` would succeed or that the rendered HTML is correct. To close this gap, the `autumn` crate provides test utilities:

### `StaticRenderTest`

```rust
#[tokio::test]
async fn test_about_page_renders_statically() {
    let result = autumn_web::test::render_static(about).await;
    assert!(result.status().is_success());
    assert!(result.body_string().contains("About Autumn"));
}

#[tokio::test]
async fn test_blog_index_renders_with_revalidation() {
    let result = autumn_web::test::render_static(index).await;
    assert!(result.status().is_success());
    assert_eq!(result.revalidate(), Some(60));
}

#[tokio::test]
async fn test_post_params_generates_slugs() {
    let params = autumn_web::test::collect_params(post_slugs).await;
    assert!(!params.is_empty());
    assert!(params.iter().all(|p| p.contains_key("slug")));
}
```

`render_static` boots a minimal app context (config + database pool), invokes the handler as `autumn build` would, and returns a `StaticRenderResult` with the response status, body, and metadata. This lets tests verify:

- The handler returns 2xx (won't fail the build)
- The rendered HTML contains expected content
- The revalidation interval is correctly set
- The `params` function returns the expected parameter sets

These utilities ship in the `autumn` crate behind `#[cfg(test)]`, requiring no additional dependencies.

---

## Testing the Static Rendering Path

To validate the hybrid rendering API before implementation, the blog example (`examples/blog/`) should be annotated with the proposed API as a design exercise:

```rust
// Blog example with hybrid rendering annotations (design sketch)

#[static_get("/about")]
async fn about() -> Markup { /* ... */ }

#[static_get("/", revalidate = 60)]
async fn index(mut db: Db) -> AutumnResult<Markup> { /* ... */ }

#[static_get("/posts/{slug}", params = post_slugs)]
async fn show(slug: Path<String>, mut db: Db) -> AutumnResult<Markup> { /* ... */ }

#[get("/admin")]  // Dynamic — requires auth in the future
async fn admin_list(mut db: Db) -> AutumnResult<Markup> { /* ... */ }
```

If any route feels awkward with this annotation style, adjust the API before writing implementation code.

---

## Macro Syntax: Open Design Question

The current design uses `#[static_get]` as a separate macro. An alternative worth evaluating during Phase 1 prototyping:

### Option A (Current): `#[static_get("/path")]`

```rust
#[static_get("/about")]
async fn about() -> Markup { ... }

#[static_get("/", revalidate = 60)]
async fn index(mut db: Db) -> AutumnResult<Markup> { ... }
```

**Pros:** Clear, unambiguous. Easy to grep for static routes. Separate macro can have independent compile-time validation.

**Cons:** New macro name to learn. Implies GET-only (which is true, but not obvious from the name). Separate codepath in `autumn-macros` to maintain.

### Option B: `#[get("/path", render = "static")]`

```rust
#[get("/about", render = "static")]
async fn about() -> Markup { ... }

#[get("/", render = "static", revalidate = 60)]
async fn index(mut db: Db) -> AutumnResult<Markup> { ... }
```

**Pros:** Single macro family. Extends existing `#[get]` with a new parameter. Makes it clear that static is a *rendering mode*, not a different kind of route. Allows future values like `render = "isr"` or `render = "edge"`.

**Cons:** Longer annotation. `render` parameter only valid on `#[get]`, not `#[post]` — needs validation. String literal for the mode name.

**Decision:** Defer until Phase 1 prototyping. Implement both internally and see which produces better error messages and DX. The choice doesn't affect the build pipeline, middleware, or runtime behavior — only the macro surface.

---

## Implementation Phases

### Phase 1: Foundation (v0.2)

- `#[static_get]` macro (no `params`, no `revalidate` — static paths only)
- `autumn build` command (renders static-path routes to `dist/`)
- `StaticFileLayer` middleware (serves from `dist/`, falls back to router)
- `[static]` config section in `autumn.toml`
- `dist/manifest.json` generation
- Blog example updated with `#[static_get("/about")]` showcase

### Phase 2: Dynamic Params (v0.3)

- `params` function support for parameterized static routes
- `static_params!` macro
- `fallback` configuration (server vs 404)
- Blog example updated: all published posts pre-rendered with `params`

### Phase 3: ISR (v0.4)

- `revalidate` parameter on `#[static_get]`
- Background regeneration with atomic file replacement
- Stale-while-revalidate serving logic
- Manifest timestamp tracking

### Phase 4: Polish (v0.5)

- `autumn build --watch` for development (rebuilds on file change)
- Sitemap generation (`dist/sitemap.xml`)
- RSS/Atom feed generation for blog-style sites
- `autumn build --dry-run` to preview what would be rendered
- Performance metrics in build output (per-page render time)

---

## Open Questions

1. **Build approach: subprocess vs library?**
   Should `autumn build` compile and invoke the app as a subprocess with a special flag (`--autumn-build`), or should it load the compiled route handlers as a library? Subprocess is simpler and more isolated; library loading avoids recompilation but requires `cdylib` support.

   *Recommendation:* Subprocess with `--autumn-build` flag. Simpler, matches how the app actually runs, exercises the real middleware stack.

2. **Database access during build?**
   Static routes with `Db` extractors need database access at build time. This means `autumn build` needs a running Postgres instance. Is this acceptable?

   *Recommendation:* Yes. Document that `autumn build` requires the same infrastructure as `cargo run`. For CI/CD, this means a database service in the pipeline. This is the same requirement Next.js has for `getStaticProps` with database calls.

3. **Cache invalidation beyond time-based ISR?**
   Should Autumn support on-demand revalidation (e.g., webhook triggers `POST /api/revalidate?path=/posts/hello`)?

   *Recommendation:* Defer to Phase 4+. Time-based ISR covers 90% of use cases. On-demand revalidation adds API surface complexity and security considerations (authentication on the revalidate endpoint).

4. **How does this interact with htmx?**
   htmx makes partial page requests. Should `#[static_get]` routes that return htmx fragments also be pre-renderable?

   *Recommendation:* Phase 1 targets full-page routes only. htmx fragment endpoints are typically dynamic by nature (responding to user actions) and should remain `#[get]` or `#[post]`. Revisit if a clear use case emerges.

---

## Rejected Alternatives

### Global rendering mode switch

```toml
# REJECTED: Astro-style global mode
[rendering]
mode = "hybrid"  # or "static" or "server"
```

**Why rejected:** Violates Autumn's per-route opt-in principle. A global switch changes the default behavior of all routes, creating confusion about which routes are static and which are dynamic. Per-route annotation (`#[static_get]` vs `#[get]`) makes the rendering mode explicit and visible at the handler level.

### Build-time code generation (no runtime fallback)

**Why rejected:** A pure SSG mode with no server fallback means new content (e.g., a new blog post) requires a full rebuild and redeploy. ISR with server fallback means new content is available immediately via SSR, then cached as static on first request. The hybrid approach is strictly more capable.

### Separate static route file

```rust
// REJECTED: routes listed in a separate config file
// static_routes.toml
// routes = ["/about", "/posts/*"]
```

**Why rejected:** Splits the route definition across two places (the handler annotation and a config file). Autumn's philosophy is that the handler is the source of truth for its route. The `#[static_get]` annotation keeps everything co-located.

---

## References

- [Next.js Incremental Static Regeneration](https://nextjs.org/docs/app/guides/incremental-static-regeneration)
- [Astro Hybrid Rendering](https://docs.astro.build/en/guides/on-demand-rendering/)
- [Astro Islands Architecture](https://docs.astro.build/en/concepts/islands/)
- [Zola Static Site Generator](https://www.getzola.org/)
- [Autumn Architecture Doc](./architecture-autumn-2026-03-20.md)
- [Autumn PRD](./prd-autumn-2026-03-20.md)
- [Autumn Competitive Research](./research-competitive-technical-2026-03-20.md)
