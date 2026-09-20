# 2026-09-20 — `#[throttle(key = "ip")]` × MCP `tools/call` dispatch (negative result)

## 🎯 Surface

`autumn_macros::throttle` (the macro-generated `FromRequestParts` rate-limit
guard, issue #1668) × `autumn_web::mcp` (`#[api_doc(mcp)]`, `mount_mcp`,
`tools/call` dispatch). Entry point investigated: a handler carrying both
`#[throttle(limit = N, per = "…", key = "ip")]` and `#[api_doc(mcp)]`, called
through MCP's JSON-RPC `tools/call` rather than a direct HTTP request.

## 🕵️ Threat model (hypothesis)

Against an app that follows Autumn's own documented, ordinary pattern — guard
a handler with `#[throttle(key = "ip")]` and separately opt it into the agent
surface with `#[api_doc(mcp)]`, exactly as `docs/guide/mcp.md` and
`mcp_endpoint.rs`'s own `create_guarded_todo` fixture show — an MCP client
with no prior interaction could exceed the route's declared abuse budget
(a login-style or signup-style handler, or any pre-auth mutating route)
without ever being rate-limited, if the per-route `#[throttle]` bucket failed
to identify the caller once the request travels through MCP's synthetic
server-to-self dispatch (`server.dispatch.clone().oneshot(request)`) instead
of a real accepted TCP connection.

Concretely: `#[throttle(key = "ip")]`'s gate reads
`ConnectInfo<SocketAddr>` from the request's extensions
(`autumn-macros/src/throttle.rs`), which axum normally populates from the
real accepted connection. A `tools/call` dispatch never goes through that
accept path — `mcp::apply_replay_extensions` builds the request's extensions
by hand. If that hand-built request carried no resolvable peer identity, the
existing fail-open branch in `__check_throttle`
(`autumn/src/security/rate_limit.rs`) — `extract_throttle_key` returning
`None` for "an in-process caller with no identifiable peer (SSG, tests
without ConnectInfo)" — would silently bypass the bucket for *every*
MCP-routed call to an IP-keyed throttle, regardless of how many times the
same caller called it. That would be the framework's own contract failing
exactly the way row 5 of the classification table describes: a documented
primitive (`#[throttle(key = "ip")]`, sold as bounding abuse) weaker than its
contract on one specific, undocumented dispatch path. It would also clear the
severity floor directly: "unbounded resource consumption reachable pre-auth."

This exact combination had no end-to-end coverage.
`mcp_endpoint.rs::tools_list_includes_a_body_guard_written_above_the_route_attribute`
stacks `#[throttle]` on an MCP-exposed handler, but only asserts the tool
still appears in the `tools/list` catalog — it never calls the tool enough
times to observe whether the 429 actually fires over MCP dispatch, or
whether two distinct callers get independent budgets.
`mcp_secured_guard.rs` (2026-09-10) proved a macro-generated *session* guard
survives MCP dispatch by the same "gate is part of the handler's real type
signature, and dispatch never bypasses the router" mechanism — but a
session guard and an IP-keyed rate-limit guard read different request state
(`Session`/`Cookie` vs. `ConnectInfo`/`X-Forwarded-For`), so that finding
does not, by itself, cover this one.

## 🧪 Reproduction attempt → negative result

Test: `autumn/tests/integration/mcp_throttle_guard.rs`:
- `throttle_denies_the_mcp_call_that_exceeds_the_per_ip_limit`
- `throttle_keys_independently_per_ip_over_mcp_dispatch`

```
cargo test -p autumn-web --test integration_tests --features "mcp,test-support" \
  mcp_throttle_guard -- --nocapture
```

Result: **pass, both directions** — no bypass. See `after.txt` for the full
run. A `#[throttle(limit = 2, per = "60s", key = "ip")]` handler exposed via
`#[api_doc(mcp)]`:

- allows the first 2 `tools/call` requests from one `X-Forwarded-For`
  identity, then reports the 3rd as `isError: true` carrying the handler's
  real `429` (`"handler returned HTTP 429: …"` — `mcp::buffered_tool_result`
  surfaces a non-2xx dispatched status rather than swallowing it);
- gives a second caller, identified by a *different* `X-Forwarded-For` value,
  its own independent budget rather than inheriting the first caller's
  exhausted bucket or a shared/absent identity.

## 🔎 Root cause of the fail-safe behavior

Two independent forwarding paths keep the real caller identity intact across
MCP dispatch, so the SSG/no-peer fail-open branch in `__check_throttle` is
never reached for a normal MCP client:

1. `mcp::FORWARDED_HEADERS` (`autumn/src/mcp.rs:96-110`) copies
   `x-forwarded-for` and `x-real-ip` verbatim from the `/mcp` envelope onto
   the synthetic dispatched request — the same headers
   `Limiter::extract_key`'s `client_ip` resolver already trusts for a direct
   HTTP call when `trust_forwarded_headers`/`trusted_proxies` is configured.
2. `mcp::apply_replay_extensions` (`autumn/src/mcp.rs:2038-2089`) separately
   inserts `ConnectInfo(ctx.peer)` — the **envelope's own real TCP peer**,
   captured from the actual `/mcp` connection's `ConnectInfo` extractor at
   `autumn/src/mcp.rs:1180-1222` — onto the dispatched request's extensions.
   So even an app that keys `#[throttle]` on `ConnectInfo` directly (rather
   than trusting forwarded headers) still sees a real, resolvable peer: the
   MCP caller's own connection to `/mcp`, not an absent or synthetic one.

Either mechanism alone would have been enough to give `extract_throttle_key`
a `Some(bucket_key)`; both together make the fail-open branch reachable only
in the case its own comment names — a genuinely in-process caller with no
request at all (SSG pre-rendering, or a test that doesn't set up
`ConnectInfo`/forwarded headers) — never a real MCP client hitting `/mcp`
over a real connection.

The comment at `autumn/src/mcp.rs:2073-2077` also confirms the adjacent
design decision doesn't reopen this: `RateLimitEnvelopeCounted` (set when the
`/mcp` envelope itself is rate-limited) is deliberately scoped to the shared
*global default* limiter bucket only — "User/per-route limiters (path
overrides, `#[throttle]`) don't share that bucket and still charge the
replay" — so a strict per-route budget is never silently relaxed to the
`/mcp` envelope's own (typically more permissive) limit either.

## 🩹 Fix

None — no bug found. Regression test added at
`autumn/tests/integration/mcp_throttle_guard.rs`, registered in
`autumn/tests/integration/mod.rs` under `#[cfg(feature = "mcp")]`. The test
pins the actual mechanism (the 3rd call from one IP is denied with the real
429 surfaced, not swallowed; a distinct IP gets an independent budget) so it
fails loudly — not vacuously — if a future change to MCP dispatch,
`#[throttle]`'s expansion, `FORWARDED_HEADERS`, or `apply_replay_extensions`'s
peer-forwarding ever reopens the fail-open branch for a real MCP caller.

## ✅ Verification

- `cargo fmt --all -- --check` — clean.
- `cargo test -p autumn-web --test integration_tests --features "mcp,test-support" mcp_throttle_guard` — 2/2 pass (`after.txt`).
- `cargo clippy -p autumn-web --test integration_tests --features "mcp,test-support" -- -D warnings` — clean (the one printed warning, `unknown lint: clippy::unused_async_trait_impl`, is pre-existing workspace lint-config noise unrelated to this file and does not fail the gate).
- Re-attack: tried keying on `principal`/`token` structurally — those read
  `RateLimitPrincipal`/session or the `Authorization` header, both of which
  `FORWARDED_HEADERS` and `apply_replay_extensions` also forward
  (`identity`/`Cookie`/`Authorization`), so the same "real identity survives
  dispatch" argument applies; not duplicated as a third test since this
  reproduction specifically targeted the `key = "ip"` case, the one that
  depends on connection-level state (`ConnectInfo`) rather than
  application-level state (session/header), and is therefore the one most
  plausible to have been dropped by a synthetic, in-process dispatch.

## 📡 Blast radius

- Checked the other two MCP tool-registration variants
  (`expose_all_as_mcp()`, manual `mount_mcp`): all three feed the same
  `derive_tools()` → `McpToolInfo` → single `server.dispatch` router and the
  same `apply_replay_extensions`, so none has an independent bypass.
- Checked the named-limiter form (`#[throttle("name")]`): it resolves its key
  strategy from `[security.rate_limit.named.<name>]` (defaulting to the
  global `key_strategy`) through the same `extract_throttle_key` call this
  test exercises for the inline form — same code path, no separate risk.
- Checked the SSG/no-peer fail-open branch itself is not disturbed: it still
  exists (deliberately) for a genuinely in-process caller with no request,
  and this finding does not touch or narrow it — only confirms real MCP
  traffic never falls into it.
- Feature-independent: `#[throttle]`, the rate-limit engine, and MCP dispatch
  are all default-feature-set code (`mcp` only gates whether MCP compiles at
  all; `redis` only changes the limiter *backend*, not the key-extraction
  path this test exercises), so this reproduces (or, here, fails to
  reproduce) identically under every feature combination that compiles `mcp`
  + `#[throttle]` together.

## 📜 Compatibility

No behavior change, no CHANGELOG entry (test-only addition, matching this
repo's convention for negative-result commits — see
`docs/security/2026-09-10-mcp-secured-guard-dispatch/`).

## 🗂 Ledger

This directory. `after.txt` has the full green test run and gate output.
