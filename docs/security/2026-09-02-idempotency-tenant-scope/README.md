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

### Re-attacked after the fix

* **Alias keys.** `DeferredIdempotencyCommit::add_session_alias` recomputes a
  storage key after the handler ran. It reuses the `StorageKeyContext`, whose
  tenant is captured once on the request path, so an alias cannot escape the
  tenant's namespace even if it is computed outside the middleware's scope.
* **Forcing a miss.** An attacker who varies the tenant to force a fresh miss
  only moves into the namespace they resolved to, where their request is a
  first use — the same position a different session already had.
* **Tenant ids containing the separator.** Every component is length-delimited
  inside the digest; `storage_key_tenant_component_is_length_delimited` pins
  that a tenant id carrying `:` cannot spell another slot.
* **Residual, by design:** a route listed in `[tenancy] public_paths` is
  exempted by the tenancy middleware *before* it scopes anything, so requests
  to it resolve no tenant and still share one namespace. That is the same
  statement the app made by declaring the path tenant-independent (login pages,
  static assets, health probes), and such a route has no tenant-scoped data to
  leak.

## 📜 Compatibility

No public signature changes (`build_storage_key` and `StorageKeyContext` are
private). Storage keys change only for requests that resolve a tenant, which is
the point. `## [Unreleased] → Security` in `CHANGELOG.md`.

## Follow-up: `[tenancy] source = "session"` alias staleness (same day, pre-merge)

Codex's automated review on PR #2447 caught a correctness gap in the fix
itself: `StorageKeyContext::tenant` is captured once, before the handler runs.
For `header`/`subdomain`/`jwt` tenancy that's always correct — those sources
never depend on session state a handler could mutate. For `source = "session"`
it is not: a handler that changes the session's tenancy key mid-request (an
organization switch, `examples/teams`'s `switch_organization` shape) leaves the
deferred replay alias keyed by the *pre-switch* tenant. A retry presenting the
same session (no id rotation required — `session.insert` alone is enough)
resolves the *post-switch* tenant, misses the alias, and re-runs the mutation.

This is a correctness regression (duplicate execution), not a cross-tenant
leak — the retry lands in its own new slot rather than reading someone else's
data — but it undermines the idempotency guarantee for a legitimate, session
first-class pattern, and it ships in the same PR as the fix above, so it was
closed before merge rather than filed as a separate issue.

**Reproduction:** `session_tenancy_alias_uses_finalized_tenant_after_switch` in
`autumn/tests/integration/idempotency_tenant_scope.rs`. Red run:
`session-alias-follow-up-red.txt` (`left: "switched-2"`, `right: "switched-1"`
— the retry re-executed the handler). Green run:
`session-alias-follow-up-green.txt`.

**Fix:** `SessionLayer` gains an internal (`pub(crate)`) `tenancy_session_key`,
set by `build_session_layer`'s caller in `router.rs` only when
`[tenancy] source == "session"`. When the session middleware finalizes a dirty
session, it reads the tenancy key live from the just-saved session data and
passes it to `add_deferred_session_replay_key` as a `tenant_override`, which
`DeferredIdempotencyCommit::add_session_alias` uses in place of the captured
tenant for that alias's storage key. The primary key for the request that
performed the switch is untouched (it correctly reflects the tenant the
*request itself* ran as); only the alias a future retry will compute is
corrected. No public API changed — `SessionLayer::new`'s signature is
unchanged, and the new field defaults to `None` (today's behavior) for every
other tenancy source and for apps not using session tenancy.
