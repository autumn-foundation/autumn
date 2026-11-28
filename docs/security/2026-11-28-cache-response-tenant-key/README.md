# 2026-11-28: `CacheResponseLayer`'s key never carries the tenant

**Class:** cross-tenant read, via a framework cache key that leaves out the tenant
**Surface:** `autumn_web::cache::CacheResponseLayer` (`autumn/src/cache/layer.rs`)
**Entry point:** any `GET` route under a `CacheResponseLayer`, mounted with
`AppBuilder::scoped` or `.layer()`, in an app with `[tenancy] enabled = true`
**Status:** fixed in `autumn/src/cache/layer.rs`

## 🎯 Surface

`CacheResponseLayer` stores whole `200 OK` `GET` responses and replays them
without running the handler. Before this fix the key was
`format!("http:{}", req.uri())` and nothing else.

## 🕵️ Threat model

> Against an app that turns on `[tenancy]` with the `subdomain` or `header`
> source and puts `CacheResponseLayer` over a route group whose page is the
> same for every visitor *of a tenant* (a tenant's public catalog or landing
> page), an attacker who is any visitor of tenant B can read tenant A's
> cached page. A tenant-B session is enough when the route is gated. The app
> author did nothing the documentation told them not to do.

The layer's contract says "only wrap responses that are the same for every
visitor". A per-tenant page meets that within a tenant, so the contract does
not warn the author off it.

Why the URI does not separate tenants:

- With `subdomain` tenancy only `Host` differs. An HTTP/1.1 request target is
  origin-form (`/catalog`), so `req.uri()` has no authority. The 2026-09-21
  ledger assumed "a URI-keyed cache is tenant-safe exactly when the tenant is
  part of the URI (subdomain/path tenancy)". That is wrong for subdomain
  tenancy.
- With `header` tenancy only a header differs.

On a hit the handler never runs, so neither the `Tenant` extractor nor any
`tenant_scoped` repository filter can stop the leak.

Every sibling cache already folds in `CURRENT_TENANT`: `#[cached]`
(2026-09-05), `cache_fragment` (2026-09-21) and the idempotency store
(2026-09-02). This layer was the one left.

## 🧪 Reproduction

`autumn/tests/integration/cache_response_tenant_scope.rs::response_cache_never_serves_one_tenant_to_another`

```
cargo test -p autumn-web --test integration_tests cache_response_tenant_scope
```

The test drives real requests through `TestApp`, with the layer mounted by
`AppBuilder::scoped("/shop", CacheResponseLayer::from_cache(..), routes![..])`.
It covers subdomain tenancy, header tenancy, a same-tenant repeat that must
hit, and an untenanted public path that must still hit. On trunk it fails at
the first cross-tenant request. See [`trunk-failure.txt`](trunk-failure.txt):

```
assertion `left == right` failed: subdomain tenancy: tenant B was served tenant A's cached response
  left: "catalog-of-tenant-a-sentinel"
 right: "catalog-of-tenant-b-sentinel"
```

## 🔎 Root cause

`autumn/src/cache/layer.rs`, `CacheResponseService::call`: the key was
`http:{uri}`. The tenancy middleware runs outside scoped groups and sets
`CURRENT_TENANT` for the request, but the layer never read it.

## 🩹 Fix

The key is now built by `response_key(tenant, uri)`:

- no tenant in scope: `http:<uri>`, unchanged, so apps without tenancy and
  tenancy public paths keep their warm entries;
- a tenant in scope: `http:tenant=<len>:<tenant>:<uri>`. The `tenant=`
  marker cannot start a URI, and the length prefix keeps a `:` in a tenant
  id from moving the boundary. An empty tenant id gets its own slot, apart
  from the untenanted one. This is the same shape as `cache_fragment`'s key.

The 512-byte stack buffer is kept.

The rustdoc also showed `#[intercept(CacheResponseLayer::from_cache(store))]`.
That form is dropped by the route macro and mounts no layer (see below), so
the examples now use `AppBuilder::scoped`.

## ✅ Verification

See [`after.txt`](after.txt). The test above now passes, along with the new
`response_key` unit tests, the existing `cache::layer` unit tests (unchanged)
and `cache_global_integration`.

Re-attack after the fix: header tenancy with an identical URI and query
(`/shop/catalog?page=2`) gets separate entries. A same-tenant repeat is still
a hit, so the cache was not turned off.

## 📡 Blast radius

- `#[cached]`, `cache_fragment*`, idempotency: already tenant-keyed
  (earlier ledgers). Rechecked.
- `static_gen` ISR: fails closed under tenancy (2026-09-04).
- `etag.rs`: computes validators per response and keeps no server-side store.
- `cache::get_or_compute*`, `cache::get` / `insert`: the app supplies the key.
  Not a framework key.
- Both backends (Moka, Redis via `from_app`) go through the same key, so one
  fix covers both.
- `path` tenancy: the tenant is in the URI, so it was already separated.

**Related, not fixed here (findings material):** `#[intercept(..)]` is parsed
as a `syn::Path` (`autumn-macros/src/parse.rs`, `extract_interceptors`), and a
call expression is dropped without an error. `docs/guide/middleware.md` warns
about this, so an app that writes it is holding it wrong. But the
framework's own `CacheResponseLayer` docs showed that form, and nothing
fails at compile time. For a layer whose job is to reject requests, that
fails open. Making it a compile error changes what the macro accepts, which
needs a maintainer decision.

## 📜 Compatibility

- No public signature change.
- Untenanted keys are unchanged. Tenanted apps get one cold miss per URI
  after upgrade, since their old `http:<uri>` entries are no longer read.
  That is the intended effect.
- Changelog fragment: `changelog.d/cache-response-tenant-key.md`.
