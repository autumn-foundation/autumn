# Cross-tenant `#[cached]` read via a tenant-blind cache key (2026-09-05)

**Class:** cross-tenant read through an authenticated surface
**Surface:** `autumn-macros::cached` — `#[cached]`'s generated cache-key
construction
**Entry point:** any HTTP route (or any code path) that calls a `#[cached]`
function memoizing a `tenant_scoped` `#[repository]` read
**Affected:** `autumn-web` 0.7.0 and every earlier release that shipped
`#[cached]`
**Status:** fixed — `autumn-macros/src/cached.rs`

## 🕵️ Threat model

> Against an app that follows Autumn's own documented patterns — turns on
> multi-tenancy (`[tenancy] enabled = true`, per `docs/guide/tenant-cells.md`)
> and memoizes a `tenant_scoped` repository read with `#[cached]` (per
> `docs/guide/cache-coherence.md`, exactly as the framework's own SaaS starter
> does in `examples/saas/src/repositories.rs`) — an attacker who is an
> ordinary authenticated principal of tenant B can obtain **tenant A's cached
> response** from any such cached read, for the remainder of the entry's TTL,
> simply by calling the same route/function with the same *non-tenant*
> arguments tenant A used. The app author did nothing the documentation told
> them not to do: the SaaS starter's own `cached_project_count` is the
> *correct* pattern (thread `tenant_id` explicitly and name it in
> `key(tenant_id)`), but nothing in the macro, the build-time cache-coherence
> gate (`autumn cache audit`), or `autumn routes audit` detects or prevents
> its omission — and every *other* `tenant_scoped` operation in Autumn
> resolves the tenant from the ambient `CURRENT_TENANT` task-local
> automatically, with no explicit parameter required. A developer who caches
> a `tenant_scoped` read keyed on some other legitimate, real parameter
> (a page number, an export format, a filter) instead of the tenant has
> followed the framework's own ambient-scoping idiom everywhere else — and
> been bitten by the one place it silently doesn't apply.

## 🔎 Root cause

`autumn-macros/src/cached.rs`'s `generate_cache_body` builds the runtime
key as:

```rust
let __autumn_key = ::autumn_web::cache::make_cache_key(#id_expr, #key_args);
```

`#key_args` (from `key_tuple`) is built exclusively from the function's own
*explicit* parameters — every parameter by default, or exactly the
parameters named in `key(...)` when present (`autumn-macros/src/cached.rs`
`key_tuple`, ~line 489). It never reads `autumn_web::tenancy::CURRENT_TENANT`
— the task-local a `tenant_scoped` repository read filters by
(`autumn-macros/src/repository.rs` ~line 1913: `CURRENT_TENANT.try_with(...)`
inside every `tenant_scoped` finder). The build-time cache-coherence gate
(`autumn-cli/src/cache_audit.rs`) proves only *staleness* edges (a cached
read's declared/derived model dependencies vs. a repository's
`invalidates(...)` clause) — it has no notion of tenant-key correctness at
all, so a `#[cached(reads(TenantScopedModel))]` function with no
tenant-identifying key parameter compiles, passes `autumn cache audit`, and
runs.

## 🧪 Reproduction

Test: `autumn/tests/integration/cached_tenant_scope.rs` ::
`cached_tenant_scoped_read_leaks_across_tenants`

The test seeds two tenants' sentinel rows in a `tenant_scoped` model
(`CtsWidget`), mirrors the SaaS starter's `cached_project_count` shape but
keys the cached read on a legitimate non-tenant parameter (`format`,
mirroring the starter's own "keep the repository handle out of the key"
comment — just applied to the wrong parameter), and drives it through a real
HTTP route (`GET /widgets/summary`) via `TestApp`/header-based tenancy,
exactly as `docs/security/2026-09-02-idempotency-tenant-scope/` does for the
idempotency cache.

The committed test is `#[ignore = "requires Docker (testcontainers)"]`
(per `CLAUDE.md`'s Integration Test Layout Guidelines) and is picked up
automatically by CI's Docker-dependent test sweep — no workflow edit. This
sandbox has no Docker daemon, so the red/green runs below were captured with
an identical scratch harness (same fixtures, same assertions) pointed at a
natively-installed PostgreSQL 16 service on `127.0.0.1:5432` instead of a
testcontainer; the only difference from the committed test is the pool's
connection source. The fix itself was verified reverted-and-restored
in place (`git stash`) against the *same* running Postgres instance, so the
red and green runs below differ by exactly the `autumn-macros/src/cached.rs`
change and nothing else.

Command: `cargo test -p autumn-web --test <scratch> --features
"db,cache-moka,test-support" -- --nocapture`

Failure output on trunk (fix reverted): see `trunk-failure.txt` —

```
thread 'cached_tenant_scoped_read_leaks_across_tenants' panicked at ...:170:5:
tenant B received tenant A's cached #[cached] response body: "csv:tenant-a-sentinel"
(the cache key never carried the resolved tenant)
test cached_tenant_scoped_read_leaks_across_tenants ... FAILED
```

## 🩹 Fix

`autumn-macros/src/cached.rs`'s `generate_cache_body` now reads
`CURRENT_TENANT` unconditionally and folds it into the key alongside
whatever `key(...)` already names:

```rust
let __autumn_tenant_key_component: Option<String> =
    ::autumn_web::tenancy::CURRENT_TENANT.try_with(Clone::clone).ok().flatten();
let __autumn_key = ::autumn_web::cache::make_cache_key(
    #id_expr,
    &(__autumn_tenant_key_component, #key_args),
);
```

Fixed at the enforcing layer (the macro that generates every `#[cached]`
function's key), not at any one call site — every `#[cached]` function in
every downstream app gets the fix on upgrade, with no code change required.
`CURRENT_TENANT` resolves to `None` for apps that never enable tenancy and
for any call outside a tenancy-scoped request (a background job, a
scheduled sweep), so those keys are byte-identical to before.

## ✅ Verification

- `cached_tenant_scoped_read_leaks_across_tenants`: FAILED before the fix,
  PASSED after (see `trunk-failure.txt` / `after.txt`).
- `cargo test -p autumn-macros cached::` — all 40 existing `#[cached]`
  macro-expansion unit tests pass unchanged (key selection, coherence
  registration, fence/insert ordering, etc.) — the fix only adds a key
  component, it doesn't change any existing one.
- Re-attack: confirmed the *same*-tenant case still hits the cache
  (repeat call from tenant A within the TTL still serves tenant A's own
  cached value — the fix partitions by tenant, it doesn't disable caching).

## 📡 Blast radius

- **Every `#[cached]` function is the same shape** — there is exactly one
  code path that builds the runtime key
  (`generate_cache_body`/`key_tuple`), so this is a single fix point, not a
  per-call-site patch. Swept: no other function in `autumn-macros` or
  `autumn/src/cache/*` independently constructs a `#[cached]`-style key.
- **Feature gates:** `#[cached]`'s default backend is `cache-moka`; when a
  process-level shared backend is registered (`autumn-cache-redis`,
  `redis` feature) the same generated key is used as the store key, so the
  fix covers that backend identically — verified by reading
  `autumn/src/cache/mod.rs`'s `global_cache()` call site in the generated
  body, which sits *after* the key is built.
- **Cache coherence / `autumn cache audit`:** intentionally left alone —
  it proves staleness (missing `invalidates(...)`), a different property
  from tenant-key correctness, and adding a tenant-key lint there was
  considered but the macro-level fix is strictly stronger (it fixes the
  data race even for an app that never runs `autumn cache audit` in CI).
- **Fragment cache (`autumn/src/cache/fragment.rs`) and read-through
  (`autumn/src/cache/read_through.rs`)** build their keys from
  caller-supplied strings, not function-argument hashing — out of scope
  for this fix; a follow-up findings note may be warranted if either is
  found to compute its key without an app-supplied tenant discriminator,
  but neither is macro-generated the way `#[cached]` is, so an app calling
  them incorrectly is the "app held it wrong" shape, not this one.

## 📜 Compatibility

- No public macro *input* syntax changed (`key(...)`, `ttl`, `reads(...)`,
  etc. are unchanged) — per `STABILITY.md`, `#[cached]`'s generated code is
  explicitly not part of the semver-covered surface.
- Behavior change, called out in `CHANGELOG.md` under `## [Unreleased]` →
  `### Security`: a `#[cached]` function that intentionally serves one
  shared, cross-tenant value now partitions its cache per resolved tenant
  too, when called from within a tenant's request. This is a hit-rate
  regression for that (out-of-contract for `tenant_scoped` data) usage, not
  a correctness change — the computed value doesn't vary by tenant, so a
  cache miss just recomputes the same answer once per tenant instead of
  once globally.
- No config default changed; no migration required.

## 🗂 Ledger

- `trunk-failure.txt` — red run (fix reverted)
- `after.txt` — green run (fix applied)
