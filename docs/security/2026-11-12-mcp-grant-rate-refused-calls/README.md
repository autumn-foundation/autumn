# 2026-11-12: a refused MCP call spent the grant's rate cap

## 🎯 Surface

`autumn_web::mcp::serve_tools_call` and `run_task` (the `tools/call`
dispatcher), and `agent_authority::rate::RateLimiter`. Entry point: `POST
/mcp` `tools/call` on a tool whose route has `#[agent_operable(grant = G)]`
and whose grant declares `rate`.

## 🕵️ Threat model

Against an app that guards an MCP tool with `#[secured]` (or `#[authorize]`,
`#[throttle]`, CSRF) and caps its grant with `rate: "10/min"`, as
`docs/guide/agent-authority.md` shows, an MCP client with **no session** that
can reach `/mcp` can use up the cap with calls the route rejects. Every call
the route then admits is refused with `rate cap … exceeded` for as long as
the attacker keeps sending. The app author did nothing the guide warns
against. The guide says one bucket serves the whole grant. It does not say
that a call the route rejects spends it.

Class: pre-auth lockout of a governed action (availability). No data is read
or written.

## 🧪 Reproduction

`autumn/tests/integration/agent_authority.rs`,
`a_call_the_route_guard_rejects_does_not_spend_the_rate_cap`:

```
cargo test -p autumn-web --features "test-support,mcp" --test integration_tests \
  agent_authority::a_call_the_route_guard_rejects_does_not_spend_the_rate_cap
```

Three anonymous calls are rejected by `#[secured]`, and the handler counter
shows the handler never ran. Then `login_as("agent-1")` and one call. On trunk
the call is refused: `trunk-failure.txt`. After the fix it reaches the
handler: `after.txt`.

The consolidated binary was killed by the OOM killer on the 15 GB host used
for this run, so both runs compiled the module alone through a temporary
`[[test]]` that `#[path]`-includes `tests/integration/agent_authority.rs`. The
harness was not committed.

## 🔎 Root cause

`mcp.rs` `admit_call` runs `check_rate_cap` before
`server.dispatch.oneshot(request)`. The route's guards (`#[secured]`'s
`FromRequestParts` gate, CSRF, `#[throttle]`) run inside that dispatch, so the
token is gone before the guard decides. The bucket key is
`grant name + declaration site`. It has no caller part, by design.

## 🩹 Fix

`RateLimiter::refund` gives one token back, never more than the burst.
`refund_refused_call` calls it after dispatch when the route answered `401`,
`403` or `429`, in the buffered/streaming path and in the task path. The check
before dispatch is unchanged, so concurrent admitted calls still cannot get
past the cap.

Residual: a handler that does the action and *then* answers `403` gets its
token back. A handler does that only on purpose. Moving the check after the
guards would close it, but that changes what `#[agent_operable]` expands to.
That is a separate decision.

## 📡 Variant sweep

- `admit_call` is the only call site of `check_rate_cap`. `call_tool`
  (progressive catalog) unwraps to the same path (`prepare_call`).
- Task dispatch (`run_task`): same refund.
- MCP task store quota: keyed by client IP, so an attacker uses up only their
  own quota. A full global store makes calls run blocking, not refused.
- The envelope's IP limiter and `#[throttle]`: keyed per caller.
- `spend` is declared only and is not metered, so it has nothing to drain.
- Feature gates: `agent_authority::rate` is `mcp`-only. No other backend.

Affected: every release with grant `rate` enforcement in the MCP dispatcher
(issue #1691 onward).

## 📜 Compatibility

No public signature changes. `RateLimiter` is `pub(crate)`. Behaviour change:
refused calls no longer count against the cap. Admitted calls count as
before. Changelog fragment: `changelog.d/mcp-grant-rate-refused-calls.md`. The
guide's `rate` row now says this.

No `queries.txt` or manifest diff: the path issues no SQL, and the routes
audit classification does not change.
