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
- `cargo test -p autumn-macros --lib`: 1187/1187 pass, including every
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

## Round 4: `cfg_attr` hid an authorize-shaped payload from every check

The final round-2/round-3 design still scanned each attribute's own
`attr.meta` directly. `#[cfg_attr(pred, ...)]` is a compiler builtin, not a
macro: it stays unexpanded (as `param_helpers.rs`'s pre-existing
`attr_or_cfg_attr_matches_any` already documents) until every attribute
*macro* has finished running, so its top-level path is `cfg_attr`, never the
inner attribute's name. Both `attr_is_authorize_shaped` and
`reject_if_ambiguous_authorize_shape` therefore missed anything written as
`#[cfg_attr(feature = "auth", authz("update", resource = Note))]` —
including a **literal** `#[cfg_attr(pred, authorize(...))]`, which should
have been recognized as a real guard and wasn't. With the feature enabled at
compile time, the ambiguous/aliased case reopened the original replay
bypass (`IdempotencyReplayLayer` stays on the route since `has_authorize_guard`
never saw a pending `#[authorize]`), and the literal case caused an earlier
gate to wrongly claim replay ownership that `#[authorize]` should have kept.

Fixed by adding `for_each_conditionally_applied_meta(attr, check)`, which
mirrors `attr_or_cfg_attr_matches_any`'s existing pattern: when
`attr.path().is_ident("cfg_attr")` it parses the nested
`Punctuated<Meta, Token![,]>` and runs `check` over every entry after the
first (the `cfg`/`cfg_attr` predicate); otherwise it runs `check` on
`attr.meta` directly. Both `attr_is_authorize_shaped` (via
`meta_is_literally_authorize`) and `reject_if_ambiguous_authorize_shape`
(via `meta_is_ambiguous_authorize_shape`) now go through this helper, so a
`cfg_attr`-wrapped alias or shape collision is refused at compile time and a
`cfg_attr`-wrapped literal `#[authorize(...)]` is correctly recognized as a
real guard.

New tests: `rejects_an_aliased_authorize_shape_behind_cfg_attr`,
`rejects_an_unrelated_shape_behind_cfg_attr`,
`accepts_the_literal_name_behind_cfg_attr_without_ambiguity`,
`attr_is_authorize_shaped_recognizes_the_literal_name_behind_cfg_attr`.

## Round 5: even a literal, correctly-spelled `cfg_attr`-conditional `#[authorize]` is unsafe to resolve — plus nested `cfg_attr`

Round 4 landed on: recognize a `cfg_attr`-wrapped attribute (name or shape)
the same way a plain one is recognized. Codex review found two further gaps
in that same commit.

**Finding A — presence ambiguity, not just name ambiguity.** Round 4 treated
`#[cfg_attr(feature = "auth", authorize(...))]` as an unconditionally
*present* `#[authorize]` guard, on the reasoning that the literal name is
never ambiguous. That reasoning only holds for a plainly-written attribute,
which always applies. A `cfg_attr`-wrapped one might not: Autumn cannot
evaluate the cfg predicate at macro-expansion time, so whether this
`#[authorize]` will actually be present in the compiled handler is unknown.
Guessing either way is unsafe, in the same shape as the original
aliasing problem but about *presence* instead of *name*:

- Guess "present" (round 4's behavior): `has_authorize_guard`/
  `should_own_replay` suppress the standalone `IdempotencyReplayLayer` (or an
  earlier gate's own replay-serving) on the assumption `#[authorize]`'s
  in-body check will own it. If the predicate is actually false at build
  time, `#[authorize]` never runs, nothing ends up owning replay, and a
  retried mutation re-executes instead of replaying — the idempotency
  guarantee silently disappears.
- Guess "absent": if the predicate is actually true, the standalone replay
  layer stays active and can serve a stale cached response before
  `#[authorize]`'s in-body policy re-check ever gets a chance to run — the
  original stale-authorization bypass, reopened.

**Finding B — nested `cfg_attr` isn't recursed into.** The round-4
`for_each_conditionally_applied_meta` only unwrapped one level of `cfg_attr`,
so `#[cfg_attr(a, cfg_attr(b, authorize(...)))]` tested the inner
`cfg_attr(b, authorize(...))` meta itself (whose path is `cfg_attr`, not
`authorize`) against the check function and never reached the real
`authorize` meta underneath.

**Fix:** `for_each_conditionally_applied_meta`'s walk (used by
`attr_is_authorize_shaped`) is now genuinely recursive — any nested meta
whose own path is `cfg_attr` is unwrapped the same way, at any depth (closes
Finding B, and applies to every caller of `attr_is_authorize_shaped`).
`reject_if_ambiguous_authorize_shape` now refuses **any** `#[authorize]`-
shaped attribute reached through `cfg_attr` — including the literal,
correctly-spelled name — via a new `find_unsafe_cfg_attr_authorize_meta`
helper and `conditionally_applied_meta_is_unsafe_to_resolve` predicate
(closes Finding A). Outside `cfg_attr`, a plainly-written `#[authorize(...)]`
is still accepted without ambiguity, since it always applies and has no
presence question to answer.

New tests: `rejects_the_literal_name_behind_cfg_attr_since_presence_is_unknowable`
(replaces the now-incorrect `accepts_the_literal_name_behind_cfg_attr_without_ambiguity`),
`rejects_an_aliased_authorize_shape_behind_nested_cfg_attr`,
`attr_is_authorize_shaped_recognizes_the_literal_name_behind_nested_cfg_attr`.

> **Superseded by Round 6 below.** Every claim in rounds 4 and 5 about
> `cfg_attr` staying unexpanded when a *sibling* attribute macro runs was
> wrong, verified empirically. All of the cfg_attr-specific code introduced
> in these two rounds — `for_each_conditionally_applied_meta`,
> `find_unsafe_cfg_attr_authorize_meta`,
> `conditionally_applied_meta_is_unsafe_to_resolve`, and the unit tests
> named above — was removed in Round 6. Left in place here, uncorrected, as
> an honest record of the reasoning that seemed right at the time and where
> it broke down; see Round 6 for what actually happens and why the security
> property survives anyway (via a different, pre-existing code path).

## Round 6: rounds 4 and 5 were built on a wrong premise about `cfg_attr` — corrected, verified empirically

Codex review on the round-5 commit flagged that the reasoning behind the
`cfg_attr`-specific code (`param_helpers.rs`'s pre-existing, and previously
unquestioned, claim that `cfg_attr` stays unexpanded until *every* attribute
macro on an item has run) doesn't hold for a `cfg_attr` sitting alongside a
*separate*, active attribute macro on the same item — only for the case
`param_helpers::attr_or_cfg_attr_matches_any` was actually written for
(#2513): detecting a `cfg_attr`-wrapped guard for a **refuse-this-
combination** check, where over-detection is harmless. Rounds 4/5 reused
that pattern for a **replay-ownership** decision, where guessing wrong in
either direction is unsafe — and the underlying premise turned out to be
false for this shape of check.

This was not taken on faith. Two real `rustc` compiles (via `cargo check
--example`, against the already-built crate, deleted after use — not a
unit test constructing tokens by hand) settled it:

1. `#[route] #[cfg_attr(all(), authorize(...))]` and the reverse attribute
   order both show `#[authorize]`'s own signature injection (hidden
   `Session`/`State` parameters) firing — proof `authorize_macro` really
   runs and `cfg_attr` is already gone by the time it or `route_macro` sees
   the item, regardless of which is written first.
2. `#[route] #[cfg_attr(all(), authz_alias(...))]` (an aliased name behind
   `cfg_attr`) produces this file's *ordinary*, non-`cfg_attr`
   `AMBIGUOUS_NAME_MESSAGE` — not a `cfg_attr`-specific one — and the alias
   import is flagged `unused`, proof the compiler resolved `cfg_attr` and
   handed `route_macro` a plain, already-unwrapped attribute *before*
   `authz_alias` was ever looked up as its own macro.

**Conclusion:** on this workspace's MSRV, `cfg_attr` is resolved by the
compiler before a *sibling* attribute macro on the same item ever runs,
independent of write order. So the round-4/5 `cfg_attr`-unwrapping code in
`attr_is_authorize_shaped` and `reject_if_ambiguous_authorize_shape` was
dead: never reached by real compiled code, only by the unit tests that
constructed input via `syn::parse_quote!` (which cannot observe real
`rustc` expansion order at all — they were passing for the wrong reason).
The *security property* rounds 4/5 were chasing was never actually missing:
an aliased name behind `cfg_attr` is still caught, because by the time
`reject_if_ambiguous_authorize_shape` runs, the wrapper is already gone and
it's indistinguishable from an ordinary aliased `#[authorize]` — the
pre-existing, unmodified `meta_is_ambiguous_authorize_shape` path already
handles it correctly.

**Fix:** removed `for_each_conditionally_applied_meta`,
`find_unsafe_cfg_attr_authorize_meta`, and
`conditionally_applied_meta_is_unsafe_to_resolve` entirely.
`attr_is_authorize_shaped` and `reject_if_ambiguous_authorize_shape` are
back to plain, non-`cfg_attr`-aware checks. Removed the round-4/5 unit
tests for the reason above. Added real, `rustc`-verified coverage instead:
`tests/compile-fail/authorize_ambiguous_shape_alias_behind_cfg_attr.rs`, a
trybuild fixture proving the aliased-behind-`cfg_attr` case is refused via
the *ordinary* path — genuine end-to-end coverage the unit tests never
provided. Corrected `docs/migrations/next.md`, which had documented the
round-5 `cfg_attr` refusal as its own breaking case; removed that
subsection (a literal `#[authorize]` behind `cfg_attr` was never actually
refused in real builds, so there was nothing to migrate).

**Lesson:** `param_helpers::attr_or_cfg_attr_matches_any`'s own doc comment
(#2513) is correct for what it's used for — a **refuse-this-combination**
check, where treating a `cfg_attr`-wrapped guard as unconditionally present
is a safe, conservative direction to guess. It is not a general fact about
`cfg_attr` expansion order safe to reuse for every kind of check; applying
it to a **replay-ownership** decision (where over- *and* under-detection are
both unsafe) was the actual mistake, not the underlying Rust semantics,
which — per the two probes above — are actually more forgiving here than
rounds 4/5 assumed. `param_helpers.rs`'s own use of the pattern is
unaffected by this correction: it was never subject to this failure mode,
since its checks are refuse-only, not replay-ownership decisions, and their
guess is safe in the "always suspect a cfg_attr-wrapped guard" direction
regardless of real expansion order.

## Round 7: name alone still can't prove *identity* even when it matches

Rounds 0–6 closed the case where Autumn's real `#[authorize]` is reached
under a name that *doesn't* match ("authorize" spelled some other way).
Codex review found the mirror-image gap: name alone also can't prove that
an attribute *literally* named `authorize` really is Autumn's macro. A
proc-macro attribute never sees import resolution, so an entirely unrelated
attribute macro reachable as `authorize` — imported from another crate
under that name, or aliased to it — passes `attr_is_authorize_shaped`'s
name check exactly as easily as Autumn's own, misleading
`has_authorize_guard`/`should_own_replay` into suppressing replay-layer
protection for a route that has no real authorization check on it at all.

Unlike the name-*mismatch* case, this one narrows without breaking
Autumn's own primary feature: `#[authorize]` always requires
`"action", resource = Type[, from = ident]`, and `authorize_macro` itself
already enforces that grammar independently — a genuine Autumn call with
malformed arguments fails to compile regardless of what
`attr_is_authorize_shaped` answers. So requiring the argument shape *in
addition to* the name closes the overwhelming majority of the gap without
any cost to legitimate usage: a real Autumn call always matches (name *and*
valid grammar), while an unrelated macro reachable as `authorize` would
have to coincidentally also accept Autumn's exact bespoke calling
convention to still slip through.

**Fix:** `attr_is_authorize_shaped` now requires both `meta_is_literally_authorize`
and a new `meta_has_authorize_arg_shape` helper (the argument-parsing logic
already used by `meta_is_ambiguous_authorize_shape`, extracted so both
share it). New tests:
`attr_is_authorize_shaped_rejects_a_same_named_attribute_with_a_different_grammar`
(a same-named attribute with unrelated arguments, and a bare `#[authorize]`
with none, are both no longer treated as present).

**Accepted residual limitation:** an unrelated macro reachable as
`authorize` that *also* happens to accept the exact
`"action", resource = Type[, from = ident]` grammar would still be
misclassified as Autumn's guard. This is not fixed, and — unlike every
other gap in this document — is not expected to be: refusing every
literally-named `#[authorize(...)]` attribute (the only way to close it
completely) would break the framework's own core feature for every
legitimate caller, with no compensating benefit, since the residual
scenario requires an unrelated crate to coincidentally invent both the same
macro name *and* the same bespoke argument syntax Autumn uses. Documented
here rather than left silent.

## Round 8: fixing round 7 for `#[authorize]` broke `#[feature_flag]` + `#[static_get]`

Round 3 added `__AutumnFlagGate_` to `param_helpers::GUARD_GATE_TYPE_PREFIXES`
so `#[secured]`/`#[step_up]`/`#[throttle]` could see an earlier
`#[feature_flag]` gate for idempotency-replay-ownership purposes. That list
turned out to be shared by an entirely unrelated check:
`static_route.rs`'s `has_any_guard_gate_param` call, used to decide whether
an *already-expanded* guard is incompatible with `#[static_get]`. Adding
`__AutumnFlagGate_` to the shared list made that check start rejecting
`#[feature_flag]` too — but only when `#[feature_flag]` was written *above*
`#[static_get]` (so its gate had already expanded by the time
`static_get_macro` ran); the reverse order kept compiling, since
`feature_flag_macro` never checked for the static-route marker either. An
unintended, order-dependent compile break, not itself a security issue —
Codex found it while reviewing round 7's changes.

**Fix (superseded by round 10 below):** added a narrower
`has_any_auth_guard_gate_param` (secured/step_up/throttle only) and
switched `static_route.rs`'s check to it, restoring compilation for
`#[feature_flag] + #[static_get]` in both orders.

## Round 9: the compile-error migration guide overclaimed universality

A P2 finding on the migration guide itself: `reject_if_ambiguous_authorize_shape`
only scans attributes still present, unexpanded, at the point a route/guard
macro runs — an aliased or shape-alike attribute positioned *above* the
route macro expands first (as its own real macro) and is gone from the
attribute list before this refusal ever sees it. The migration guide's
prose didn't scope the claim to this ordering.

Verified this ordering doesn't reopen a gap either way: a genuine aliased
`#[authorize]` positioned there still runs for real and leaves a
recognizable in-body check every guard macro's existing fallback body-scan
already detects (the same mechanism that makes a *literal* `#[authorize]`
above `#[post]` work); an unrelated attribute positioned there just expands
as itself and leaves nothing authorize-shaped behind, so the route is
correctly recognized as unguarded. No code change — clarified
`docs/migrations/next.md` to scope the claim precisely instead of stating
it unconditionally.

## Round 10: round 8's fix was backwards — `#[feature_flag]` should stay incompatible with `#[static_get]`, for the same reason the other three guards are

Codex review on round 8 pointed out the real, deeper issue round 8 missed
entirely: `#[static_get]` routes serve cached SSG/ISR hits through the
static-first middleware, entirely **before** the inner router — and the
handler, and every one of its pre-body `FromRequestParts` gates — is ever
reached. That is exactly why `#[secured]`/`#[step_up]`/`#[throttle]`/
`#[authorize]` are already rejected in combination with `#[static_get]`
(`static_route.rs`'s own pre-existing `INCOMPATIBLE_GUARD_MSG` documents
this rationale). `#[feature_flag]`'s pre-body gate has the identical shape
and the identical blind spot, but was never included in that rejection —
predating this entire PR, not introduced by it. Round 8's fix made the
combination *compile*, which is the wrong direction: a
`#[feature_flag]`-gated `#[static_get]` route looked correctly protected
but silently wasn't — once a page was cached, disabling the flag afterward
never stopped the cache from continuing to serve it.

**Fix:** reverted round 8's narrowing (removed
`has_any_auth_guard_gate_param`/`AUTH_GUARD_GATE_TYPE_PREFIXES` entirely);
`static_route.rs`'s already-expanded-guard check is back to the broad
`has_any_guard_gate_param`, which already includes `__AutumnFlagGate_`.
Added `"feature_flag"` to `INCOMPATIBLE_GUARD_ATTRS` and the diagnostic
message for the reverse attribute order (still-unexpanded, below
`#[static_get]`). `feature_flag_macro` now also calls
`param_helpers::reject_if_incompatible_route_marker`, mirroring
`secured`/`step_up`/`throttle`/`authorize` — without this, an **aliased**
`#[feature_flag]` written below `#[static_get]` would slip past
`static_get_macro`'s literal-name scan (which can't see aliases, the same
limitation as every other guard) and expand anyway, unprotected. Confirmed
`#[feature_flag]` doesn't rewrite the handler's return type, so `#[ws]`'s
separate, differently-reasoned incompatibility list (return-type mismatch,
not cache-bypass) does not need the same addition.

Since this closes a genuinely pre-existing (not this-PR-introduced) gap —
`#[feature_flag] + #[static_get]` compiled and silently under-protected on
trunk before any of this PR's changes — it has its own `CHANGELOG.md`
`### Security` entry and
[migration guide section](../../migrations/next.md#static_get-feature_flag-is-now-a-compile-error),
separate from the `#[authorize]` aliasing entry.

## 🗂 Ledger

- `trunk-failure.txt` — round-0 RED run (the original runtime bypass)
- `after.txt` — round-0 GREEN run (round-1 fix attempt, later found
  insufficient — see Reproduction)
