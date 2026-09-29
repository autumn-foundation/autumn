//! Regression test: does `#[secured]`'s macro-generated session guard — a
//! hidden `FromRequestParts` gate inserted ahead of the handler's own
//! parameters (issue #1668) — actually run when the guarded handler is also
//! tagged `#[api_doc(mcp)]` and dispatched through MCP's `tools/call`,
//! rather than called directly over HTTP?
//!
//! This combination has no prior coverage: `tools_call_enforces_bearer_token_via_real_pipeline`
//! in `mcp_endpoint.rs` proves a *tower-layer* guard (`RequireApiToken`)
//! survives MCP dispatch, but no existing test stacks a macro-generated
//! guard like `#[secured]` on an MCP-exposed handler. `mcp.rs::build_request`
//! forwards the caller's `Cookie` header into the synthetic dispatched
//! request, and `serve_tools_call` sends that request through
//! `server.dispatch.clone().oneshot(request)` — the same fully-assembled
//! axum `Router` a direct HTTP call would traverse — so `#[secured]`'s
//! extractor should resolve identically either way. Pinning that as a
//! regression gate rather than leaving it merely "structurally sound".

#![cfg(feature = "mcp")]

use autumn_web::prelude::*;
use autumn_web::test::{TestApp, TestClient};

#[get("/secured-mcp-tool")]
#[secured]
#[api_doc(mcp, summary = "Return a secret only a logged-in caller may read")]
async fn secured_mcp_tool() -> Json<&'static str> {
    Json("secured-mcp-tool-secret")
}

async fn call_secured_tool(client: &TestClient) -> serde_json::Value {
    let resp = client
        .post("/mcp")
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": "secured_mcp_tool", "arguments": {}}
        }))
        .send()
        .await;
    resp.assert_ok();
    resp.json::<serde_json::Value>()
}

/// An unauthenticated MCP caller must not reach the handler: no session
/// cookie is forwarded, so `#[secured]`'s guard must reject the dispatched
/// request before the handler body ever runs, exactly as a direct HTTP call
/// with no cookie would.
#[tokio::test]
async fn secured_guard_rejects_an_unauthenticated_mcp_tool_call() {
    let client = TestApp::new()
        .routes(routes![secured_mcp_tool])
        .mount_mcp("/mcp")
        .build();

    let out = call_secured_tool(&client).await;

    assert_eq!(
        out["result"]["isError"], true,
        "an unauthenticated tools/call against a #[secured] handler must be \
         reported as a tool error, not succeed: {out}"
    );
    let text = out["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(
        !text.contains("secured-mcp-tool-secret"),
        "the guarded secret must never appear when the guard rejects the call: {text}"
    );
}

/// A caller with a valid session (established the same way `acting_as`/
/// `login_as` would for a direct HTTP request) reaches the handler exactly
/// as a direct HTTP call would: `#[secured]`'s bare form only requires an
/// authenticated session, so the tool call succeeds and returns the real
/// response.
#[tokio::test]
async fn secured_guard_allows_an_authenticated_mcp_tool_call() {
    let client = TestApp::new()
        .routes(routes![secured_mcp_tool])
        .mount_mcp("/mcp")
        .build();
    client.login_as("user-1").await;

    let out = call_secured_tool(&client).await;

    assert_ne!(
        out["result"]["isError"], true,
        "an authenticated tools/call against a #[secured] handler must succeed: {out}"
    );
    let text = out["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("secured-mcp-tool-secret"),
        "the real handler response must come through once the guard passes: {text}"
    );
}
