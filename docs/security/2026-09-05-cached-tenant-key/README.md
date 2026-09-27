# Cross-tenant `#[cached]` read (2026-09-05)

**Class:** cross-tenant read through an authenticated surface, via a
framework cache key missing the tenant
**Surface:** `autumn_web::cached` (`autumn-macros::cached::cached_macro`) ×
`#[repository(..., tenant_scoped)]`
**Entry point:** any HTTP route (or other caller) invoking a `#[cached]`
function that memoizes a `tenant_scoped` repository read
**Affected:** `autumn-web` 0.7.0 and every earlier release that shipped
`#[cached]`
**Status:** fixed — `autumn-macros/src/cached.rs`

## 🕵️ Threat model

> Against an app that follows Autumn's own documented patterns — turns on
> multi-tenancy (`[tenancy] enabled = true`) and memoizes a `tenant_scoped`
> repository read with `#[cached]`, exactly as the framework's own SaaS
> starter does in `examples/saas/src/repositories.rs` — an attacker who is an
> ordinary authenticated principal of tenant B can obtain **tenant A's cached
> response** from any such cached read, for the remainder of the entry's TTL,
> simply by calling the same route/function with the same *non-tenant*
> arguments tenant A used; the app author did nothing the documentation told
> them not to do.

The SaaS starter's own `cached_project_count` is the framework's *documented
correct* pattern: thread `tenant_id` explicitly through the function
signature and name it in `key(tenant_id)`. But nothing enforces that pattern.
Every *other* `tenant_scoped` operation in Autumn resolves the tenant from
the ambient `CURRENT_TENANT` task-local automatically, with **no** explicit
parameter required — `tenant_scoped` finders, `save()`, preload, retention
sweeps, all of it. `#[cached]` is the one place that idiom silently doesn't
apply: its generated cache key is built purely from the function's own
explicit arguments. A developer who caches a `tenant_scoped` read keyed on
some other real, legitimate parameter (a page number, an export format, a
filter) instead of the tenant has followed the framework's ambient-scoping
idiom everywhere else and been bitten by the one place it silently doesn't
hold — not "held it wrong," a framework default that is unsafe by
composition.

## 🧪 Reproduction

Test: `autumn/tests/integration/cached_tenant_scope.rs::cached_tenant_scoped_read_leaks_across_tenants`

Seeds two tenants' sentinel rows (`tenant-a-sentinel`, `tenant-b-sentinel`)
in a `tenant_scoped` `#[repository]` model, defines `cached_widget_labels` —
the buggy-but-natural sibling of the SaaS starter's `cached_project_count`,
keyed on a real non-tenant parameter (`format`) instead of the tenant — and
drives it through a real HTTP route (`GET /widgets/summary`) via `TestApp` +
header-based tenancy, mirroring the convention already established in
`docs/security/2026-09-02-idempotency-tenant-scope/`.

Committed as `#[ignore = "requires Docker (testcontainers)"]` per
`CLAUDE.md`'s Integration Test Layout Guidelines — CI's Docker sweep picks it
up automatically, no workflow edit needed. This sandbox had no Docker daemon,
so the red/green runs below were captured with an identical scratch harness
(`autumn/tests/warden_scratch_cached_tenant.rs`, not committed) pointed at a
natively-installed PostgreSQL 16 service instead of a testcontainer — only
the pool's connection source differs, and the fix commit
(`autumn-macros/src/cached.rs`) was reverted via `git stash` and restored
against the *same* running Postgres, so the two runs differ by exactly that
one file.

See `trunk-failure.txt` (fix reverted, FAILED) and `after.txt` (fix
restored, ok — plus the pre-existing `#[cached]` macro unit tests and the
cache-coherence integration suite, both unaffected).

## 🔎 Root cause

`autumn-macros/src/cached.rs::generate_cache_body` built the key as:

```rust
let __autumn_key = ::autumn_web::cache::make_cache_key(#id_expr, #key_args);
```

where `#key_args` (from `key_tuple`, ~line 489) is exclusively the
function's own explicit parameters — every parameter by default, or exactly
the parameters named in `key(...)`. It never read
`autumn_web::tenancy::CURRENT_TENANT`, the task-local every `tenant_scoped`
repository finder filters by (`autumn-macros/src/repository.rs` ~line 1913,
`::autumn_web::tenancy::CURRENT_TENANT.try_with(|t| t.clone())`).

The build-time cache-coherence gate (`autumn-cli/src/cache_audit.rs`,
`autumn cache audit`) proves only *staleness* — that a cached read's declared
or derived model dependencies are covered by a repository's
`invalidates(...)` clause. It has no notion of tenant-key correctness, so a
`#[cached(reads(TenantScopedModel))]` function with no tenant-identifying key
parameter compiles, passes the audit, and runs.

## 🩹 Fix

`generate_cache_body` now reads `CURRENT_TENANT` unconditionally and folds it
into the key alongside whatever `key(...)` already names:

```rust
let __autumn_tenant_key_component: Option<String> =
    CURRENT_TENANT.try_with(Clone::clone).ok().flatten();
let __autumn_key = make_cache_key(
    #id_expr,
    &(__autumn_tenant_key_component, #key_args),
);
```

Fixed at the enforcing layer — the macro that generates every `#[cached]`
function's key — not at the one call site this test names, so every
downstream app is covered on upgrade with no code change. `CURRENT_TENANT`
resolves to `None` for apps without tenancy enabled and for any call outside
a request (a background job, a scheduled sweep), so those keys are
unchanged.

## ✅ Verification

- Reproduction test: FAILED before the fix (`trunk-failure.txt`), PASSED
  after (`after.txt`).
- `cargo test -p autumn-macros cached::` — all 40 pre-existing `#[cached]`
  macro-expansion unit tests pass unchanged.
- `cargo test -p autumn-web --test integration_tests --features
  "db,cache-moka,test-support" cache_coherence` — 16 passed, 0 failed; the
  new test module compiles cleanly into the full 1934-test consolidated
  binary alongside the existing cache-coherence suite.
- `cargo clippy -p autumn-macros --lib -- -D warnings` and
  `cargo fmt --all` — clean.
- Re-attack: confirmed a same-tenant repeat call still hits the cache within
  the TTL (the fix partitions by tenant; it does not disable caching).

## 📡 Blast radius

- Single fix point: every `#[cached]` function shares the same
  key-construction code path (`generate_cache_body` / `key_tuple`) — no other
  site independently builds a `#[cached]`-style key.
- Applies identically to both cache backends: the key is built before
  `global_cache()` backend selection, so the Moka default and the
  `autumn-cache-redis` shared backend both use the fixed key.
- `autumn cache audit` / the cache-coherence gate is intentionally untouched
  — it proves a different property (staleness, not tenant isolation); the
  macro-level fix is strictly stronger since it protects apps that never run
  the audit at all.
- Checked and out of scope: `autumn/src/cache/fragment.rs` and
  `cache/read_through.rs` build keys from caller-supplied strings, not
  macro-generated argument hashing — an app misusing those directly by hand
  is the "held it wrong, no gate" shape, not this one.
- Affects every released `autumn-web` version that shipped `#[cached]`.

## 📜 Compatibility

- No macro *input* syntax changed; per `STABILITY.md`, `#[cached]`'s
  generated code is explicitly not SemVer-covered.
- Behavior change, recorded in `CHANGELOG.md` under `## [Unreleased]` →
  `### Security`: a `#[cached]` function intentionally serving one shared,
  cross-tenant value (a genuinely global computation, not a `tenant_scoped`
  read) now partitions its cache per resolved tenant too when called from
  within a tenant's request — a hit-rate regression for that out-of-contract
  usage, not a correctness change.
- No config default changed; no migration required.
