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
`autumn-macros/src/secured.rs`, `autumn-macros/src/step_up.rs`,
`autumn-macros/src/throttle.rs`, `autumn-macros/src/route.rs`,
`autumn-macros/src/feature_flag.rs`, `autumn-macros/src/param_helpers.rs`

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

The reproduction evolved across three commits as PR review (Codex, two
rounds) found the first two fix attempts still unsafe. All three runs are
preserved below since each is evidence for why the final design looks the
way it does.

**Round 0 — the original bypass (RED, `autumn/tests/integration/authorization_integration.rs`):**

```
cargo test -p autumn-web --test integration_tests idempotent_replay_bypasses_aliased_authorize_policy_changes -- --nocapture
```

Admin session mutates `/notes-attr-alias/1` (handler reached via
`use autumn_web::authorize as authz_alias; #[authz_alias("update", resource
= Note)]`, `.idempotent()` on) → `200 OK`, cached under an idempotency key.
The session's `admin` role is revoked. A retry with the identical body and
idempotency key must get `403 Forbidden`.

Failure output on trunk (`trunk-failure.txt`):
```
assertion `left == right` failed: cached idempotency replay must not skip the current #[authorize] policy check, even when #[authorize] is imported under an alias
  left: 200
 right: 403
```
The stale `200 OK` was returned instead of a fresh `403` — confirmed fixed
by the round-1 attempt (`after.txt`), which made `attr_is_authorize_shaped`
fall back to parsing the attribute's argument tokens through
`#[authorize]`'s own grammar whenever the literal name didn't match.

**Round 1 → Codex P1 (fresh false positive, not caught by the round-0 test):**
the round-1 shape fallback classified *any* attribute sharing
`#[authorize]`'s exact grammar (`"action", resource = Type`) as
authorize-like, including an unrelated `#[audit("update", resource =
Note)]` with no real `#[authorize]` anywhere. With nothing left to serve a
cached reply, a retried mutation would re-execute instead of replaying —
losing `.idempotent()`'s dedup guarantee, not "merely an optimization" as
the round-1 doc comment claimed. Fixed in round 2 by also requiring a
parameter binding matching `#[authorize]`'s calling convention
(`from`/snake_case(`resource`)).

**Round 2 → Codex P1 again (the parameter check doesn't close the gap):**
Codex correctly pointed out that a handler stacked with an
`#[audit(...)]`-shaped attribute routinely *does* have a resource parameter
matching that name (that's the natural shape for an audit/logging
attribute), so the round-2 narrowing didn't materially reduce the
false-positive rate — `#[audit("update", resource = Note)]` on
`async fn h(note: Note)` still collided.

**Final design:** no shape-based guess is safe in both directions, so
disambiguation moved to compile time. `attr_is_authorize_shaped` reverted to
exact-name-only matching; a new `authorize::reject_if_ambiguous_authorize_shape`
refuses to compile a handler carrying an attribute that matches
`#[authorize]`'s grammar under any other name, wired into `secured_macro`,
`step_up_macro`, `throttle_macro`, and `route_macro`. Proven by two trybuild
`compile_fail` fixtures instead of a runtime test, since the vulnerable
pattern (and the round-1/round-2 false positive) no longer compile at all:

```
cargo test -p autumn-web --test integration_tests integration::compile_fail::compile_fail_tests -- --exact --nocapture
```

- `tests/compile-fail/authorize_ambiguous_shape_alias.rs` — the original
  aliased-`#[authorize]` case now refused at compile time.
- `tests/compile-fail/authorize_ambiguous_shape_unrelated.rs` — the exact
  Codex-reported false positive (`#[audit("update", resource = Note)]` on a
  handler with a matching `note` parameter, no real `#[authorize]`) also
  refused, rather than silently accepted in either direction.

## 🔎 Root cause

- `autumn-macros/src/idempotency_guard.rs` — `has_pending_authorize_attr`
  compared `attr.path().segments.last() == "authorize"` only.
- `autumn-macros/src/route.rs` — `has_authorize_guard`, same literal
  comparison, gating `body_guarded_replay` (whether the route keeps the
  standalone `IdempotencyReplayLayer`).

Neither control covers an attribute reached through a `use ... as ...`
alias, because a proc-macro attribute has no visibility into the module's
`use` declarations. That is a hard Rust limitation — not a fixable
oversight in the name-matching itself — which is also why no purely
syntactic *heuristic* (shape of the arguments, with or without a parameter
check) can resolve it safely in both directions. The fix therefore doesn't
try to guess right; it refuses to guess at all.

## 🩹 Fix

`autumn-macros/src/authorize.rs`:

- `attr_is_authorize_shaped(attr, input_fn) -> bool` — exact-name-only
  (`attr.path()` ends in `"authorize"`). No shape or parameter fallback.
- `reject_if_ambiguous_authorize_shape(input_fn) -> Option<TokenStream>` —
  for every attribute that is *not* literally `#[authorize]`, parses its
  argument tokens through `#[authorize]`'s own grammar
  (`parse_with_leading_literal`, already used for the same purpose by
  `api_doc`'s OpenAPI metadata extractor). If it supplies both the required
  `action` and `resource`, returns a `compile_error!` explaining the
  ambiguity and how to resolve it (spell `#[authorize(...)]` by its real
  name, or rename the colliding attribute).

Wired into the four macro entry points whose idempotency-replay-ownership
decision depends on knowing whether a not-yet-expanded `#[authorize]` is
present: `secured_macro`, `step_up_macro`, `throttle_macro` (all three via
`idempotency_guard::should_own_replay`'s `has_pending_authorize_attr`), and
`route_macro` (via `has_authorize_guard`).

`api_doc::extract_authorize_bindings` (the OpenAPI metadata extractor) keeps
its own exact-name-only scan — it was never security-relevant (a missing
binding just omits API-doc metadata) and doesn't need the compile-time
refusal, since a route that fails to compile obviously has no metadata to
extract either way.

## ✅ Verification

- `cargo test -p autumn-web --test integration_tests -- authorization_integration`:
  27/27 pass — every pre-existing sibling unaffected (stacked `#[secured]` +
  `#[authorize]`, reversed attribute order, non-aliased replay-vs-policy-
  change cases, `idempotent_replay_does_not_bypass_authorize_policy_check_when_secured_gate_runs_first`).
- `cargo test -p autumn-web --test integration_tests integration::compile_fail::compile_fail_tests`:
  both new fixtures pass. 8 of the other 79 registered fixtures fail in this
  sandbox (`lifecycle_undeclared_transition`, `lifecycle_terminal_has_no_exit`,
  `lifecycle_start_only_on_initial`, `repository_ledgered_purge_rejected`,
  `classified_json_model_leak`, `classified_json_field_leak`,
  `classified_wrong_boundary`, `classified_column_wrapper_cannot_retype`) —
  confirmed pre-existing/environmental: none of their macros, fixtures, or
  goldens were touched by this change (`git diff --name-only` touches only
  `authorize`/`secured`/`step_up`/`throttle`/`route` and this PR's own new
  fixtures), consistent with a rustc/dependency version drift between this
  sandbox and whichever environment last generated those goldens.
- `cargo test -p autumn-macros --lib`: 1182/1182 pass, including every
  golden-expansion test pinning `#[secured]`/`#[step_up]`/`#[throttle]`/
  `#[authorize]`'s exact generated output (unaffected — the fix only adds an
  early rejection, never changes what a guard emits when it does compile)
  and five new tests covering the name-only detector and the compile-time
  refusal in both directions (aliased-authorize and unrelated-attribute
  collision).
- `cargo fmt --all -- --check`: clean.
- `cargo clippy -p autumn-macros --all-targets -- -D warnings`: clean.
- `cargo clippy -p autumn-web --all-targets -- -D warnings`: clean.
  (All three: only the pre-existing, unrelated `clippy::unused_async_trait_impl`
  unknown-lint warning documented at `Cargo.toml:163-178`.)

## 📡 Blast radius

Swept every other name-based "is guard X present" scan in
`autumn-macros/src/` for the same anti-pattern:

- `param_helpers::has_any_guard_gate_param` and friends detect an
  *already-expanded* gate by its framework-generated parameter-type prefix
  (`__AutumnSecuredGate_`/`__AutumnStepUpGate_`/`__AutumnThrottleGate_`),
  never derived from a user alias — not affected.
- `route::has_step_up_guard`/`has_throttle_guard` have the identical
  literal-name blind spot for their *own* pending-attribute half of the
  check, but — unlike `#[authorize]` — `#[step_up]`/`#[throttle]` are
  themselves pre-body `FromRequestParts` gates once expanded, and a gate
  parameter is always detectable by its framework-controlled name regardless
  of how the attribute itself was spelled. An aliased `#[step_up]`/
  `#[throttle]` therefore only risks the lower-stakes "standalone
  `IdempotencyReplayLayer` redundantly retained" correctness shape (no
  in-body policy re-check to bypass, since these two enforce entirely inside
  their own gate) — logged here, not fixed in this PR, since it needs its
  own reproduction and is a narrower correctness gap rather than an
  authorization bypass. Worth a follow-up.
- `api_doc::extract_authorize_bindings` — deliberately left as exact-name-
  only (see Fix); not a security-relevant gap.
- Feature matrix: this bug lives entirely in `autumn-macros` (proc-macro
  expansion, not runtime), so it is feature-independent — it reproduces
  identically under every feature combination that compiles `#[authorize]`
  and `.idempotent()` together, which is the default feature set already
  exercised here.

## 📜 Compatibility

This is a real, intentional breaking change, not a pure bugfix: an app that
reaches `#[authorize]` through `use ::autumn_web::authorize as x;` compiled
successfully (and was vulnerable) before this change; it gets a compile
error after. No other Autumn macro's aliasing is affected, and the literal
`#[authorize(...)]` spelling — every known caller, in this repo and (per a
full-repo grep before landing) every example — is unaffected. No public
signature, config default, or route status-code change. `CHANGELOG.md`
`## [Unreleased] > ### Security` entry documents the compile-time refusal
and its migration (spell `#[authorize]` by its real name). No version bump.

## Round 3: `#[feature_flag]`'s gate never consulted replay ownership at all

While reviewing the round-2 fix, Codex found a fourth pre-body gate macro,
`#[feature_flag]`, whose `FromRequestParts` gate served a cached idempotency
reply **unconditionally** whenever the flag was enabled — it never called
`idempotency_guard::should_own_replay` at all, unlike `#[secured]`/
`#[step_up]`/`#[throttle]`. With `#[feature_flag(...)] #[authorize(...)]
#[post(...)]` (feature flag topmost, so its gate ends up leftmost and runs
first), the gate would serve a stale cached response before `#[authorize]`'s
policy re-check ever ran — reachable with a **literal** `#[authorize]`, not
only an aliased one, since `#[feature_flag]` never checked by name, shape,
or ownership in the first place. Separately, `param_helpers::GUARD_GATE_TYPE_PREFIXES`
never listed `__AutumnFlagGate_`, so `#[secured]`/`#[step_up]`/`#[throttle]`
couldn't detect an earlier `#[feature_flag]` gate either and could
wrongly double-claim ownership on top of it.

Fixed by making `feature_flag_macro`'s gate wrap its replay check in
`should_own_replay(&input_fn)` (the exact pattern the other three gates
use), wiring `reject_if_ambiguous_authorize_shape` into it too, and adding
`__AutumnFlagGate_` to `GUARD_GATE_TYPE_PREFIXES`. A sweep of every
`FromRequestParts` impl generator in `autumn-macros/src/` (`grep -rl "impl.*FromRequestParts"`)
confirmed these four gate macros (`secured`, `step_up`, `throttle`,
`feature_flag`) are the complete set with this shape; `repository.rs`'s and
`service.rs`'s `FromRequestParts` impls are for unrelated, self-contained
extractors (a repository's own generated policy-check-then-replay sequence,
already correctly ordered within one macro's own output; DI-only service
extraction) with no cross-macro ordering ambiguity to exploit.

## 🗂 Ledger

- `trunk-failure.txt` — round-0 RED run (the original runtime bypass)
- `after.txt` — round-0 GREEN run (round-1 fix attempt, later found
  insufficient — see Reproduction)
