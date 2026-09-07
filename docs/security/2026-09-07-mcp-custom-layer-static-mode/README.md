# 2026-09-07 — MCP `tools/call` dispatch skipped `AppBuilder::layer` custom layers in SSG/ISR mode

## 🎯 Surface

`autumn::mcp` `tools/call` dispatch (`serve_tools_call` → `McpServer::dispatch`)
× `autumn::router::try_build_router_with_static_inner`'s SSG/ISR (`dist`
manifest) path. Entry point: an MCP tool backed by a route whose only
protection is a global Tower layer registered via `AppBuilder::layer(...)`, in
an app that also serves from a prior `autumn build` (`dist/manifest.json`
present at boot).

## 🕵️ Threat model

Against an app that mounts a route, protects it with a path-scoped
`AppBuilder::layer(...)` Tower layer (a real, unrestricted, documented way to
add a runtime check — `docs/guide/middleware.md`: "Wrap every request |
`AppBuilder::layer(..)` | app | Cross-cutting concerns that genuinely apply
everywhere"), exposes the same handler as an MCP tool (`#[api_doc(mcp)]` or
the `expose_all` hatch), and separately runs the app in SSG/ISR mode (a `dist`
manifest present at boot, i.e. `autumn build` was run and the app now
serves from it) — an attacker holding **no credential at all** (not a session,
not an API token, nothing) could reach the handler by calling the MCP tool
instead of the HTTP route, while the identical direct HTTP request was
correctly rejected by the same layer.

The app author did nothing the docs warn against. `docs/guide/mcp.md` §6
promises, without a static-mode carve-out: "`tools/call` runs through the
**real handler pipeline** — the same in-process path Autumn's test client
uses. That means `#[secured]`, authorization, tenancy, rate limits, and
validation all apply identically to an agent call and an ordinary HTTP call.
There is no separate auth subsystem," and later: "it replays it through a
clone of the fully-assembled application router. Because it traverses the
same routes, layers, and middleware an external request would, security and
validation are *shared*, not re-implemented." The one documented exception is
narrower and names a different registration:
`docs/guide/middleware.md`'s `static_gate` section says the gate "is never
applied to MCP `tools/call` dispatch anyway" and explicitly redirects readers
to route-level guards — but says nothing about `AppBuilder::layer`, which the
same page's own comparison table lists as covering "every request."

## 🔎 Root cause

`autumn/src/router.rs`, `try_build_router_with_static_inner` (SSG/ISR path):
`ctx.custom_layers` (the `AppBuilder::layer(...)` registrations) is drained
out of `RouterContext` *before* `build_router_pre_state` runs, so it can be
reapplied to the live-serving router *outside* the static-first middleware
(so a layer can process pre-rendered responses too, e.g. compress cached
HTML — see `RouterContext::custom_layers`'s doc comment). But
`build_router_pre_state` is also where the MCP dispatch clone is captured
(`mcp_prepared` block) — and that capture happens using the router as it
looks *inside* that function, i.e. with `ctx.custom_layers` already empty.
So while a direct request to the live-serving router traverses the reapplied
layer, a `tools/call` replay dispatches against the pre-drain clone and never
sees it. In the fully-dynamic path (no `dist` manifest) `ctx.custom_layers`
is never drained — it is baked into the router via `apply_middleware` before
the same dispatch clone is taken — so parity held there; only the SSG/ISR
path diverged. The code carried this as a `// Known limitation` comment
(`autumn/src/router.rs`, next to the `mcp_prepared` clone) rather than a
deliberate, documented design decision like `static_gate`'s exclusion.

## 📡 Blast radius — what's affected vs. not

Swept every mechanism the docs recommend for gating an MCP-exposed route:

| Mechanism | Drained before the MCP clone in SSG/ISR mode? | Affected |
| --- | --- | --- |
| `AppBuilder::layer(...)` (global custom layer) | **Yes** (`ctx.custom_layers`) | **Yes — this finding** |
| `AppBuilder::static_gate(...)` | Yes, but deliberately excluded from MCP dispatch **in both modes** (own test: `static_gate_is_excluded_from_mcp_dispatch_in_dynamic_mode`) | No — documented, unrelated to this fix |
| `.scoped(path, RequireApiToken, routes![...])` (`docs/guide/mcp.md` §6, the recommended pattern) | No — mounted via `ctx.scoped_groups`, part of route composition inside `build_router_pre_state`, never drained | No |
| A sub-router's own `.layer(...)` merged/nested into the app (e.g. `docs/guide/tauri-mobile-offline-sync.md`'s `require_sync_auth`) | No — part of `ctx.merge_routers`/`ctx.nest_routers` | No |
| `#[secured]` / `#[step_up]` / `#[throttle]` (macro-generated guards) | No — compiled into the handler's own `FromRequestParts` gate, part of the route itself | No |
| Session auth | No — the session layer is part of `apply_middleware`, not drained | No |
| `AppBuilder::secure_mcp(layer)` (whole-endpoint gate) | No — applied directly to the MCP router mount, a separate mechanism (`endpoint_layer`) | No |
| i18n's `AmbientLocaleLayer` (also registered via the internal `custom_layers` slot) | No — explicitly partitioned back into `ctx.custom_layers` before the drain (`#[cfg(feature = "i18n")]` block) specifically so it keeps working | No (already handled) |

So the gap was narrow but real: **only** apps using bare
`AppBuilder::layer(...)` — not the documented `.scoped`/sub-router/`#[secured]`
patterns — for a check that matters on an MCP-exposed route, while also
running SSG/ISR, were exposed. No other feature gate changes this (the code
path doesn't depend on `redis`/`ws`/`mail`/`i18n`/etc.).

## 🧪 Reproduction

Test: `autumn/src/router.rs`,
`router::trusted_host_tests::custom_layer_protects_mcp_dispatch_in_static_mode`
(a `#[cfg(feature = "mcp")]` unit test in `router.rs`'s own test module,
following the file's existing `try_build_router_with_static_inner` +
`oneshot` convention — e.g. `static_gate_runs_before_cached_static_page` next
to it — rather than the consolidated `tests/integration/` binary, since it
needs the crate-internal `RouterContext`/`CustomLayerRegistration` types).

```
cargo test -p autumn-web --lib --features mcp,openapi,maud \
  router::trusted_host_tests::custom_layer_protects_mcp_dispatch_in_static_mode -- --nocapture
```

The test registers a `CustomLayerRegistration` (the runtime shape of
`AppBuilder::layer(...)`) that rejects any request to `/secret` lacking
`x-api-key: secret123`, but lets every other path — crucially `/mcp` itself —
through unconditionally (a realistic, ordinary shape for a global auth layer
scoped by path prefix). It builds a real `dist` dir with a valid but empty
manifest (so the app takes the SSG/ISR code path without any page actually
being cache-served), mounts the route with `expose_all` MCP exposure, and
sends two requests with no credential: a direct `GET /secret`, and a
`tools/call` for the same handler via `POST /mcp`.

**Failure output on trunk** (`trunk-failure.txt`): the direct request is
correctly rejected (`401`), but the `tools/call` replay succeeds — the
JSON-RPC result comes back `isError: false` with an empty (204-contract)
success body, i.e. the handler ran with no credential at all:

```
thread '...custom_layer_protects_mcp_dispatch_in_static_mode' panicked at autumn/src/router.rs:...:
assertion `left == right` failed: the custom AppBuilder::layer() gate that protects the direct route
must also protect the MCP tools/call replay in static/ISR mode — an unauthenticated tool call must
not reach the handler: {"id":1,"jsonrpc":"2.0","result":{"content":[{"text":"","type":"text"}],"isError":false}}
  left: Bool(false)
 right: true
```

## 🩹 Fix

`autumn/src/router.rs`:

- `build_router_pre_state` gains a new parameter, `mcp_dispatch_extra_layers:
  Vec<crate::app::CustomLayerRegistration>` — applied to the MCP dispatch
  clone alone (via the existing `apply_layers_in_registration_order` helper),
  never to the router the function returns.
- The fully-dynamic caller (`try_build_router_inner`) passes `Vec::new()`:
  `ctx.custom_layers` is never drained on that path, so it's already baked
  into the dispatch clone and this is a no-op — parity there is unchanged.
- The SSG/ISR caller (`try_build_router_with_static_inner`) now clones
  `custom_layers` right after computing it (the drained, i18n-partitioned
  set that gets reapplied outside the static-first middleware) and passes
  the clone through. The original, un-cloned `Vec` still gets applied to the
  live-serving router exactly as before — same position, same ordering, no
  double-application, no behavior change for direct requests.
- `crate::app::CustomLayerRegistration` gained `#[derive(Clone)]` (its
  `layer: ErasedAppLayer` is a `tower::util::BoxCloneSyncServiceLayer`,
  already `Clone`), so cloning the registration set is a cheap `Arc`-level
  clone, not a semantic change to what a layer does.

`static_gate` is untouched: its exclusion from MCP dispatch is deliberate and
already covered by its own tests in both modes, and the docs already tell
users to authenticate MCP tools with route-level guards instead of it.

## 🔁 Review round 2 (Codex, PR #2608)

Two automated findings landed on the first push. Both verified against the
running code before deciding what to do with them.

**P2 — "Avoid applying custom layers twice to each tool call" (confirmed,
fixed).** The first version of this fix merged `mcp_router` inside
`build_router_pre_state` exactly as before, with only the new
`mcp_dispatch_extra_layers` added on top for the dispatch clone. In SSG/ISR
mode that meant `/mcp` was still nested inside the router the caller's own
`custom_layers` reapplication wraps (`try_build_router_with_static_inner`,
"Custom (outside static middleware)") — so a single `tools/call` ran every
custom layer **twice**: once for the live `/mcp` POST (the envelope), once
for the dispatch replay. Verified empirically with a probe test (a
`from_fn` layer incrementing an `Arc<AtomicUsize>` counter): one `tools/call`
produced a count of `2` before the fix below, `1` after. Harmless for an
idempotent layer (security headers), but a real bug for a stateful one — a
rate limiter or quota counter would be charged twice per call, halving its
effective MCP throughput relative to direct HTTP; a one-time-use nonce/token
layer could reject the replay outright.

Fix: `build_router_pre_state` no longer merges `mcp_router` internally when
`defer_security_headers` is true (SSG/ISR mode) — it hands the router back
to the caller instead (return type becomes `(Router<AppState>,
Option<Router<AppState>>)`), and `try_build_router_with_static_inner` merges
it in *after* its `custom_layers` reapplication. The live `/mcp` envelope
now never traverses `custom_layers` at all in either mode — matching the
fully-dynamic path, where it structurally never did (see
`docs/guide/mcp.md`'s "why `/mcp` sits outside the global middleware
stack"). `mcp_router` is unaffected otherwise: it already carries its own
dedicated copies of security headers, rate-limit, timeout, CORS, etc.
(mirroring `apply_middleware`, per the existing comments in
`build_router_pre_state`), so where exactly it merges relative to
compression/shadow-mirroring/`static_gate`/security-headers doesn't matter —
only staying outside `custom_layers` does. Locked in by a new committed
regression test,
`custom_layer_runs_exactly_once_per_tools_call_in_static_mode`, asserting
the counter is exactly `1`.

**P1 — "Preserve custom authentication headers in the replay" (verified,
declined as out of scope).** Accurate: `mcp::build_request`'s
`FORWARDED_HEADERS` allowlist (`authorization`, `cookie`,
`idempotency-key`, `host`, `forwarded`, the `x-forwarded-*` family,
`x-real-ip`, `accept-language`) does not include arbitrary custom headers
like `x-api-key`, so a custom layer that authenticates on a header outside
that curated list will reject a `tools/call` replay even when the *live*
`/mcp` request carried a valid credential on that header — this PR's own
test only exercises the "no credential" side of that, so it doesn't
demonstrate the false-positive case Codex describes, but the mechanism is
real. It is **not**, however, a regression this PR introduces: `build_request`
is shared, unconditional code that already behaves identically for the
fully-dynamic path today, on trunk, with no relation to `dist` manifests or
`custom_layers`. It also fails in the safe direction — a legitimate call is
over-rejected, not an illegitimate one let through — so it does not reopen
the bypass this PR closes. Expanding `FORWARDED_HEADERS` is a deliberate,
curated security decision (see the header-by-header reasoning already in
that list and in `docs/guide/failure-capsules.md`'s "names are matched
exactly" precedent for the *same* kind of curation problem in capsule
redaction) that trades off forwarding more of a client's headers into an
internal replay against the risk of leaking or misusing ones the framework
doesn't intend to forward — not something to change opportunistically inside
an unrelated authn-bypass fix. Filed as a follow-up rather than folded into
this PR; replied on the review thread with this reasoning.

## ✅ Verification

- Repro test red on trunk (`trunk-failure.txt`), green after the fix
  (`after.txt`).
- `cargo test -p autumn-web --lib --features mcp,openapi,maud
  router::trusted_host_tests::` — 178 passed, including every `static_gate`
  test (`static_gate_runs_before_cached_static_page`,
  `static_gate_runs_in_dynamic_mode`,
  `static_gate_redirect_carries_security_headers_ssg`,
  `static_gate_redirect_carries_security_headers_dynamic`,
  `static_gate_layer_requires_fail_closed_idempotency`) unchanged.
- `cargo test -p autumn-web --lib --features mcp,openapi,maud,test-support`
  (full crate unit suite) — 5762 passed, 0 failed, 21 ignored.
- `cargo test -p autumn-web --test integration_tests --features
  test-support,mcp,openapi,maud mcp` — 84 passed, 0 failed (every MCP
  integration test: envelope auth, streaming, plugin exposure, schema
  derivation, structured query args, including
  `tools_call_enforces_bearer_token_via_real_pipeline` and
  `static_gate_is_excluded_from_mcp_dispatch_in_dynamic_mode`).
- `cargo check -p autumn-web --features sqlite` and `cargo clippy -p
  autumn-web --features sqlite --lib -- -D warnings` — clean. CI's
  `sqlite-runtime` job caught a real `clippy::needless_pass_by_value` on
  `mcp_dispatch_extra_layers` in the non-`mcp` build (the parameter is
  genuinely unused when the feature is off) — the per-parameter
  `#[cfg_attr(..., allow(unused_variables))]` pattern already used for `ctx`
  in this function does not extend to this lint (it reasons about the whole
  function body, not one binding), so the allow moved to the function
  itself, `#[cfg_attr(not(feature = "mcp"),
  allow(clippy::needless_pass_by_value))]`.
- `cargo check --workspace` — clean.
- `cargo fmt --all` — clean.
- `cargo clippy -p autumn-web --all-targets --features
  mcp,openapi,maud,test-support -- -D warnings` — clean. Two real hits along
  the way, both fixed: `too_many_lines` on the first new test (resolved with
  `#[allow(clippy::too_many_lines)]`, matching `build_router_pre_state`'s own
  existing allow for the same lint), and `items_after_statements` on the
  second new test (an `async fn` item declared after a `let` — moved the
  `fn` to the top of the test body). The `autumn-macros` `unknown_lints`
  warning present in these runs' logs is pre-existing on trunk
  (`clippy::unused_async_trait_impl` is not a real lint name on this
  toolchain) and unrelated to this change — confirmed by running the same
  clippy invocation against trunk via `git stash`.
- `./scripts/check-panic-gate.sh` — 35/35 self-tests, 67 request-path modules
  gated (unaffected; this change touches router assembly, not a
  request-path-panic-gated module).
- `./scripts/check-determinism-gate.sh` — 18 modules gated (floor 18),
  unaffected.
- `./scripts/check-semver.sh` — could not complete in this environment
  (installs `cargo-semver-checks` from scratch and then requires a pinned
  `1.94.1` toolchain not installed here). Manually verified instead: `git
  diff` shows no `pub fn`/`pub struct` signature changed — the only
  `pub fn`s touched (`try_build_router_inner`, called only by
  `try_build_router` which is unchanged; `try_build_router_with_static_inner`)
  keep their exact signatures, `build_router_pre_state` is a private `fn`,
  and `CustomLayerRegistration` is `pub(crate)` (never part of the public
  API). No semver-relevant surface changed.
- Re-attack: tried the same shape via `secure_mcp` (whole-endpoint gate,
  unaffected — separate `endpoint_layer` mechanism, not `custom_layers`),
  and via a `static_gate` layer instead of `AppBuilder::layer` (still
  excluded from MCP dispatch by design — its own tests already cover this
  and continue to pass).

## 📡 Variant sweep

Grepped for other `std::mem::take`/`drain`-before-the-MCP-clone patterns in
`try_build_router_with_static_inner`: only `custom_layers` and
`static_gate_layers` are extracted before `build_router_pre_state` runs.
`static_gate_layers`'s exclusion is deliberate (table above) and out of
scope. No other registration list is drained on this path. Not
feature-gate-dependent (doesn't touch `redis`/`ws`/`mail`/`i18n`/`sqlite`)
beyond the pre-existing `#[cfg(feature = "i18n")]` `AmbientLocaleLayer`
carve-out, which was already correct and is untouched by this fix.

## 📜 Compatibility

No public API signature changed (see Verification). Behavior change: an app
using `AppBuilder::layer(...)` in SSG/ISR mode will now find that layer
enforced on MCP `tools/call` dispatch too, where before it was silently
skipped. This can only make a previously-open path more restrictive — it
cannot break an app that was relying on the layer running (nothing could have
relied on the bypass without the app itself being the vulnerable case this
fix closes). Documented in `CHANGELOG.md` under `## [Unreleased]` →
`### Security`. No config default changed, no migration needed.

## 🗂 Ledger

- `trunk-failure.txt` — the red run (test asserting secure behavior, against
  the code before the fix).
- `after.txt` — the green run (same test, after the fix).
