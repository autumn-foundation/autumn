# Migrating to the next Autumn release (rolling draft)

> **Rolling draft.** This is the in-flight guide for the changes currently
> under `## [Unreleased]` in [`CHANGELOG.md`](../../CHANGELOG.md). Every PR
> that lands a breaking change appends a section here and links this file from
> its changelog entry. At release time the file is renamed to
> `docs/migrations/<version>.md`, its version placeholders are filled in, and
> the index in [`README.md`](README.md) is updated — see
> [`docs/release-checklist.md`](../release-checklist.md), *Migration Guide
> Gate*.

## At a glance

- **Old version:** `autumn-web 0.6.x`
- **New version:** the next release (unreleased)
- **Expected upgrade effort:** none for application code, beyond two new
  deprecation warnings if you call `scheduler::now_unix_secs` /
  `now_unix_duration` (see *Deprecations* below). Small for **plugins
  and libraries** that build `autumn_web::Route` values by hand, for **JSON
  API clients** of a scaffolded model that declares a `lock_version` column,
  and for the (rare) **layer author** who implemented
  `tower::Layer<axum::routing::Route>` for a concrete type instead of
  generically.
- **MSRV delta:** `1.88.0` -> `1.88.0` (unchanged so far)
- **Carried dependency majors:** none so far

## Summary

Nothing in the unreleased line changes how an application written against
0.6.x compiles or behaves. There are three breaks, all narrow. The first is at
the *route-construction* seam: `Route` and `StaticRouteMeta` gained a field, so
code that builds those structs with a literal — which in practice means plugins
assembling a `Vec<Route>` rather than using `routes![]` — has to name it. The
second is on the wire, not in Rust: a model declaring a `lock_version` column
now requires JSON `PUT`/`PATCH` clients to send that version. The third is at
the *layer-registration* seam: `AppBuilder::layer` accepts layers via a bound
on Autumn's erased ingress service rather than `axum::routing::Route`, which
only a layer implemented non-generically against `Route` itself can notice.

## Before you start

1. Pin your current dependency (`autumn-web = "=0.6.0"`) and commit.
2. Make sure your tests are green on 0.6.x.
3. If you maintain a plugin, have its conformance check to hand
   (`autumn plugin-check --plugin-name <your-plugin>`).

## Step-by-step

1. **Bump the dependency.**
2. **Run `cargo check`.** Application code should be clean; a plugin that
   constructs routes literally gets `error[E0063]` (see below).
3. **Add the missing field** to each literal.
4. **Run the test suite** and the [verification list](#how-to-verify).

## Breaking changes

### Routing: `Route` and `StaticRouteMeta` gained an `seo` field

**Automation:** `manual` — no mechanical rewrite. A new struct field needs a
value, and only the author of the route knows which one; adding arguments is
outside the rename-level scope of `autumn upgrade`.

**Why:** route-level SEO defaults (`#[get("/about", seo(title = "..."))]`) are
recorded on the route itself, so the router can install the request extension
only for routes that declared something and routes without `seo(...)` pay
nothing (#1182).

`routes![]` and the route attribute macros fill the field in for you. Only a
**literal** struct construction has to change.

**Before (`0.6`):**

```rust,ignore
let route = autumn_web::Route {
    method: Method::GET,
    path: "/health",
    handler,
    // ...
};
```

**After:**

```rust,ignore
let route = autumn_web::Route {
    method: Method::GET,
    path: "/health",
    handler,
    seo: autumn_web::seo::SeoRouteDefaults::EMPTY,
    // ...
};
```

The same applies to `autumn_web::static_gen::StaticRouteMeta`.

`SeoRouteDefaults` is `#[non_exhaustive]` and built by chaining its `const fn
with_*` setters from `EMPTY`, so future SEO keys are additive — this is the
last time this particular field costs you an edit.

**If you are automating the upgrade:**

```bash
rg -n 'Route \{|StaticRouteMeta \{' src/
```

### JSON API: a `lock_version` model now requires the version on `PUT`/`PATCH`

**Automation:** `manual` — no mechanical rewrite. The change is on the wire,
in clients this tool never sees, not in the app's Rust source.

**Why:** declaring a column literally named `lock_version` opts a model into
optimistic locking (#1318). `#[lock_version]` carries the column on
`Update{Model}` as the *expected* version, which is what lets the repository
reject a stale write with `RepositoryError::Conflict` instead of silently
letting the last writer win.

This affects you only if **both** are true: your model declares a non-nullable
`i32`/`i64` column named `lock_version`, and you have JSON clients writing
through the generated `PUT`/`PATCH /api/<plural>/{id}` endpoints. Browser
forms on an HTML scaffold are unaffected — the generated edit form carries the
version in a hidden field for you.

**Before (`0.6`):** the field was ignored on the wire, so a client could omit
it.

```jsonc
{ "title": "Hello" }
```

**After:** the field is required, and its value must be the version the client
read. Omitting it fails deserialization (HTTP 422); sending a stale one is
answered with a conflict rather than an overwrite.

```jsonc
{ "title": "Hello", "lock_version": 7 }
```

Read the current version from the same record's `GET` response (it is a plain
column) and echo it back on the next write.

**If you did not want optimistic locking**, the column name is the entire
opt-in: rename it (e.g. to `revision`) and the behaviour goes away. `autumn
generate` prints a warning naming this escape hatch whenever it detects the
name.

### App-wide layers are now erased

**Automation:** `manual` — no mechanical rewrite. The fix is to make a layer
impl generic, which is a change of shape rather than of name.

**Why:** `AppBuilder::layer` / `static_gate` registrations (including plugin
layers) are type-erased at registration time and composed into a single
`Router::layer` application, so registering app-wide middleware no longer
deepens the framework's per-request clone cascade (#2198).

`IntoAppLayer`'s sealed blanket impl is therefore bound on
`tower::Layer<autumn_web::app::ErasedAppService>` (the new public alias for
the composed ingress service) instead of `tower::Layer<axum::routing::Route>`.

This affects you only if you wrote a layer against **one concrete inner
service type** rather than generically. Every layer that is generic over the
service it wraps — the shape of every tower / tower-http layer, every layer in
this repo and its plugins, and every example in the docs — satisfies both
bounds and compiles unchanged.

**Before (`0.6`):**

```rust,ignore
impl tower::Layer<axum::routing::Route> for MyLayer { /* … */ }
```

**After** — either make it generic (preferred):

```rust,ignore
impl<S> tower::Layer<S> for MyLayer { /* … */ }
```

or retarget the one concrete impl:

```rust,ignore
impl tower::Layer<autumn_web::app::ErasedAppService> for MyLayer { /* … */ }
```

**If you are automating the upgrade:**

```bash
rg -n 'Layer<axum::routing::Route>|Layer<Route>' src/
```

## Deprecations (non-breaking)

### `scheduler::now_unix_secs` / `scheduler::now_unix_duration`

**Automation:** `manual` — no mechanical rewrite. The replacement takes the
app's injected clock as a new argument, and finding the right `AppState` in
scope is inference well beyond a rename — explicitly out of scope for
`autumn upgrade` (issue #1629).

Both read the real system clock directly, off the framework's injected-clock
seam, so anything derived from them — a scheduled-task tick key, an expiry
computation — is not reproducible under a `#[sim_test]` and is exposed to a
wall-clock jump. They still work exactly as before; they now emit a
deprecation warning.

```rust
// Before
let secs = autumn_web::scheduler::now_unix_secs();
let dur  = autumn_web::scheduler::now_unix_duration();

// After — read the app's injected clock
let secs = autumn_web::time::clock_unix_secs(state.clock());
let dur  = autumn_web::time::clock_unix_duration(state.clock());
```

`state` is any `AppState` (handlers, `#[job]`/`#[scheduled]` bodies, startup
hooks). If you hold an `Arc<dyn ClockSource>` rather than an `AppState`, pass
`clock.as_ref()`. If you genuinely have neither in scope and cannot thread one
in yet, `#[allow(deprecated)]` at the call site is a fine interim step — the
functions are not scheduled for removal before the next major release (see
[STABILITY.md](../../STABILITY.md), *Deprecation process*).

The framework's own scheduler already reads the injected clock; neither function
has a remaining caller inside `autumn`.

## Compiler error cheat sheet

| Error message (truncated) | Where you see it | Fix |
|---|---|---|
| ``error[E0063]: missing field `seo` in initializer of `Route` `` | a literal `Route { .. }` | add `seo: autumn_web::seo::SeoRouteDefaults::EMPTY` |
| ``error[E0063]: missing field `seo` in initializer of `StaticRouteMeta` `` | a literal `StaticRouteMeta { .. }` | same |

## Configuration changes

None so far.

## Behavior changes

- A `#[static_get]` route declaring `robots = "noindex"` is now left out of the
  generated `sitemap.xml`. Entries supplied by a `SitemapSource` you register
  are still passed through unfiltered.
- On a model with a `lock_version` column, a concurrent write no longer wins
  silently: the HTML scaffold's edit form comes back at **409** with your input
  intact and the record's current version, and the JSON path answers with a
  conflict.

## How to verify

1. `cargo check` — clean, with no `missing field `seo`` error.
2. `cargo test` — your suite is green.
3. Plugins: `autumn plugin-check --plugin-name <your-plugin>` passes
   (`--plugin-name` is required).
4. If you have a `lock_version` model with JSON clients: `PUT` a record without
   `lock_version` and confirm it is rejected, then `PUT` it with the version
   from the record's `GET` and confirm it succeeds.
5. Serve the app (`cargo run`, default `http://127.0.0.1:3000`) and view-source
   a page whose route declares `seo(...)` — the declared `<title>` / `<meta>`
   values render.
6. `curl http://127.0.0.1:3000/sitemap.xml` — no `robots = "noindex"` static
   route is listed. Substitute your own address if you changed `[server] host`
   or `[server] port`.

### Guide-only upgrade walkthrough

The release checklist requires upgrading an app scaffolded with `autumn new` on
the previous release using **only** this guide — no changelog, no source
reading. See [`docs/release-checklist.md`](../release-checklist.md), *Migration
Guide Gate*.

- **Status:** pending — performed and recorded during the release that ships
  this guide, before publishing to crates.io.

## Reporting problems

If you hit something this guide does not cover, that is a bug in the guide.
Open an issue at <https://github.com/autumn-foundation/autumn/issues> with the
error or unexpected behaviour, the version you upgraded from, and a minimal
reproduction if you have one.
