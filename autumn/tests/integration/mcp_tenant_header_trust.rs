//! Regression gate: MCP `tools/call` replay cannot launder a forged tenant
//! header past `[tenancy] header_trust = "trusted_proxy"` (#3729).
//!
//! Attacker: any client that can reach `/mcp` directly (not through the
//! gateway), holding no credential for tenant B.
//! Defender: an app with `source = "header"`, `header_trust =
//! "trusted_proxy"` and a gateway range — the shape the tenant-sources guide
//! documents — that exposes a handler as a tool with `#[api_doc(mcp)]` and
//! mounts `mount_mcp("/mcp")`.
//! Flag: the handler's `CURRENT_TENANT` reading `tenant-b`.
//!
//! The tool call is a second request, built by the server and sent to its own
//! router. It forwards the tenant header and the envelope's `ConnectInfo`. If
//! the replay lost the peer, or put a loopback or a gateway address in its
//! place, the trust check would run against the wrong address. These tests
//! pin that the replay is checked against the caller's own peer.
//!
//! The second test puts `/mcp` in `tenancy.public_paths`, so the envelope
//! skips tenancy and only the replay can refuse. Without that, the envelope's
//! own check would refuse first and the replay would never be tested.

#![cfg(feature = "mcp")]

use std::net::SocketAddr;

use autumn_web::config::{AutumnConfig, TenantHeaderTrust};
use autumn_web::prelude::*;
use autumn_web::test::TestApp;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use tower::ServiceExt as _;

const FLAG: &str = "tenant-b-sentinel-3729";

#[get("/tenant-echo")]
#[api_doc(mcp, summary = "Report the tenant the request resolved to")]
async fn tenant_echo() -> Json<String> {
    Json(
        autumn_web::tenancy::CURRENT_TENANT
            .try_with(Clone::clone)
            .ok()
            .flatten()
            .unwrap_or_default(),
    )
}

/// A gateway at `10.0.0.0/8` sets the tenant header.
fn behind_gateway() -> AutumnConfig {
    let mut config = AutumnConfig::default();
    config.tenancy.enabled = true;
    "header".clone_into(&mut config.tenancy.source);
    config.tenancy.header_trust = Some(TenantHeaderTrust::TrustedProxy);
    config.security.trusted_proxies.trust_forwarded_headers = true;
    config.security.trusted_proxies.ranges = vec!["10.0.0.0/8".to_owned()];
    config
}

fn router(config: AutumnConfig) -> axum::Router {
    TestApp::new()
        .routes(routes![tenant_echo])
        .config(config)
        .mount_mcp("/mcp")
        .build()
        .into_router()
}

fn tools_call(peer: &str, tenant: &str) -> Request<Body> {
    let peer: SocketAddr = peer.parse().expect("socket address");
    let body = serde_json::to_vec(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {"name": "tenant_echo", "arguments": {}}
    }))
    .expect("request body serializes");
    let mut req = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("content-type", "application/json")
        .header("x-tenant-id", tenant)
        .body(Body::from(body))
        .expect("request builds");
    req.extensions_mut().insert(ConnectInfo(peer));
    req
}

async fn send(router: &axum::Router, req: Request<Body>) -> (StatusCode, String) {
    let response = router.clone().oneshot(req).await.expect("infallible");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 256 * 1024)
        .await
        .expect("body reads");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// Control: the gateway's own call reaches the tool with the tenant it set.
/// Without this, a test that sees no flag could be passing only because the
/// tool never ran.
#[tokio::test]
async fn the_gateway_selects_the_tenant_through_tools_call() {
    let app = router(behind_gateway());
    let (status, body) = send(&app, tools_call("10.0.0.5:4000", FLAG)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rpc: serde_json::Value = serde_json::from_str(&body).expect("json-rpc");
    assert_ne!(rpc["result"]["isError"], true, "{rpc}");
    assert!(
        body.contains(FLAG),
        "the gateway's tenant must reach the tool: {body}"
    );
}

/// A direct client is refused at the envelope, before any tool runs.
#[tokio::test]
async fn a_direct_client_cannot_select_the_tenant_through_tools_call() {
    let app = router(behind_gateway());
    let (status, body) = send(&app, tools_call("203.0.113.9:4000", FLAG)).await;
    assert!(
        !body.contains(FLAG),
        "a client outside the gateway range selected tenant B through MCP: {status} {body}"
    );
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
}

/// With `/mcp` exempt from tenancy, the envelope does not check the header.
/// The replay must still be checked against the caller's own peer, and the
/// tool must not run in tenant B.
#[tokio::test]
async fn the_replay_checks_the_callers_peer_when_the_envelope_is_exempt() {
    let mut config = behind_gateway();
    config.tenancy.public_paths = vec!["/mcp".to_owned()];
    let app = router(config);

    let (status, body) = send(&app, tools_call("203.0.113.9:4000", FLAG)).await;
    assert!(
        !body.contains(FLAG),
        "the tools/call replay trusted a forged tenant header from a direct client: \
         {status} {body}"
    );
    assert_eq!(status, StatusCode::OK, "the envelope is exempt: {body}");
    let rpc: serde_json::Value = serde_json::from_str(&body).expect("json-rpc");
    assert_eq!(rpc["result"]["isError"], true, "{rpc}");
    let text = rpc["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(text.contains("403"), "the tool error carries the 403: {text}");

    // The gateway still gets through on the same exempt envelope.
    let (_, body) = send(&app, tools_call("10.0.0.5:4000", FLAG)).await;
    assert!(body.contains(FLAG), "{body}");
}
