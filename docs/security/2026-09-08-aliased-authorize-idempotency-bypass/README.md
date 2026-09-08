# Aliased `#[authorize]` lets idempotency replay skip the policy re-check (2026-09-08)

**Class:** privilege escalation / authorization bypass via a macro that
silently fails to detect itself under a documented, ordinary Rust import
alias
**Surface:** `autumn_macros::idempotency_guard::has_pending_authorize_attr`
and `autumn_macros::route::has_authorize_guard` × `#[authorize]` ×
`AppBuilder::idempotent()`
**Entry point:** any macro-generated mutating route (`#[post]`/`#[put]`/
`#[patch]`/`#[delete]`) guarded by `#[authorize(...)]`, on an app that calls
`.idempotent()`, where the handler reaches `#[authorize]` through
`use ::autumn_web::authorize as <alias>;` instead of the literal
`#[autumn_web::authorize(...)]`/`#[authorize(...)]` spelling
**Affected:** `autumn-web` 0.7.0 and every earlier release that shipped
`#[authorize]` together with `.idempotent()`
**Status:** fixed — `autumn-macros/src/authorize.rs`,
`autumn-macros/src/idempotency_guard.rs`, `autumn-macros/src/route.rs`,
`autumn-macros/src/api_doc.rs`

## 🎯 Surface

`#[authorize]` is Autumn's record-level authorization macro: it runs as the
first statement inside the handler body (`autumn_macros::authorize::authorize_macro`),
re-checking the registered `Policy` on every request. Two other pieces of the
framework need to know, at macro-expansion time, whether a given handler has
`#[authorize]` on it, before `#[authorize]` itself has necessarily expanded:

- `idempotency_guard::should_own_replay` (used by `#[secured]`/`#[step_up]`/
  `#[throttle]`, issue #1668's pre-body `FromRequestParts` gates) — decides
  whether an earlier-running gate should also serve a cached idempotency
  replay itself, or defer to `#[authorize]`'s own in-body replay check.
- `route::has_authorize_guard` — decides whether the route macro keeps the
  standalone `IdempotencyReplayLayer` Tower middleware on the route at all,
  or suppresses it because some other guard (a gate, or `#[authorize]`'s own
  body-level check) already owns replay-serving.

Both did this by comparing the last path segment of each still-unexpanded
attribute on the handler against the literal string `"authorize"`.

## 🕵️ Threat model

Against an app that follows Autumn's own documented `#[authorize]` +
`AppBuilder::idempotent()` pattern (`docs/guide/*`, and this repo's own
`authorization_integration.rs` test suite) — and does nothing more unusual
than importing the macro under a local name, e.g.
`use autumn_web::authorize as authorize_note;`, entirely ordinary Rust with
nothing in Autumn's docs telling an app author not to do it — an attacker
who is an **ordinary authenticated principal whose authorization has since
been narrowed or revoked** (a role downgrade, a resource-ownership
transfer, a policy update) can keep obtaining the **stale "allowed"
response** from before the change, indefinitely, by replaying the exact
request (same body, same `Idempotency-Key`) that succeeded while they were
still authorized. The app author did nothing wrong: they used `#[authorize]`
and `.idempotent()` exactly as documented, under an import alias the
language itself supports.

**Mechanism:** a proc-macro attribute is only ever handed the tokens of the
item it annotates — never the enclosing module's `use` declarations — so
neither `has_pending_authorize_attr` nor `has_authorize_guard` can resolve
an aliased attribute name back to `"authorize"`. Concretely, for
`#[post] #[authz("update", resource = Post)] async fn update(...)`  (no
`#[secured]`/`#[step_up]`/`#[throttle]` stacked):

1. `has_authorize_guard` scans the handler's attributes, sees only `authz`
   (not `"authorize"`), and — since the body hasn't been touched yet either
   — reports `false`.
2. The route macro's `body_guarded_replay` is therefore `false`, so it keeps
   the standalone `IdempotencyReplayLayer` on the route.
3. That layer is Tower middleware that serves a cached response for a
   matching `Idempotency-Key` **before axum ever calls the handler
   function** — i.e. before `#[authorize]`'s in-body policy re-check, which
   only ever runs once the handler is actually invoked, gets a chance to
   run.
4. `#[authorize]` still expands and still emits its own in-body
   `__replay_response` check as a defensive measure, but it never gets a
   chance to execute: the outer layer already returned the cached response.

With `#[secured]`/`#[step_up]`/`#[throttle]` also stacked, the same
name-blindness hits `has_pending_authorize_attr` instead:
`should_own_replay` wrongly returns `true` for the earlier gate, which then
claims replay-serving for itself inside its own `FromRequestParts` impl —
which runs strictly before the handler body (and `#[authorize]`'s check
inside it) under all circumstances.

Either path is a **stale authorization** bug with real teeth: the whole
point of `#[authorize]` over a one-time `#[secured]` role check is that it
re-evaluates a `Policy` against live, mutable state (ownership, membership,
a revoked grant) on every request. Idempotency replay silently turns that
back into a point-in-time check, cached for as long as the idempotency
store retains the key, for the exact scenario (an authorization change) the
per-request re-check exists to catch.

## 🧪 Reproduction

Test: `integration::authorization_integration::idempotent_replay_bypasses_aliased_authorize_policy_changes`
(`autumn/tests/integration/authorization_integration.rs`)

Command:
```
cargo test -p autumn-web --test integration_tests idempotent_replay_bypasses_aliased_authorize_policy_changes -- --nocapture
```

Scenario: admin session mutates `/notes-attr-alias/1` (handler reached via
`use autumn_web::authorize as authz_alias; #[authz_alias("update", resource
= Note)]`, `.idempotent()` on) → `200 OK`, cached under
`idempotency-key: policy-recheck-key-alias`. The session's `admin` role is
then revoked. A retry with the identical body and idempotency key must get
`403 Forbidden` from `AdminOrOwnerPolicy`'s current re-check.

Failure output on trunk (`trunk-failure.txt`):
```
assertion `left == right` failed: cached idempotency replay must not skip the current #[authorize] policy check, even when #[authorize] is imported under an alias
  left: 200
 right: 403
```
The stale `200 OK` — the cached response from before the role was
revoked — was returned instead of a fresh `403`.

## 🔎 Root cause

- `autumn-macros/src/idempotency_guard.rs:33` — `has_pending_authorize_attr`
  compared `attr.path().segments.last() == "authorize"` only.
- `autumn-macros/src/route.rs:611` — `has_authorize_guard`, same literal
  comparison, gating `body_guarded_replay` (whether the route keeps the
  standalone `IdempotencyReplayLayer`).

Neither control covers an attribute reached through a `use ... as ...`
alias, because a proc-macro attribute has no visibility into the module's
`use` declarations — this is a hard Rust limitation, not a fixable oversight
in the name-matching itself, hence the fix changes what property is
checked rather than how the name is compared.

## 🩹 Fix

`autumn-macros/src/authorize.rs` gains `attr_is_authorize_shaped`: falls
back, when the literal name doesn't match, to parsing the attribute's
argument tokens through `#[authorize]`'s own grammar
(`parse_with_leading_literal` — already used for the same purpose by
`api_doc`'s OpenAPI metadata extractor, so this reuses an existing,
already-tested parser rather than adding a new one). Only an attribute that
supplies both the required `action` and `resource` parses successfully, so
the check stays targeted to `#[authorize]`'s actual shape instead of
deferring for every unrecognized attribute on a handler. A false positive
here only costs an optimization — the route becomes ineligible for
gate/layer-level replay caching, while `#[authorize]`'s own in-body
`replay_stop` still serves the cached response correctly *after* the policy
re-check — never a security property, so the check deliberately errs toward
over-matching rather than under-matching.

Wired into all three name-based scans that share this root cause:

- `idempotency_guard::has_pending_authorize_attr`
- `route::has_authorize_guard`
- `api_doc::extract_authorize_bindings` (the OpenAPI metadata extractor —
  same blind spot, lower stakes: an aliased route's authorize binding was
  silently missing from the generated API doc rather than misrepresenting
  a security property)

## ✅ Verification

- `cargo test -p autumn-web --test integration_tests -- authorization_integration`:
  28/28 pass (`after.txt`), including the new red test and every existing
  sibling — stacked `#[secured]` + `#[authorize]`, reversed attribute order,
  the non-aliased replay-vs-policy-change cases, and
  `idempotent_replay_does_not_bypass_authorize_policy_check_when_secured_gate_runs_first`.
- `cargo test -p autumn-macros --lib`: 1177/1177 pass, including every
  golden-expansion test pinning `#[secured]`/`#[step_up]`/`#[throttle]`/
  `#[authorize]`'s exact generated output — unchanged by this fix, since it
  only touches the *detection* helpers, not what any guard itself emits.
- `cargo fmt --all -- --check`: clean.
- `cargo clippy -p autumn-macros -p autumn-web --all-targets -- -D warnings`:
  run as part of this change (see PR for output).

## 📡 Blast radius

Swept every other name-based "is guard X present" scan in
`autumn-macros/src/` for the same anti-pattern:

- `secured`/`step_up`/`throttle`'s own literal-name self-checks (e.g.
  `param_helpers::reject_if_incompatible_route_marker`,
  `has_any_guard_gate_param`) are **not** affected: they detect an
  *already-expanded* gate by its framework-generated parameter-type prefix
  (`__AutumnSecuredGate_`/`__AutumnStepUpGate_`/`__AutumnThrottleGate_`),
  which is never derived from a user-chosen alias.
- `has_step_up_guard`/`has_throttle_guard` (`route.rs`) have the identical
  literal-name blind spot for their *own* pending-attribute half of the
  check, but — unlike `#[authorize]` — `#[step_up]`/`#[throttle]` are
  themselves pre-body `FromRequestParts` gates once expanded, and a gate
  parameter is always detectable by its framework-controlled name regardless
  of how the attribute itself was spelled. An aliased `#[step_up]`/
  `#[throttle]` therefore only risks the same lower-stakes
  `IdempotencyReplayLayer`-not-suppressed shape as `#[authorize]` alone (no
  in-body policy re-check to bypass, since these two enforce entirely inside
  their gate) — logged here, not fixed in this PR, since it needs its own
  reproduction and is a narrower, "replay-layer redundantly retained"
  correctness gap rather than an authorization bypass. Worth a follow-up.
- `api_doc::extract_authorize_bindings` — fixed in this PR alongside the two
  security-relevant call sites, since it shares the exact same root cause
  and the shared helper closes all three in one change.
- Feature matrix: this bug lives entirely in `autumn-macros` (proc-macro
  expansion, not runtime), so it is feature-independent — it reproduces
  identically under every feature combination that compiles `#[authorize]`
  and `.idempotent()` together, which is the default feature set already
  exercised by `autumn/tests/integration/authorization_integration.rs`.

## 📜 Compatibility

Pure bugfix at the macro-expansion layer: no public signature, config
default, or route status code changes. Existing tests (golden-expansion and
integration) pass unchanged. No `CHANGELOG.md` version bump — landing under
`## [Unreleased]` per `CLAUDE.md`.

## 🗂 Ledger

- `trunk-failure.txt` — the RED run
- `after.txt` — the GREEN run
