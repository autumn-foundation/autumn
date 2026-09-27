# 2026-09-10 — `#[secured]` × MCP `tools/call` dispatch (negative result)

## 🎯 Surface

`autumn_macros::secured` (the macro-generated `FromRequestParts` session/role
guard, issue #1668) × `autumn_web::mcp` (`#[api_doc(mcp)]`, `mount_mcp`,
`tools/call` dispatch). Entry point investigated: a handler carrying both
`#[secured]` (or `#[secured("role")]`/`#[secured(scopes = [...])]`) and
`#[api_doc(mcp)]`, called through MCP's JSON-RPC `tools/call` rather than a
direct HTTP request.

## 🕵️ Threat model (hypothesis)

Against an app that follows Autumn's own documented, ordinary pattern —
guard a handler with `#[secured]` and separately opt it into the agent
surface with `#[api_doc(mcp)]`, exactly as `docs/guide/mcp.md` and
`examples/todo-app` show for other guards — an attacker who is an MCP client
with **no session** (no valid `Cookie`, no prior login) could reach the
guarded handler and read its response by calling it as a tool through
`POST /mcp` `tools/call` instead of the direct HTTP route, if the MCP
dispatch path skips `#[secured]`'s guard the way `#2608`
(`docs/security/2026-09-07-mcp-custom-layer-static-mode/`) found it skipping
`AppBuilder::layer` custom layers in SSG/ISR mode. The app author would have
done nothing wrong: they used `#[secured]` and `#[api_doc(mcp)]` exactly as
documented, entirely independently of each other.

This combination had no prior coverage. `mcp_endpoint.rs`'s
`tools_call_enforces_bearer_token_via_real_pipeline` proves a *tower-layer*
guard (`RequireApiToken`, mounted via `.scoped(...)`) survives MCP dispatch,
but no existing test stacks a *macro-generated* guard like `#[secured]` on an
MCP-exposed handler — the exact class of "does a route's HTTP guard still
run when the same handler is dispatched as a tool?" question the ranked MCP
surface calls out.

## 🧪 Reproduction attempt → negative result

Test: `autumn/tests/integration/mcp_secured_guard.rs`:
- `secured_guard_rejects_an_unauthenticated_mcp_tool_call`
- `secured_guard_allows_an_authenticated_mcp_tool_call`

```
cargo test -p autumn-web --test integration_tests --features "mcp,test-support" \
  mcp_secured_guard -- --nocapture
```

Result: **pass, both directions** — no bypass. See `after.txt` for the full
run. An unauthenticated `tools/call` against a `#[secured]`-guarded,
`#[api_doc(mcp)]`-tagged handler gets `isError: true` with no trace of the
handler's real response; a caller with a valid session (seeded the same way
`TestClient::login_as` seeds one for a direct HTTP test) gets the real
response.

(An earlier draft of this test used a `&'static str`-returning handler and
failed both cases with `"unknown tool: secured_mcp_tool"` — not a security
finding, a test-authoring mistake: MCP tool derivation requires a JSON
response, the same "JSON-only eligibility" rule `mcp_endpoint.rs`'s
`html_page` test already documents. Fixed by returning `Json<&'static str>`
before the reproduction attempt below ran for real.)

## 🔎 Root cause of the fail-safe behavior

`#[secured]` expands to a hidden `FromRequestParts` parameter inserted ahead
of the handler's own parameters (`autumn-macros/src/secured.rs`) — it is part
of the handler's real type signature, not a body statement, so axum resolves
it during extraction for *any* caller that reaches the handler through the
router, regardless of how the request was constructed.

`autumn_web::mcp::serve_tools_call` dispatches every `tools/call` through
`server.dispatch.clone().oneshot(request)` (`autumn/src/mcp.rs:2157`) —
`server.dispatch` is documented as "the fully-assembled application router"
and there is exactly one call site of `McpServer::new`/`build_mcp_router`, so
there is no registration path (`#[api_doc(mcp)]`, `expose_all_as_mcp()`,
manual `mount_mcp`) that calls a handler function directly instead of going
through the real tower/axum `Service` stack.

`mcp::build_request` (`autumn/src/mcp.rs:2700-2824`) reconstructs the
synthetic dispatched request from the caller's own headers via
`FORWARDED_HEADERS`, explicitly including `Cookie` ("session-based
`#[secured]` routes / session tenancy") and `Authorization` — so whatever
session or bearer credential the real MCP caller presented is exactly what
`#[secured]`'s extractor sees, the same as a direct HTTP call.

Together: the guard is unconditionally part of the handler's signature, and
MCP dispatch never bypasses the router that resolves it. There is no
gap analogous to the already-fixed `AppBuilder::layer`/SSG-ISR case, because
that bug was specifically about a Tower **layer** wrapped around the router
being skipped in one dispatch mode — `#[secured]` is not a layer, it is part
of the handler itself, so it has no "wrapped around" surface to skip.

## 🩹 Fix

None — no bug found. Regression test added at
`autumn/tests/integration/mcp_secured_guard.rs`, registered in
`autumn/tests/integration/mod.rs` under `#[cfg(feature = "mcp")]`. The test
pins the actual mechanism (`isError: true` with no leaked body on rejection;
the real response on success) so it fails loudly — not vacuously — if a
future change to MCP dispatch, `#[secured]`'s expansion, or `build_request`'s
header-forwarding ever reopens this path.

## ✅ Verification

- `cargo fmt --all -- --check` — clean.
- `cargo test -p autumn-web --test integration_tests --features "mcp,test-support" mcp_secured_guard` — 2/2 pass (`after.txt`).
- `cargo clippy -p autumn-web --test integration_tests --features "mcp,test-support" -- -D warnings` — clean.
- Re-attack: also checked `#[secured("role")]`/`#[secured(scopes = [...])]`
  forms structurally — same hidden-parameter mechanism, no separate dispatch
  path, so the bare-session case pinned here is representative; not
  duplicated as a second test since `secured_route.rs`'s existing
  macro-expansion tests already cover the role/scopes forms independently of
  MCP.

## 📡 Blast radius

- Checked all three MCP tool-registration variants
  (`#[api_doc(mcp)]`, `expose_all_as_mcp()`, manual `mount_mcp`): all three
  feed the same `derive_tools()` → `McpToolInfo` → single `server.dispatch`
  router, so none has an independent bypass.
- Checked the already-fixed SSG/ISR custom-layer case
  (`docs/security/2026-09-07-mcp-custom-layer-static-mode/`) is a different
  mechanism (a Tower layer wrapped around the router, skipped in one
  dispatch mode) and does not reopen here: `#[secured]` is not a layer.
- Feature-independent: the guard macro and MCP dispatch path are both
  default-feature-set code (`mcp` feature only gates whether MCP compiles at
  all), so this reproduces (or, here, fails to reproduce) identically under
  every feature combination that compiles `mcp` + `#[secured]` together.

## 📜 Compatibility

No behavior change, no CHANGELOG entry (test-only addition, matching this
repo's convention for negative-result commits — see
`docs/security/2026-09-06-idempotency-token-principal/`).

## 🗂 Ledger

This directory. `after.txt` has the full green test run.
