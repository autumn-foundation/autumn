//! Regression test: does `#[step_up]`'s macro-generated freshness guard — a
//! hidden `FromRequestParts` gate inserted ahead of the handler's own
//! parameters (issue #1668) — actually run when the guarded handler is also
//! tagged `#[api_doc(mcp)]` and dispatched through MCP's `tools/call`, rather
//! than called directly over HTTP?
//!
//! `mcp_secured_guard.rs` and `mcp_throttle_guard.rs` already proved this for
//! `#[secured]` (a session guard) and `#[throttle]` (a rate-limit guard); this
//! is the analogous proof for `#[step_up]`, which is a different shape again:
//! unlike the other two, its non-JSON rejection is a `302` redirect to
//! `/reauth` rather than a plain `401`/`403`/`429` status — worth pinning on
//! its own, since MCP's buffered-result packaging
//! (`mcp.rs::buffered_tool_result`) maps *any* non-2xx status to a tool error
//! via `StatusCode::is_success()`, and a redirect is exactly the status range
//! (`3xx`) that a narrower "is this a 4xx/5xx" check would have missed —
//! which would fail *open* on a session that is authenticated but merely
//! stale, letting a stolen session cookie run a step-up-gated action with no
//! fresh reauthentication.
//!
//! Two more properties matter here that `#[secured]` has no equivalent of:
//! the freshness check must actually consult wall-clock recency (a merely
//! *present* claim is not enough), and a JSON-accepting caller — which every
//! MCP `tools/call` dispatch is, since `build_request` sets `Accept:
//! application/json` — must see the JSON branch (`__step_up_json_response`,
//! `401`) rather than the HTML redirect branch, so the tool-result text is
//! useful to an agent rather than an opaque `302`.

#![cfg(feature = "mcp")]

use autumn_web::prelude::*;
use autumn_web::session::Session;
use autumn_web::test::{TestApp, TestClient};

#[post("/stamp-step-up-mcp")]
async fn stamp_step_up_mcp(session: Session) -> &'static str {
    autumn_web::step_up::set_last_strong_auth_at(&session).await;
    "stamped"
}

/// An ordinary, unguarded route whose only job is to make the session
/// middleware mint a session cookie with no step-up claim on it.
#[get("/touch-session-mcp")]
async fn touch_session_mcp(_session: Session) -> &'static str {
    "touched"
}

#[get("/step-up-mcp-tool")]
#[step_up]
#[api_doc(
    mcp,
    summary = "Return a secret only a freshly-reauthenticated caller may read"
)]
async fn step_up_mcp_tool() -> Json<&'static str> {
    Json("step-up-mcp-tool-secret")
}

async fn call_step_up_tool(client: &TestClient) -> serde_json::Value {
    let resp = client
        .post("/mcp")
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": "step_up_mcp_tool", "arguments": {}}
        }))
        .send()
        .await;
    resp.assert_ok();
    resp.json::<serde_json::Value>()
}

/// A caller with no session at all must not reach the handler: `#[step_up]`'s
/// gate has no `last_strong_auth_at` claim to find, so the dispatched request
/// is rejected before the handler body ever runs.
#[tokio::test]
async fn step_up_guard_rejects_an_mcp_tool_call_with_no_session() {
    let client = TestApp::new()
        .routes(routes![stamp_step_up_mcp, step_up_mcp_tool])
        .mount_mcp("/mcp")
        .build();

    let out = call_step_up_tool(&client).await;

    assert_eq!(
        out["result"]["isError"], true,
        "an MCP tools/call with no session against a #[step_up] handler must \
         be reported as a tool error, not succeed: {out}"
    );
    let text = out["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(
        !text.contains("step-up-mcp-tool-secret"),
        "the guarded secret must never appear when the guard rejects the call: {text}"
    );
}

/// A caller whose session exists but has never stamped `last_strong_auth_at`
/// (the shape of a session that is merely *old*, never freshly
/// reauthenticated) must also be rejected — proving the gate checks recency,
/// not just presence of a session.
#[tokio::test]
async fn step_up_guard_rejects_an_mcp_tool_call_with_a_session_but_no_fresh_claim() {
    let client = TestApp::new()
        .routes(routes![
            stamp_step_up_mcp,
            touch_session_mcp,
            step_up_mcp_tool
        ])
        .mount_mcp("/mcp")
        .build();
    // Establish a session cookie (via an ordinary, unguarded route) with no
    // step-up claim ever stamped on it.
    client.get("/touch-session-mcp").send().await.assert_ok();

    let out = call_step_up_tool(&client).await;

    assert_eq!(
        out["result"]["isError"], true,
        "an MCP tools/call with a session but no fresh step-up claim must be \
         reported as a tool error: {out}"
    );
}

/// A caller who freshly stamped `last_strong_auth_at` (the same way a reauth
/// form submission would for a direct HTTP request) reaches the handler
/// exactly as a direct HTTP call would: the tool call succeeds and returns
/// the real response.
#[tokio::test]
async fn step_up_guard_allows_an_mcp_tool_call_with_a_fresh_claim() {
    let client = TestApp::new()
        .routes(routes![stamp_step_up_mcp, step_up_mcp_tool])
        .mount_mcp("/mcp")
        .build();
    client.post("/stamp-step-up-mcp").send().await.assert_ok();

    let out = call_step_up_tool(&client).await;

    assert_ne!(
        out["result"]["isError"], true,
        "an MCP tools/call with a fresh step-up claim must succeed: {out}"
    );
    let text = out["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("step-up-mcp-tool-secret"),
        "the real handler response must come through once the guard passes: {text}"
    );
    // The gate's JSON branch (not the HTML `/reauth` redirect) must be what a
    // *failing* MCP call would have seen, since `build_request` sends `Accept:
    // application/json` — pinned indirectly by the rejection tests above
    // reporting a tool error rather than the outer `/mcp` envelope itself
    // failing to parse a redirect's empty body as a tool result.
    assert!(
        !text.contains("/reauth"),
        "a successful call must not carry any trace of the redirect branch: {text}"
    );
}
