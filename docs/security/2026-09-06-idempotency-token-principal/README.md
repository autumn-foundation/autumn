# 2026-09-06 — Idempotency × bearer-token principal (negative result)

## 🎯 Surface

`autumn_web::idempotency` (`IdempotencyLayer`, `AppBuilder::idempotent()`) ×
`autumn_web::auth::RequireApiToken` scoped service tokens. Entry point
investigated: a `#[secured(scopes = [...])]` route with no session and no
`[tenancy] enabled = true`, mounted behind `RequireApiToken` (the only way to
populate `ApiTokenScopes`/`ApiToken` before a handler runs), with
`.idempotent()` turned on.

## 🕵️ Threat model (hypothesis)

Against an app that authenticates a pure token-based API purely with
`#[secured(scopes = [...])]` (documented: "No session is required for a
scopes-only gate, so a pure service token authorizes on scopes alone") and
turns on `.idempotent()` (documented, first-class), an attacker holding their
own validly-issued API token (an ordinary token holder — e.g. a different
customer of the same B2B API, not an elevated principal) could read another
principal's stored mutation response by sending the same client-supplied
`Idempotency-Key` to the same route: `docs/guide/idempotency.md` promises
"Cached responses are scoped to a principal," but the storage-key builder
(`autumn::idempotency::build_storage_key` / `principal_scope_digest`) only
ever folds in the cookie-backed session id and the framework-resolved tenant
— never a bearer token's identity. With no session and no `[tenancy]`, two
different tokens compute byte-identical storage keys.

## 🧪 Reproduction attempt → negative result

Test: `autumn/tests/integration/idempotency_token_principal.rs::bearer_token_principals_do_not_replay_across_each_other`

```
cargo test -p autumn-web --test integration_tests --features test-support \
  bearer_token_principals_do_not_replay_across_each_other -- --nocapture
```

Result: **pass** — no leak. See `after.txt` for the full run.

Customer B's request does **not** receive customer A's cached body. Instead
it gets `409 Conflict` ("idempotency replay requires an inner replay stop for
this route") — a fail-closed rejection, not a replay.

## 🔎 Root cause of the fail-closed behavior

`RequireApiToken` is a Tower `Layer`, so an app can mount it three ways, each
protected by a different router branch:

1. **`AppBuilder::layer()`/`static_gate()`** (this ledger's reproduction). Any
   such registration not on `is_idempotency_transparent_app_layer`'s allowlist
   (today: only `SessionLayer` and the i18n bundle extension —
   `autumn/src/router.rs:2770-2780`) flips `opaque_app_layers_present` for the
   whole app (`autumn/src/router.rs:642-650`), which makes
   `idempotency_layer_for_route` (`autumn/src/router.rs:2741-2753`) select
   `IdempotencyLayer::fail_closed_on_replay()` for **every** `routes![]`-declared
   route in the app — including ones with no custom auth at all — instead of
   the normal `replay_through_inner()` path.
2. **A `.scoped(...)` group's own layer.** `mount_scoped_groups`
   (`autumn/src/router.rs:3117-3123`) selects the fail-closed `.manual` layer
   unconditionally for every route in a scoped group, regardless of
   `opaque_app_layers_present` — the group's layer is never inspected.
3. **Applied directly to a raw `axum::Router`** brought in via
   `AppBuilder::merge()`/`nest()` (`RequireApiToken`'s own doc example in
   `autumn/src/auth.rs` uses this form). `mount_raw_routers` selects the same
   `.manual` layer unconditionally, for the identical reason.

So no mounting path relies on the router recognizing `RequireApiToken`
specifically — (2) and (3) fail closed for *any* raw/scoped router regardless
of what layers it carries, and (1) fails closed because `RequireApiToken`
happens not to be on the allowlist a top-level `AppBuilder::layer()` is
checked against.

The storage key genuinely does not carry the bearer principal — the
hypothesis about the key itself is correct — but a same-key collision from a
different principal can never reach a stored response either way, because
one of the three branches above already forces fail-closed mode first. The
mechanism trades correctness for safety: a genuine same-customer retry
through `RequireApiToken` *also* gets `409` instead of the intended
idempotent replay (a reliability cost, not a leak) — which is exactly what
the doc comment at `autumn/src/idempotency.rs:134-140` calls out: "Opaque
route layers that resolve their own tenants, bearer principals, or policy
state must still use the fail-closed replay path instead of storage-key
partitioning."

## 🩹 Fix

None — no bug found. Regression test added at
`autumn/tests/integration/idempotency_token_principal.rs`, registered in
`autumn/tests/integration/mod.rs`. The test pins the actual mechanism
(`409`, not a leaked body) so it fails loudly — not just "vacuously passes
for some other reason" — if a future change ever whitelists
`RequireApiToken` (or another principal-resolving layer) as
"idempotency-transparent" without first teaching the storage key about its
principal.

## ✅ Verification

- `cargo fmt --all` — clean (no changes to the new file).
- `cargo test -p autumn-web --test integration_tests --features test-support bearer_token_principals_do_not_replay_across_each_other` — passes (`after.txt`).
- `cargo clippy -p autumn-web --test integration_tests --features test-support -- -D warnings` — clean.
- `./scripts/check-panic-gate.sh` — 35/35 self-tests pass, 64 request-path modules gated (unaffected by this test-only change).
- `./scripts/check-determinism-gate.sh` — passes, 18 modules gated (unaffected).
- `./scripts/pre-push-check.sh` — see PR for result; test-only change, no production code touched.

## 📡 Blast radius

All three mounting paths are covered above. Also checked
`is_idempotency_transparent_app_layer`'s allowlist directly: only
`SessionLayer` and the i18n bundle extension are on it, so no other
principal-resolving layer (custom OIDC/JWT middleware, a hand-rolled API-key
layer) reaching a top-level `AppBuilder::layer()` is exempted either — the
same fail-closed default applies uniformly.

## 📜 Compatibility

No behavior change, no CHANGELOG entry (test-only addition, matching this
repo's convention for negative-result commits — see #2505).

## 🗂 Ledger

This directory. `after.txt` has the full green test run.

## 🔁 Follow-up (2026-10-19): the path this ledger did not cover

The negative result holds for every way of mounting `RequireApiToken`. It did
not cover a route that checks the bearer credential **inside the handler**,
with no auth layer at all. Autumn Harvest's durable routes hit it (Harvest
`DESIGN-durable-routes.md`, R21): an ordinary `routes![]` route takes the
`replay_through_inner()` path, its storage key carried no principal for a
session-less request, and client B sending client A's `Idempotency-Key` and
body was served A's stored `2xx`. The handler never ran, so its own check of
the token never ran either. That *was* a cross-principal replay.

The fix teaches the storage key about the bearer principal, as the
regression test above required before any allowlist change:

- `BearerScope::Verified` — the principal `RequireApiToken` verified, read
  from its crate-private `ApiTokenPrincipal` extension (a client cannot
  forge it). It survives token rotation.
- `BearerScope::Header` — a SHA-256 digest of the raw `Authorization` header,
  pushed only when there is no verified principal and no cookie-backed
  session.

Both components are pushed only when present, so a session-only app keeps
byte-identical keys. With the principal in the key, `RequireApiToken`
registered directly through `AppBuilder::layer`/`static_gate` joined
`is_idempotency_transparent_app_layer`: a same-client retry now replays
instead of answering `409`. A `ServiceBuilder` stack that wraps it, a
`.scoped(...)` group and a merged router still fail closed.

`idempotency_token_principal.rs` now pins the new behaviour: each token
principal replays only its own response, a rotated token replays, a
handler-checked bearer is isolated by its header, and an `idempotent = false`
route is never cached. `test_authorization_header_does_not_split_idempotency_scope`
in `idempotency_middleware.rs`, which asserted that a request with *no*
credential replayed a bearer client's response, was reversed.
