//! Regression test: does `#[throttle(key = "ip")]`'s macro-generated
//! `FromRequestParts` gate (issue #1668) actually enforce its bucket — keyed
//! by the *real* client IP, not collapsed to a shared or absent identity —
//! when the guarded handler is also tagged `#[api_doc(mcp)]` and dispatched
//! through MCP's `tools/call`, rather than called directly over HTTP?
//!
//! This combination had no end-to-end coverage. `mcp_endpoint.rs`'s
//! `tools_list_includes_a_body_guard_written_above_the_route_attribute` stacks
//! `#[throttle]` above an MCP-exposed handler, but only asserts the tool is
//! still listed in the catalog — it never actually calls the tool enough
//! times to observe whether the 429 fires, or whether two distinct callers
//! get independent budgets. `mcp_secured_guard.rs` proved a macro-generated
//! *session* guard survives MCP dispatch; this is the analogous proof for a
//! macro-generated *rate-limit* guard, which matters for a different reason:
//! if the per-route bucket silently failed to identify the caller over MCP
//! dispatch (e.g. because the peer address is not a real TCP connection but a
//! server-to-self replay — see `mcp::apply_replay_extensions`), `#[throttle]`
//! would fail *open* for every MCP-routed call to a guard meant to bound
//! abuse (severity floor: "unbounded resource consumption reachable
//! pre-auth"), even though the app author wrote the exact guard the docs show.
//!
//! Two independent mechanisms carry the caller's real identity across
//! dispatch, and this file exercises both:
//! - `mcp::FORWARDED_HEADERS` forwards `x-forwarded-for`/`x-real-ip` onto the
//!   synthetic dispatched request, used when the app trusts forwarded headers
//!   (`throttle_denies_the_mcp_call_that_exceeds_the_per_ip_limit`,
//!   `throttle_keys_independently_per_ip_over_mcp_dispatch`).
//! - `mcp::apply_replay_extensions` separately forwards the *envelope's own*
//!   `ConnectInfo` peer — the path a **default-config** app (no
//!   `trust_forwarded_headers`, the common case) actually depends on
//!   (`throttle_denies_the_mcp_call_via_default_config_connect_info`). A test
//!   using only `X-Forwarded-For` would still pass if this second mechanism
//!   were removed, since header forwarding alone would carry the key — so
//!   this third test injects `ConnectInfo` directly on a hand-built request,
//!   the same way `security/rate_limit_xff_bypass.rs` does, to exercise that
//!   path in isolation with header-trust off.

#![cfg(feature = "mcp")]

use std::net::SocketAddr;

use autumn_web::config::AutumnConfig;
use autumn_web::prelude::*;
use autumn_web::test::{TestApp, TestClient};
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::Request;
use tower::ServiceExt as _;

#[get("/throttled-mcp-tool")]
#[throttle(limit = 2, per = "60s", key = "ip")]
#[api_doc(mcp, summary = "A rate-limited tool keyed by client IP")]
async fn throttled_mcp_tool() -> Json<&'static str> {
    Json("throttled-mcp-tool-ok")
}

fn throttle_config() -> AutumnConfig {
    let mut config = AutumnConfig::default();
    // Tests have no real TCP peer, so `#[throttle(key = "ip")]` must resolve
    // the caller from `X-Forwarded-For` instead — mirroring
    // `secure_mcp_rejections_are_rate_limited` in `mcp_endpoint.rs`.
    config.security.rate_limit.trust_forwarded_headers = true;
    config
}

async fn call_throttled_tool(client: &TestClient, xff: &str) -> serde_json::Value {
    let resp = client
        .post("/mcp")
        .header("x-forwarded-for", xff)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": "throttled_mcp_tool", "arguments": {}}
        }))
        .send()
        .await;
    resp.assert_ok();
    resp.json::<serde_json::Value>()
}

/// A single caller (identified by `X-Forwarded-For`) making more `tools/call`
/// requests than the route's `limit` must be throttled on the call that
/// exceeds it — proving the bucket is actually consulted and decremented on
/// the MCP dispatch path, not silently bypassed the way an unresolvable peer
/// bypasses it (`extract_throttle_key` returning `None`).
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn throttle_denies_the_mcp_call_that_exceeds_the_per_ip_limit() {
    // Isolate the process-global `#[throttle]` registry: take the shared
    // TEST_LOCK FIRST, then clear it, holding the guard for the whole test so
    // no sibling repopulates or drops per-principal buckets mid-assertion —
    // see `throttle_route.rs`'s `throttled_route_429s_after_burst_while_sibling_route_unaffected`
    // for the established idiom. Every registry-touching test must hold the
    // lock, since the reset clears the entire process-wide registry.
    let _throttle_lock = autumn_web::security::TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    autumn_web::security::__throttle_registry_reset();

    let client = TestApp::new()
        .routes(routes![throttled_mcp_tool])
        .config(throttle_config())
        .mount_mcp("/mcp")
        .build();

    let first = call_throttled_tool(&client, "203.0.113.50").await;
    assert_ne!(
        first["result"]["isError"], true,
        "call 1/2 within the limit must succeed: {first}"
    );

    let second = call_throttled_tool(&client, "203.0.113.50").await;
    assert_ne!(
        second["result"]["isError"], true,
        "call 2/2 within the limit must succeed: {second}"
    );

    let third = call_throttled_tool(&client, "203.0.113.50").await;
    assert_eq!(
        third["result"]["isError"], true,
        "the 3rd call from the same IP must be throttled (limit = 2): {third}"
    );
    let text = third["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(
        text.contains("429"),
        "the tool error must surface the handler's 429, not swallow it: {text}"
    );
}

/// A second caller with a *different* `X-Forwarded-For` must get its own,
/// independent budget rather than sharing (or being silently exempted from)
/// the first caller's bucket — proving the real per-call identity is threaded
/// through MCP dispatch rather than collapsed to one shared value.
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn throttle_keys_independently_per_ip_over_mcp_dispatch() {
    let _throttle_lock = autumn_web::security::TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    autumn_web::security::__throttle_registry_reset();

    let client = TestApp::new()
        .routes(routes![throttled_mcp_tool])
        .config(throttle_config())
        .mount_mcp("/mcp")
        .build();

    // Exhaust the first caller's budget.
    let _ = call_throttled_tool(&client, "203.0.113.60").await;
    let _ = call_throttled_tool(&client, "203.0.113.60").await;
    let exhausted = call_throttled_tool(&client, "203.0.113.60").await;
    assert_eq!(
        exhausted["result"]["isError"], true,
        "first caller must be throttled after exhausting its own budget: {exhausted}"
    );

    // A different caller must still have its own budget available.
    let other = call_throttled_tool(&client, "198.51.100.7").await;
    assert_ne!(
        other["result"]["isError"], true,
        "a distinct IP must not inherit another caller's exhausted MCP-dispatch bucket: {other}"
    );
}

/// Build a raw `tools/call` POST to `/mcp`, with `peer` inserted directly as
/// a `ConnectInfo` extension the way a real accepted TCP connection would
/// carry it — `TestClient` itself dispatches through `tower::Service::oneshot`
/// with no listener, so it never populates `ConnectInfo`, and this handler has
/// no forwarded-header configuration to fall back on. This is the same
/// hand-built-`Request` + injected-`ConnectInfo` pattern
/// `security/rate_limit_xff_bypass.rs` uses to drive the global rate limiter's
/// peer-resolution path without a real socket.
fn mcp_call_with_connect_info(peer: SocketAddr) -> Request<Body> {
    let body = serde_json::to_vec(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {"name": "throttled_mcp_tool", "arguments": {}}
    }))
    .expect("request body serializes");
    let mut req = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("content-type", "application/json")
        .body(Body::from(body))
        .expect("request builds");
    req.extensions_mut().insert(ConnectInfo(peer));
    req
}

async fn call_throttled_tool_via_connect_info(
    router: &axum::Router,
    peer: SocketAddr,
) -> serde_json::Value {
    let response = router
        .clone()
        .oneshot(mcp_call_with_connect_info(peer))
        .await
        .expect("request dispatches");
    assert!(
        response.status().is_success(),
        "the outer /mcp envelope response must be 200 even when the tool call \
         itself is throttled: {}",
        response.status()
    );
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("response body reads");
    serde_json::from_slice(&bytes).expect("response body is valid JSON")
}

/// The production-default path: `trust_forwarded_headers` is `false` (the
/// default) and no `X-Forwarded-For`/`X-Real-Ip` header is sent, so
/// `#[throttle(key = "ip")]` can only resolve the caller through the
/// `ConnectInfo` `mcp::apply_replay_extensions` forwards from the envelope's
/// own connection. Removing that forwarding line — the exact regression this
/// file exists to catch — would make `extract_throttle_key` return `None` for
/// every call here and both tool calls in the loop below would report success,
/// so this test fails loudly on that regression in a way the
/// `X-Forwarded-For`-based tests above cannot (they would keep passing on
/// header forwarding alone).
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn throttle_denies_the_mcp_call_via_default_config_connect_info() {
    let _throttle_lock = autumn_web::security::TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    autumn_web::security::__throttle_registry_reset();

    // Default config: `trust_forwarded_headers` is false, no trusted proxies.
    let router = TestApp::new()
        .routes(routes![throttled_mcp_tool])
        .mount_mcp("/mcp")
        .build()
        .into_router();

    let peer: SocketAddr = "203.0.113.70:4000".parse().unwrap();

    let first = call_throttled_tool_via_connect_info(&router, peer).await;
    assert_ne!(
        first["result"]["isError"], true,
        "call 1/2 within the limit must succeed: {first}"
    );
    let second = call_throttled_tool_via_connect_info(&router, peer).await;
    assert_ne!(
        second["result"]["isError"], true,
        "call 2/2 within the limit must succeed: {second}"
    );
    let third = call_throttled_tool_via_connect_info(&router, peer).await;
    assert_eq!(
        third["result"]["isError"], true,
        "the 3rd call from the same ConnectInfo peer must be throttled (limit = 2), \
         with no X-Forwarded-For trust configured: {third}"
    );

    // A different peer must get its own, independent budget.
    let other_peer: SocketAddr = "198.51.100.8:4000".parse().unwrap();
    let other = call_throttled_tool_via_connect_info(&router, other_peer).await;
    assert_ne!(
        other["result"]["isError"], true,
        "a distinct ConnectInfo peer must not inherit another peer's exhausted bucket: {other}"
    );
}
