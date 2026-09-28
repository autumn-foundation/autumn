# 2026-09-28 — `AppBuilder::idempotent()` × MCP `tools/call` dispatch (negative result)

## 🎯 Surface

`autumn_web::idempotency::{IdempotencyLayer, IdempotencyReplayLayer}` (the
`AppBuilder::idempotent()`-driven dedupe of retried writes, issue #677) ×
`autumn_web::mcp` (`#[api_doc(mcp)]`, `mount_mcp`, `tools/call` dispatch).
Entry point investigated: a mutating handler carrying `#[api_doc(mcp)]`,
guarded app-wide by `.idempotent()`, called through MCP's JSON-RPC
`tools/call` rather than a direct HTTP request.

## 🕵️ Threat model (hypothesis)

Against an app that follows Autumn's own documented pattern — enable
`AppBuilder::idempotent()` and separately opt a mutating handler into the
agent surface with `#[api_doc(mcp)]`, exactly as `docs/guide/mcp.md` shows
for any other MCP tool — an MCP client (most plausibly an AI agent retrying
a `tools/call` after a dropped or timed-out response, the documented reason
`docs/guide/idempotency.md` gives for sending `Idempotency-Key` at all)
could get a write executed twice — a double charge, a duplicate order, a
second email — instead of the second attempt being served from the
idempotency cache, if the layer that owns replay-lookup/caching ever failed
to run on MCP's synthetic dispatch path (`mcp::apply_replay_extensions` /
`mcp::build_request`) the way `#2608`
(`docs/security/2026-09-07-mcp-custom-layer-static-mode/`) found
`AppBuilder::layer` custom layers skipping it in SSG/ISR mode.

`#[secured]` (`2026-09-10-mcp-secured-guard-dispatch`), `#[throttle]`
(`2026-09-20-mcp-throttle-guard-dispatch`), and `#[step_up]`
(`2026-09-27-mcp-step-up-guard-dispatch`) were each already proven to
survive this dispatch path by the same mechanism (`serve_tools_call`
dispatches through the real, fully-assembled router). Idempotency replay
had no dedicated coverage, and it is architecturally a different shape from
all three: it is a Tower `Layer` wrapped around the route's own
`MethodRouter` at macro-expansion time
(`autumn-macros/src/route.rs`'s `build_handler_expr`, which bakes
`IdempotencyReplayLayer` onto the handler unless a stacked guard already
owns replay) plus a second, app-wide `IdempotencyLayer` applied per
route/sub-router during route composition (`autumn/src/router.rs`'s
`group_and_mount_routes`/`mount_scoped_groups`/`mount_raw_routers`) — never
a hidden `FromRequestParts` extractor parameter the way the other three are.
So it was not obviously covered by those three proofs, and it is a
different failure mode if it did fail: not an authz/rate-limit bypass, but a
double-execution of a write the framework's own guarantee exists to
prevent.

## 🧪 Reproduction

Added `autumn/tests/integration/mcp_idempotency_guard.rs`:

- `idempotency_replay_prevents_double_execution_over_mcp_dispatch`: a
  `#[post]` handler tagged `#[api_doc(mcp)]`, mounted under an app with
  `.idempotent()`, called twice through `POST /mcp` `tools/call` with the
  same `Idempotency-Key` and the same body. Asserts the handler's own call
  counter is `1` (not `2`) and that both `tools/call` responses are
  byte-identical.
- `idempotency_replay_rejects_reused_key_with_different_payload_over_mcp_dispatch`:
  the same key reused with a *different* body must surface the layer's
  `422` payload-mismatch as a tool error, not silently replay the first
  call's stored response, and must not re-execute the handler either.

Command: `cargo test -p autumn-web --test integration_tests --features mcp
mcp_idempotency_guard -- --nocapture`

### A false failure, and how it was diagnosed

The first version of this test used **one shared `static AtomicU32` call
counter across both `#[tokio::test]` functions**, reusing the same handler
for both scenarios. `#[tokio::test]`s in this consolidated binary run
concurrently, so the two tests' `CHARGE_CALLS.store(0, ...)` resets and
`fetch_add`s could interleave. That produced a real, reproducible failure
on trunk:

```
thread 'integration::mcp_idempotency_guard::idempotency_replay_prevents_double_execution_over_mcp_dispatch' (11099) panicked at autumn/tests/integration/mcp_idempotency_guard.rs:109:5:
assertion `left == right` failed: the handler must run exactly once; the retry must be served from the idempotency cache instead of double-charging
  left: 2
 right: 1
test integration::mcp_idempotency_guard::idempotency_replay_prevents_double_execution_over_mcp_dispatch ... FAILED
test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 2276 filtered out; finished in 0.30s
```

That failure looked exactly like the vulnerability hypothesized above
(double execution, not a mismatch error), and would have been a real 🛡
Warden PR if it held up. Before writing that up, it was worth checking
whether the *lookup* mechanism worked at all: the sibling test in the same
run (reused key, *different* payload) passed and returned a genuine `422`
payload-mismatch — which is only possible if the layer successfully found
a cached entry for that key and compared its stored `body_hash` against the
new request's. That the mismatch path worked while the same-payload path
didn't pointed at the test's own setup rather than the framework: the two
`#[tokio::test]`s shared one handler function and one module-level
`static`, and running concurrently, one test's `CHARGE_CALLS.store(0, ...)`
could land between the other test's two calls and its assertion.

Reading `autumn/src/idempotency.rs`'s `handle_cache_miss`/`replay_cache_hit`
confirmed the layer's actual mechanism is sound for this shape: on a cache
miss it calls `store.try_set(&storage_key, record, body_hash, ttl)`
*synchronously* before unlocking (deferred-via-`SessionLayer` commit is
gated on `session.has_pending_changes()`, which is always `false` with no
`SessionLayer` registered — not this app's path); on a cache hit with a
matching `body_hash` it replays the stored response
(`response_from_record`, adding `X-Idempotent-Replayed: true`); on a
mismatch it returns `422` (`"idempotency key reused with different
payload"`). None of that depends on anything MCP-specific, and
`mcp::build_request` sets the dispatched request's URI to the tool's *real*
registered path (`tool.path_template`, filled from arguments) — the same
path a direct HTTP caller would use — so the storage key's `method`+`target`
components are identical between a direct call and an MCP-dispatched one.

Splitting the two scenarios onto separate handlers with separate counters
(`charge_mcp_tool_a`/`DOUBLE_EXECUTION_CHARGE_CALLS`,
`charge_mcp_tool_b`/`MISMATCHED_PAYLOAD_CHARGE_CALLS`) removed the race.
Both tests pass reliably on trunk — see `after.txt`.

## 🔎 Root cause (of the false failure, not of a framework bug)

`autumn/tests/integration/mcp_idempotency_guard.rs`'s first draft: one
`static CHARGE_CALLS: AtomicU32` shared by two concurrently-run
`#[tokio::test]`s. No production code was at fault.

## 🩹 Fix

No framework change. Fixed the test (separate handler + counter per
scenario) and committed both the corrected test and this negative-result
writeup.

## ✅ Verification

- `cargo test -p autumn-web --test integration_tests --features mcp
  mcp_idempotency_guard -- --nocapture` — 2 passed, 0 failed (`after.txt`).
- `cargo fmt --all`
- `cargo clippy --workspace --all-targets --features mcp -- -D warnings`

## 📡 Blast radius

Idempotency replay's per-route layering
(`IdempotencyLayer`/`IdempotencyReplayLayer`) is identical for every route
regardless of feature gate — there is no `redis`/`ws`/`mail`/`i18n`-specific
variant of this mechanism to sweep. The three sibling MCP-dispatch proofs
(`#[secured]`, `#[throttle]`, `#[step_up]`) remain the only other
macro-generated-guard-vs-MCP-dispatch checks; this closes the fourth and,
architecturally, most different shape (a Tower `Layer` rather than a
`FromRequestParts` extractor). No other released version is affected since
there was nothing to fix in `autumn-web` itself.

## 📜 Compatibility

No behavior change; test-only. No `CHANGELOG.md` entry (per `CLAUDE.md`:
write one for what a user of the framework can see — this changes nothing
they can see).

## 🗂 Ledger

`docs/security/2026-09-28-mcp-idempotency-guard-dispatch/`
