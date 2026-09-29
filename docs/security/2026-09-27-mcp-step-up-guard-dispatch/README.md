# 2026-09-27 — `#[step_up]` × MCP `tools/call` dispatch (negative result)

## 🎯 Surface

`autumn_macros::step_up` (the macro-generated `FromRequestParts` freshness
guard, issue #1668) × `autumn_web::mcp` (`#[api_doc(mcp)]`, `mount_mcp`,
`tools/call` dispatch). Entry point investigated: a handler carrying both
`#[step_up]` (or `#[step_up(max_age = "…")]`) and `#[api_doc(mcp)]`, called
through MCP's JSON-RPC `tools/call` rather than a direct HTTP request.

## 🕵️ Threat model (hypothesis)

Against an app that follows Autumn's own documented pattern — guard a
sensitive handler with `#[step_up]` and separately opt it into the agent
surface with `#[api_doc(mcp)]`, exactly as it would guard any other MCP tool
— an attacker holding a **stolen but merely authenticated session cookie**
(no fresh reauthentication) could reach the step-up-gated handler and run
the sensitive action by calling it through `POST /mcp` `tools/call` instead
of the direct HTTP route, if MCP dispatch failed to carry the guard the way
`#2608` (`docs/security/2026-09-07-mcp-custom-layer-static-mode/`) found it
skipping `AppBuilder::layer` custom layers in SSG/ISR mode. The app author
would have done nothing wrong: `#[step_up]` and `#[api_doc(mcp)]` are both
documented, independent attributes.

`#[secured]` (`2026-09-10-mcp-secured-guard-dispatch`) and `#[throttle]`
(`2026-09-20-mcp-throttle-guard-dispatch`) were each proven to survive this
same dispatch path, by the same general mechanism (`serve_tools_call`
dispatches through the real, fully-assembled router). `#[step_up]` had no
dedicated coverage, and it is a different shape from both: its rejection is
a `302` redirect to `/reauth` for a non-JSON caller, or a `401` for a
JSON-accepting one (`__step_up_json_response`) — not a plain `401`/`403`
(`#[secured]`) or `429` (`#[throttle]`). That matters because MCP's
buffered-result packaging (`buffered_tool_result` in `autumn/src/mcp.rs`)
decides `isError` from `!status.is_success()`, i.e. "not 2xx" — a narrower
"is this 4xx or 5xx" check would have missed the `3xx` redirect branch and
reported a stale-session step-up rejection as tool *success*, handing the
handler's real response back to a caller who never freshly reauthenticated.
`#[step_up]` also has a precondition `#[secured]` does not: presence of a
claim is not enough, it must additionally be *fresh* — worth pinning
separately from a "logged in or not" boolean, with a claim that is
genuinely stale rather than merely absent.

## 🧪 Reproduction attempt → negative result

Test: `autumn/tests/integration/mcp_step_up_guard.rs`:
- `step_up_guard_rejects_an_mcp_tool_call_with_no_session`
- `step_up_guard_rejects_an_mcp_tool_call_with_a_session_but_no_fresh_claim`
- `step_up_guard_rejects_an_mcp_tool_call_with_a_stale_claim`
- `step_up_guard_allows_an_mcp_tool_call_with_a_fresh_claim`

```
cargo test -p autumn-web --test integration_tests --features mcp mcp_step_up_guard
```

Result: **pass, all four** — no bypass. See `after.txt` for the full run,
including the Codex review round that caught two accuracy gaps in the first
version of this test (below). A caller with no session, a caller with a
session but no claim, and a caller with a claim that is present but older
than the route's `max_age` (backdated 600s against the 300s default) all get
`isError: true` with no trace of the handler's real response. A caller who
freshly stamped the claim gets the real response.

**Corrected during review** (both flagged by Codex on the PR, both real
gaps, neither a security finding):
1. The original "no fresh claim" test only exercised an *absent* claim, not
   a genuinely stale one, so it didn't actually prove recency-checking —
   fixed by adding `step_up_guard_rejects_an_mcp_tool_call_with_a_stale_claim`.
2. The original write-up claimed `build_request` sends
   `Accept: application/json`, so a rejected `tools/call` would exercise
   `#[step_up]`'s JSON branch. That's wrong: `FORWARDED_HEADERS` doesn't
   forward `Accept` at all (see Root cause below), so every MCP-dispatched
   rejection actually takes the HTML `/reauth` redirect branch. The
   negative result is unaffected — a `302` is exactly as correctly
   classified as `isError: true` as a `401` — but the misleading
   comment/assertion claiming JSON-branch coverage was removed.

## 🔎 Root cause of the fail-safe behavior

`#[step_up]` expands to a hidden `FromRequestParts` parameter inserted ahead
of the handler's own parameters (`autumn-macros/src/step_up.rs`), exactly
like `#[secured]` and `#[throttle]` — it is part of the handler's real type
signature, not a body statement, so axum resolves it during extraction for
any caller that reaches the handler through the router, however the request
was constructed.

`autumn_web::mcp::serve_tools_call` dispatches every `tools/call` through
`server.dispatch.clone().oneshot(request)`, the same fully-assembled router
a direct HTTP request traverses, and `mcp::build_request` forwards the
caller's `Cookie` header onto the synthetic dispatched request — so
whatever session the real MCP caller presented (or didn't) is exactly what
`#[step_up]`'s `Session::from_request_parts` extraction and
`check_step_up`'s freshness comparison see.

Separately, `buffered_tool_result`'s `!status.is_success()` check is a
genuine `StatusCode::is_success()` call (true only for `2xx`), not a
hand-rolled `4xx`/`5xx` range check — so it already correctly classifies
`#[step_up]`'s `302` redirect and `401` problem-details response alike as
`isError: true`. There is no narrower-than-2xx-vs-not check anywhere on this
path that a `3xx` status could slip through.

Which of those two rejection responses actually fires over MCP is not, in
fact, under the caller's control: `mcp::build_request`'s `FORWARDED_HEADERS`
list (`autumn/src/mcp.rs`) forwards `Accept-Language` but not `Accept`
itself, so the dispatched request never carries an `Accept` header at all.
`#[step_up]`'s `__wants_json` check reads a missing `Accept` as `false`, so
every MCP `tools/call` against a `#[step_up]` handler takes the HTML
`/reauth` redirect branch on rejection — never the JSON `401` branch. That
does not weaken the negative result (both branches are non-2xx and both are
therefore `isError: true`), but it does mean an agent driving a
`#[step_up]`-guarded tool only ever sees an empty-bodied redirect's tool-error
text, never the richer `application/problem+json` body. That is a
response-quality gap, not a security one, would apply to *any*
`Accept`-branching handler exposed over MCP (not specifically `#[step_up]`),
and is out of scope for this negative-result test.

## 🩹 Fix

None — no bug found. Regression test added at
`autumn/tests/integration/mcp_step_up_guard.rs`, registered in
`autumn/tests/integration/mod.rs` under `#[cfg(feature = "mcp")]`. The test
pins the actual mechanism (no-session, no-claim, and stale-claim rejection
with no leaked body; fresh-claim success with the real response) so it
fails loudly — not vacuously — if a future change to MCP dispatch,
`#[step_up]`'s expansion, or the buffered-result status check ever reopens
this path.

## ✅ Verification

- `cargo fmt --all -- --check` — clean.
- `cargo test -p autumn-web --test integration_tests --features mcp mcp_step_up_guard` — 4/4 pass (`after.txt`).
- `cargo clippy -p autumn-web --test integration_tests --features mcp -- -D warnings` — clean; no warnings attributed to the new file.
- Re-attack: confirmed `buffered_tool_result` uses `StatusCode::is_success()`
  (a real 2xx check), not a hand-written 4xx/5xx range, so the `302`
  redirect branch specific to `#[step_up]` (which `#[secured]`/`#[throttle]`
  have no equivalent of) cannot slip through as a false success.
- Codex review round: verified both flagged gaps directly against
  `autumn-macros/src/step_up.rs` and `autumn/src/mcp.rs` rather than taking
  them on faith, fixed both (added the stale-claim test; corrected the
  false `Accept: application/json` claim), and re-ran the full suite green
  (`after.txt`).

## 📡 Blast radius

- Checked all three MCP tool-registration variants
  (`#[api_doc(mcp)]`, `expose_all_as_mcp()`, manual `mount_mcp`): all three
  feed the same `derive_tools()` → `McpToolInfo` → single `server.dispatch`
  router already established for `#[secured]`/`#[throttle]`, so none has an
  independent bypass for `#[step_up]` either.
- `#[step_up(max_age = "…")]` (the non-default form) uses the identical gate
  skeleton with only the compile-time constant differing
  (`autumn-macros/src/step_up.rs::build_check_call`), so the bare-form case
  pinned here is representative; not duplicated as a second MCP test since
  `step_up.rs`'s own macro-expansion tests already cover the `max_age` forms
  independently of MCP.
- Feature-independent: the guard macro and MCP dispatch path are both
  default-feature-set code (`mcp` only gates whether MCP compiles at all),
  so this reproduces (or, here, fails to reproduce) identically under every
  feature combination that compiles `mcp` + `#[step_up]` together.

## 📜 Compatibility

No behavior change, no CHANGELOG entry (test-only addition, matching this
repo's convention for negative-result commits — see
`docs/security/2026-09-10-mcp-secured-guard-dispatch/`).

## 🗂 Ledger

This directory. `after.txt` has the full green test and lint runs.
