# 2026-09-21 — `cache_fragment_global`'s key never carries the tenant

**Class:** cross-tenant read through an authenticated surface, via a
framework cache key missing the tenant
**Surface:** `autumn_web::cache::{cache_fragment, cache_fragment_global,
cache_fragment_in, cache_fragment_global_in}` (`autumn/src/cache/fragment.rs`)
**Entry point:** any HTTP route rendering a Maud fragment cached by
`(identity, version)`
**Affected:** `autumn-web` every release that shipped fragment caching
(#1040) through 0.7.0
**Status:** fixed — `autumn/src/cache/fragment.rs`

## 🎯 Surface

`cache_fragment_global` (and its non-global and namespaced siblings) cache a
rendered Maud fragment in the **process-global** cache backend
(`cache-coherence.md`'s own words: "shared across replicas"), keyed by a
caller-supplied `(identity, version)` pair. `docs/guide/fragment-caching.md`
is the entry point that matters here: it is the framework's own worked
example of how to call this helper from inside an ordinary handler.

## 🕵️ Threat model

> Against an app that follows Autumn's own documented patterns — turns on
> multi-tenancy (`[tenancy] enabled = true`), shards a `tenant_scoped`
> repository (`#[repository(tenant_scoped, sharded)]`, `docs/guide/sharding.md`),
> and caches a record's rendered card with `cache_fragment_global` exactly as
> `docs/guide/fragment-caching.md`'s own Quick Start shows
> (`format_args!("post_card:{}", post.id)` as the identity, `post.lock_version`
> — `docs/guide/conditional-get.md`'s documented idiomatic version token — as
> the version) — an attacker who is an ordinary authenticated principal of
> tenant B can obtain **tenant A's cached fragment**, including whatever
> record content it rendered, simply by being the second tenant (of any two)
> to create their first row of that model. The app author did nothing the
> documentation told them not to do.

Two independently documented Autumn facts compose into a deterministic
collision, not a probabilistic one:

1. `docs/guide/sharding.md`'s resharding runbook says outright that a sharded
   table's primary key is a **shard-local `BIGSERIAL`**: "the shard-local
   `BIGSERIAL` id is not copied, so re-runs never collide on the primary key"
   (of the *destination* shard, when moving data between shards) — i.e. every
   shard hands out its own `1, 2, 3, …` independently. Two different tenants'
   first row of the same model, sharded onto different physical shards, both
   get `id = 1`.
2. `docs/guide/conditional-get.md`'s "Integration with `#[lock_version]`"
   section promotes `post.lock_version` as the idiomatic per-record version
   token ("the idiomatic combo when a model has both" a timestamp and an
   optimistic-lock counter) — an alternative `fragment-caching.md` itself
   invites ("a version-history sequence number") to the microsecond
   timestamp shown in its own Quick Start. A freshly inserted row's
   `#[lock_version]` starts at the same initial value for every tenant,
   deterministically.

So two tenants' first-ever row of a `#[cached]`-adjacent, fragment-cached
model can resolve to the exact same `(identity, version)` pair — no timing,
no guessing, no brute force — the instant both rows exist.

`cache_fragment_global`'s key is built *only* from that pair. It never
consults the ambient `CURRENT_TENANT` task-local the way every other
tenant-scoped Autumn primitive does: `tenant_scoped` repository finders,
`save()`, preload, retention sweeps, and — since Warden's 2026-09-05 fix
(`docs/security/2026-09-05-cached-tenant-key/`) — `#[cached]` itself, whose
generated key now folds `CURRENT_TENANT` in unconditionally. Fragment caching
is the one primitive left where that ambient-scoping idiom silently doesn't
apply, and nothing in `fragment-caching.md` warns that a record's own primary
key is not a safe fragment identity once sharding is in play — the "Important"
callout in "Choosing the identity and version" only warns about *stale*
viewer-dependent variants (a vote state), never about cross-tenant collision.

## 🧪 Reproduction

Test: `autumn/tests/cache_fragment_tenant_scope.rs::fragment_cache_leaks_across_tenants_on_first_row_collision`

```
cargo test -p autumn-web --test cache_fragment_tenant_scope -- --nocapture
```

Isolated as its own `[[test]]` binary (`autumn/Cargo.toml`), not part of the
consolidated `integration_tests` binary — `cache_fragment_global` reads and
writes the **process-global** cache backend, which `TestApp::build` clears
unconditionally and which a concurrently running consolidated-binary test
could otherwise stomp on. Same reason `cache_global_integration` and
`cached_global_backend` are isolated (CLAUDE.md, "Integration Test Layout
Guidelines" § Isolated tests, "Has process-wide side effects").

The test does not need a real sharded Postgres cluster: the vulnerable code
path is entirely inside `cache_fragment_global`'s key construction, which is
agnostic to *how* the caller's `id` and `lock_version` arrived at the same
values — only that they did. Two synthetic "first post" records (one per
tenant), each `id = 1, lock_version = 1`, reproduce exactly the state a real
shard-local `BIGSERIAL` + a fresh `#[lock_version]` row produce on their
respective shards. A real HTTP route (`GET /post-card`), header-based
tenancy (`x-tenant-id`, mirroring `docs/security/2026-09-05-cached-tenant-key/`'s
own convention), and `TestApp`/`TestClient` drive the request — no private
helper is called directly.

**Trunk (before the fix) — red**, captured with `git stash` isolating the fix
commit from the test commit and running against the same working tree (see
`trunk-failure.txt` for the full run):

```
thread 'fragment_cache_leaks_across_tenants_on_first_row_collision' panicked at autumn/tests/cache_fragment_tenant_scope.rs:171:5:
tenant B received tenant A's cached fragment: "<p>tenant-a-private-sentinel</p>" (cache_fragment_global's key never carried the resolved tenant, and both tenants' first row is id=1, lock_version=1)
test fragment_cache_leaks_across_tenants_on_first_row_collision ... FAILED

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.15s
```

**After the fix — green** (see `after.txt`):

```
running 1 test
test fragment_cache_leaks_across_tenants_on_first_row_collision ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
```

## 🔎 Root cause

`autumn/src/cache/fragment.rs`, `cache_fragment` (pre-fix):

```rust
let identity = identity.to_string();
let key = format!("fragment:{}:{identity}:{version}", identity.len());
```

and `cache_fragment_in`:

```rust
let identity = identity.to_string();
let key = format!(
    "{namespace}:fragment:{}:{identity}:{version}",
    identity.len()
);
```

Neither reads `crate::tenancy::CURRENT_TENANT`. Both `cache_fragment_global`
and `cache_fragment_global_in` call straight through to these, against
`super::global_cache()` — the same backend shared across every tenant's
requests in the process (and across replicas, for a Redis-backed
installation). Nothing about the existing control (Maud's escaping, the
tenancy middleware that resolves `Tenant`, the `#[repository(tenant_scoped)]`
filter on the underlying query that *produced* `post`) reaches this cache
key: by the time a handler calls `cache_fragment_global`, the tenant-scoped
read is already done, and the *cache key* itself is a separate, unscoped
namespace the handler's own tenant-correct query has no say over.

## 🩹 Fix

`autumn/src/cache/fragment.rs`: `tenant_key_component()` reads
`crate::tenancy::CURRENT_TENANT.try_with(Clone::clone)` and returns
`Option<String>` — preserving the task-local's own `None`/`Some`
discriminant rather than collapsing it to a string. A new `fragment_key()`
builds the key from that `Option`:

```rust
fn fragment_key(prefix: &str, tenant: Option<&str>, identity: &str, version: impl Display) -> String {
    match tenant {
        Some(tenant) => format!(
            "{prefix}tenant={}:{tenant}:{}:{identity}:{version}",
            tenant.len(), identity.len()
        ),
        None => format!("{prefix}{}:{identity}:{version}", identity.len()),
    }
}
```

Two review-round fixes are load-bearing here (Codex, PR #2884):

- **`None` builds the exact pre-fix key**, with no tenant segment at all.
  An earlier draft unconditionally inserted a tenant segment (even an empty
  one) for every caller, which would have changed the key for *every*
  existing single-tenant/non-request cache entry on upgrade — a silent,
  total cold-cache for a permanent (`ttl = None`) Redis-backed installation,
  with no way to reclaim the orphaned old keys. `None` now reaches the
  `format!("{prefix}{}:{identity}:{version}", identity.len())` arm
  unchanged.
- **`Some(tenant)` — including an empty string — cannot alias `None`.** The
  literal `tenant=` marker is non-numeric, so it can never collide with the
  `None` key's leading decimal identity-length; and the tenant itself is
  length-prefixed, so `Some("")` and `Some("x")` (and `None`) all land in
  distinct key spaces. An earlier draft collapsed `CURRENT_TENANT`'s `Option`
  to a plain string via `.flatten().unwrap_or_default()` before building the
  key, which made a resolved-but-empty tenant id (reachable through the
  public `with_tenant`) compute the *same* component as no tenant context at
  all.

`cache_fragment`/`cache_fragment_in` both call `fragment_key`;
`cache_fragment_global`/`cache_fragment_global_in` are unchanged — they
already call through to the fixed functions, so the fix covers all four
public entry points. In `cache_fragment_in` the tenant segment sits *after*
the `{namespace}:` prefix, so `invalidate_namespace`'s `SCAN
MATCH "{namespace}:*"` (Redis) / prefix match (`MokaCache`) is unaffected —
namespace-based invalidation still reaches every tenant's entries for that
namespace.

This mirrors the 2026-09-05 fix to `#[cached]` (`autumn-macros/src/cached.rs`):
fold the ambient `CURRENT_TENANT` in at the primitive, so every caller gets
tenant isolation for free rather than needing to remember to thread a
tenant discriminator through `identity` by hand — and, like that fix,
preserve `Option`'s own discriminant rather than lossily stringifying it.

Two unit tests pin both review-round properties directly:
`no_tenant_context_reads_a_pre_fix_legacy_key` (hand-writes the exact
pre-fix key and confirms a post-fix, no-tenant-context call still hits it)
and `empty_string_tenant_does_not_alias_no_tenant_context` (an empty
resolved tenant and no tenant context land in distinct cache entries).

## ✅ Verification

```
cargo fmt --all
cargo test -p autumn-web --test cache_fragment_tenant_scope -- --nocapture   # new regression test, green
cargo test -p autumn-web --lib cache::fragment                                # unit tests, unaffected (no request-scoped tenant in scope)
cargo test -p autumn-web --test integration_tests --features test-support fragment_cache_integration -- --nocapture  # pre-existing fragment-cache integration tests, unaffected
cargo clippy -p autumn-web --all-targets -- -D warnings
./scripts/pre-push-check.sh
./scripts/check-changelog-fragments.sh
```

Existing `fragment_cache_integration.rs` tests call `cache_fragment` directly
from plain `#[test]` functions with no tenancy-resolved request scope
active, so `CURRENT_TENANT` resolves to `None` for every call in that file —
the tenant component is the same empty prefix on every key, exactly as
before the fix. Behavior for single-tenant apps, and for any caller outside a
request (a background job, a scheduled sweep), is unchanged.

Re-attack after the fix: the same collision (`id=1, lock_version=1` on both
tenants' first row) now produces two **different** cache keys (the tenant
segment differs), so the second request is a genuine miss and renders its
own content — confirmed by the green run above.

## 📡 Blast radius

- `cache_fragment` / `cache_fragment_in` — fixed directly.
- `cache_fragment_global` / `cache_fragment_global_in` — fixed transitively
  (call through to the above; no separate key construction).
- `CacheResponseLayer` (`autumn/src/cache/layer.rs`) keys whole-response
  caching purely by URI (`format!("http:{}", req.uri())`), with the same
  absence of a tenant component. **Not additionally fixed here** —
  `fragment-caching.md` itself already discloses this, rather than silently
  claiming coverage: "Whole-response caching is **not** covered: a
  `CacheResponseLayer` entry is keyed by URI with no annotated item naming
  what it was derived from, and the coherence manifest lists it under
  `excluded` rather than pretending otherwise." A URI-keyed cache is
  tenant-safe exactly when the tenant is part of the URI (subdomain/path
  tenancy) and tenant-unsafe when it is header/session/JWT-resolved — that is
  a materially different, already-flagged risk shape from this issue's
  "the documented Quick Start is unsafe with no warning," and is better
  suited to its own investigation rather than folded into this fix.
- `#[cached]` (`autumn-macros/src/cached.rs`) — already fixed 2026-09-05;
  confirmed still folding `CURRENT_TENANT` in (`autumn-macros/src/cached.rs`
  around the `__autumn_tenant_key_component` binding).
- **Known limitation, NOT fixed here — `CURRENT_TENANT` is never scoped on
  `tenancy.public_paths` routes, even when a handler independently resolves a
  real per-request tenant** (Codex review, PR #2884). `tenancy_middleware`
  early-returns for `is_public_path` routes (`tenancy.rs:627-628`) without
  ever calling `CURRENT_TENANT.scope(...)`, while the `Tenant` extractor's
  fallback (`tenancy.rs:44-50`) independently re-resolves and returns a real
  tenant from headers/domain on exactly such a route — `is_public_path`'s own
  doc comment states the contract explicitly: "exempt from tenant
  resolution," not just exempt from *requiring* one. A handler on a public
  path that extracts `Tenant` and caches tenant-varying content (a
  tenant-branded public storefront, a public pricing page) would still
  collide across tenants after this fix, because every `CURRENT_TENANT`-only
  primitive — this one and `#[cached]`'s identical
  `__autumn_tenant_key_component` — sees `None` there regardless. This
  predates this PR (confirmed present, unfixed, in `#[cached]`'s own
  2026-09-05 fix) and is not specific to fragment caching: it is a
  framework-wide gap in every `CURRENT_TENANT`-ambient primitive. Closing it
  means changing what "public path" means in `tenancy_middleware` —
  best-effort resolving and scoping `CURRENT_TENANT` even on a path that
  doesn't reject a missing tenant — which touches every request through a
  public path (health probes, static assets, the login page) and is a
  materially larger, more architecturally significant change than this PR's
  key fix. Flagged for a maintainer decision on prioritizing a dedicated
  `tenancy_middleware` fix; not folded into this PR.
- `get_or_compute` / `get_or_compute_with` (`autumn/src/cache/read_through.rs`)
  — take a caller-supplied key directly (no macro/helper builds it), the same
  "raw primitive, caller's responsibility" shape as `cache_fragment`'s
  pre-fix state. Grepped every direct caller outside the `#[cached]` macro
  expansion: `autumn-cache-redis/src/lib.rs` (its own doctests/unit tests) and
  `autumn/src/actuator.rs` (a metrics-counter unit test) — no production
  request-path caller in `autumn/src` or the shipped plugins. Not in scope
  for this fix (no demonstrated documented-usage vulnerability —
  `cache-stampede.md` does not show a tenant-scoped example), but worth a
  human look if a future guide starts recommending it for tenant-scoped
  reads.
- Feature gates: `cache_fragment`/`cache_fragment_global` are behind `maud`
  (default-on) and read `crate::tenancy::CURRENT_TENANT`, which compiles
  unconditionally (`autumn/src/tenancy.rs` has no feature gate) — the fix
  applies identically whether or not `[tenancy]` is enabled at runtime
  (`CURRENT_TENANT` is simply always `None` when it isn't).

## 📜 Compatibility

Non-breaking: no public signature changed. Behavior changes only in that a
fragment rendered from within a tenant-resolved request now partitions its
cache entry per tenant. An app whose fragment content is tenant-invariant
(a genuinely global widget, not a `tenant_scoped` record) sees a harmless
drop in cache hit rate when that fragment happens to be rendered from inside
a tenant's request — never a correctness change, since the rendered markup
for such a fragment does not vary by tenant. Changelog fragment:
`changelog.d/fragment-cache-tenant-key.md` (`### Security`). No migration
guide needed (no API change, `scripts/check-migration-guides.sh` has nothing
to gate).

## 🗂 Ledger

This directory. `trunk-failure.txt` — red run on trunk. `after.txt` — green
run with the fix. No `queries.txt` (no SQL involved — this is a pure
in-process/Redis cache-key bug) or `manifest-*.json` (no route
classification involved).
