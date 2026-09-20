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
//! `mcp::build_request` forwards `x-forwarded-for`/`x-real-ip` (`FORWARDED_HEADERS`)
//! and `apply_replay_extensions` forwards the envelope's own `ConnectInfo` peer
//! onto the synthetic dispatched request, so `#[throttle(key = "ip")]`'s
//! extractor should resolve the same client identity either way. Pinning that
//! as a regression gate rather than leaving it merely "structurally sound".

#![cfg(feature = "mcp")]

use autumn_web::config::AutumnConfig;
use autumn_web::prelude::*;
use autumn_web::test::{TestApp, TestClient};

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
async fn throttle_denies_the_mcp_call_that_exceeds_the_per_ip_limit() {
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
async fn throttle_keys_independently_per_ip_over_mcp_dispatch() {
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
