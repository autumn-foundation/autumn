# Cross-tenant idempotency replay (2026-09-02)

**Class:** cross-tenant read through an authenticated surface / silent write
suppression
**Surface:** `AppBuilder::idempotent()` × `[tenancy] enabled = true`
**Entry point:** any macro-generated mutating route (`#[post]` / `#[put]` /
`#[patch]` / `#[delete]`) over HTTP
**Affected:** `autumn-web` 0.7.0 and every earlier release that shipped the
idempotency middleware
**Status:** fixed — `autumn/src/idempotency.rs`

## 🕵️ Threat model

> Against an app that turns on Autumn's documented multi-tenancy (`[tenancy]
> enabled = true`, with `source = "header"`, `"subdomain"` or `"jwt"`) and
> Autumn's documented idempotency middleware (`AppBuilder::idempotent()`), an
> attacker who is an ordinary authenticated principal of **tenant B** can obtain
> the stored response body of a mutation **tenant A** performed on the same
> route — and have their own write silently suppressed — by sending the same
> `Idempotency-Key` and the same request body; the app author did nothing the
> documentation told them not to do.

Both halves are documented, first-class features that the guides tell an app to
turn on with one builder call each. Neither guide mentions that composing them
merges the two tenants' idempotency namespaces.

The attacker needs the victim's key and body. That is not a stretch in
practice: `Idempotency-Key` is commonly derived deterministically by client
libraries (a hash of the payload, an invoice/cart identifier), so two tenants
performing the *same* action with the *same* payload collide with no attacker
effort at all — and an attacker who wants the collision only has to guess the
convention rather than a secret.

## 🔎 Root cause

`autumn/src/idempotency.rs::build_storage_key` namespaced the cache slot by

* `method`,
* `target` (path **+ query**),
* a principal digest derived **only** from a cookie-backed session id,
* the client-supplied `Idempotency-Key`.

The tenant resolved by the framework's own `tenancy_middleware`
(`autumn/src/tenancy.rs`) was not a component, and for `header`, `subdomain`
and `jwt` tenancy it is not recoverable from `target` either — the tenant
header and the `Host` are deliberately excluded from the key (a
client-controlled header must not let a retry force a fresh miss). For a
token-authenticated API there is no cookie session, so the principal digest is
the same constant for every caller.

Every macro-generated route carries
`RouteIdempotency::ReplayThroughInner` (`autumn-macros/src/route.rs`), so a
cache hit is replayed *through* the route's own guards. Those guards check
roles and scopes; none of them checks tenant identity. The handler — and
therefore every `tenant_scoped` repository predicate inside it — never runs.

The router already recognises this exact hazard class for *app-supplied* tenant
resolution. `custom_layers_require_fail_closed_idempotency`
(`autumn/src/router.rs`) forces the fail-closed (409) idempotency layer as soon
as the app registers any opaque `AppBuilder::layer` / `static_gate` layer:

> "an auth/tenant layer in either slot must force fail-closed replay so a
> cached mutation can't be served to a different principal carrying the same
> Idempotency-Key"

and `autumn/tests/integration/idempotency_middleware.rs` asserts it
(`test_app_wide_generated_route_fails_closed_for_opaque_tenant_scope`). The
framework's *own* tenancy layer is not a custom layer, so that protection never
engaged for the documented way to do multi-tenancy. The same asymmetry shows up
in the unit tests: `a different cookie-backed session must not replay another
user's response` was asserted for sessions and never for tenants.

## 🧪 Reproduction

`autumn/tests/integration/idempotency_tenant_scope.rs`

```bash
cargo test -p autumn-web --test integration_tests idempotency_tenant_scope
```

On trunk (`trunk-failure.txt`):

```
tenant B received tenant A's cached response body: "order-for-tenant-a-sentinel" (status 200 OK)
```

`same_tenant_retry_still_replays` passes on trunk and after the fix — the
feature itself is intact in both directions.

## 🩹 Fix

`build_storage_key` gains a `tenant` component, captured once on the request
path from the `CURRENT_TENANT` task-local (`StorageKeyContext::from_parts`) so
the later alias keys land in the same namespace.

Keying on the *framework's resolution* rather than on the wire is what keeps
the existing "client-controlled headers must not force a fresh miss" property:
a retry from the same tenant reproduces the same component and still replays,
while a request that resolved elsewhere gets its own slot.

The component is pushed **only when a tenant was resolved**, so an app without
tenancy computes byte-identical keys to before — no cache-wide invalidation on
upgrade, and no chance of a mutation retried across a deploy executing twice.

## 📡 Blast radius

* Both idempotency backends (`memory`, `redis`) go through the one
  `build_storage_key`, so one fix covers both.
* MCP `tools/call` forwards `idempotency-key` and dispatches through the
  fully-assembled router, so the dispatched request now carries the tenant too.
* Checked and **not** affected: `cache::fragment` and the `#[cached]` macro key
  on app-supplied identities; `cache::layer::CacheResponseLayer` keys on the URI
  alone but documents that in its own rustdoc as the whole contract; rate
  limiting, job tracking and session storage already key on a principal.
* `RouteIdempotency::Direct` routes (raw `merge`/`nest` routers, `scoped`
  groups, `#[intercept]`ed routes) were already fail-closed and are unchanged.

## 📜 Compatibility

No public signature changes (`build_storage_key` and `StorageKeyContext` are
private). Storage keys change only for requests that resolve a tenant, which is
the point. `## [Unreleased] → Security` in `CHANGELOG.md`.
