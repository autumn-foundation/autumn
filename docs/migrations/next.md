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
- **Expected upgrade effort:** none for application code. Small for **plugins
  and libraries** that build `autumn_web::Route` values by hand.
- **MSRV delta:** `1.88.0` -> `1.88.0` (unchanged so far)
- **Carried dependency majors:** none so far

## Summary

Nothing in the unreleased line changes how an application written against
0.6.x compiles or behaves. The single break is at the *route-construction*
seam: `Route` and `StaticRouteMeta` gained a field, so code that builds those
structs with a literal — which in practice means plugins assembling a
`Vec<Route>` rather than using `routes![]` — has to name it.

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

## How to verify

1. `cargo check` — clean, with no `missing field `seo`` error.
2. `cargo test` — your suite is green.
3. Plugins: `autumn plugin-check --plugin-name <your-plugin>` passes
   (`--plugin-name` is required).
4. Serve the app (`cargo run`, default `http://127.0.0.1:3000`) and view-source
   a page whose route declares `seo(...)` — the declared `<title>` / `<meta>`
   values render.
5. `curl http://127.0.0.1:3000/sitemap.xml` — no `robots = "noindex"` static
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
