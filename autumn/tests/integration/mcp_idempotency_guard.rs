//! Regression test: does `AppBuilder::idempotent()`'s macro-baked
//! `IdempotencyReplayLayer`/`IdempotencyLayer` (issue #677) actually dedupe a
//! retried write — keyed by the real `Idempotency-Key` header, not collapsed
//! to an absent or shared identity — when the guarded handler is also tagged
//! `#[api_doc(mcp)]` and dispatched through MCP's `tools/call`, rather than
//! called directly over HTTP?
//!
//! This combination had no end-to-end coverage. `mcp_secured_guard.rs`,
//! `mcp_throttle_guard.rs`, and `mcp_step_up_guard.rs` each proved a
//! macro-generated `FromRequestParts` gate survives MCP dispatch; idempotency
//! replay is architecturally different — a Tower `Layer` wrapped around the
//! route's own `MethodRouter` at macro-expansion time
//! (`autumn-macros/src/route.rs`'s `build_handler_expr`), not a hidden
//! extractor parameter — so it is not obviously covered by those same three
//! proofs. If the layer were ever skipped on MCP's synthetic dispatch path
//! (`mcp::apply_replay_extensions` / `mcp::build_request`), a client retrying
//! a `tools/call` after a dropped response (the documented reason to send
//! `Idempotency-Key` at all — `docs/guide/idempotency.md`) would double-charge
//! or double-send instead of replaying the cached response, even though the
//! app author wrote the exact guard the docs show.
//!
//! Both assertions below would fail loudly on that regression:
//! `idempotency_replay_prevents_double_execution_over_mcp_dispatch` if the
//! layer stopped engaging at all (the handler would run twice, producing two
//! different `charge_id`s), and
//! `idempotency_replay_rejects_reused_key_with_different_payload_over_mcp_dispatch`
//! if the layer's payload-fingerprint check were bypassed (a reused key with
//! a different body would silently replay the *first* payload's response
//! instead of surfacing `422`).

#![cfg(feature = "mcp")]

use std::sync::atomic::{AtomicU32, Ordering};

use autumn_web::prelude::*;
use autumn_web::test::{TestApp, TestClient};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct ChargeRequest {
    amount: i64,
}

static CHARGE_CALLS: AtomicU32 = AtomicU32::new(0);

/// Each execution returns a fresh `charge_id`, so two responses being
/// byte-identical proves the second call was replayed from the idempotency
/// cache rather than re-executed.
#[post("/charge-mcp-tool")]
#[api_doc(mcp, summary = "Charges a card; must not double-charge on retry")]
async fn charge_mcp_tool(Json(body): Json<ChargeRequest>) -> AutumnResult<Json<serde_json::Value>> {
    let charge_id = CHARGE_CALLS.fetch_add(1, Ordering::SeqCst);
    Ok(Json(serde_json::json!({
        "charge_id": charge_id,
        "amount": body.amount,
    })))
}

async fn call_charge_tool(
    client: &TestClient,
    idempotency_key: &str,
    amount: i64,
) -> serde_json::Value {
    let resp = client
        .post("/mcp")
        .header("idempotency-key", idempotency_key)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": "charge_mcp_tool", "arguments": {"body": {"amount": amount}}}
        }))
        .send()
        .await;
    resp.assert_ok();
    resp.json::<serde_json::Value>()
}

/// A retried MCP `tools/call` carrying the same `Idempotency-Key` and the
/// same payload must be served from the cache, not re-executed — proving
/// `IdempotencyLayer` is actually consulted on the MCP dispatch path.
#[tokio::test]
async fn idempotency_replay_prevents_double_execution_over_mcp_dispatch() {
    CHARGE_CALLS.store(0, Ordering::SeqCst);

    let client = TestApp::new()
        .idempotent()
        .routes(routes![charge_mcp_tool])
        .mount_mcp("/mcp")
        .build();

    let first = call_charge_tool(&client, "retry-key-1", 500).await;
    assert_ne!(
        first["result"]["isError"], true,
        "first call must succeed: {first}"
    );

    let second = call_charge_tool(&client, "retry-key-1", 500).await;
    assert_ne!(
        second["result"]["isError"], true,
        "replayed call must still report success: {second}"
    );

    assert_eq!(
        first, second,
        "a retried MCP tools/call with the same Idempotency-Key must return the byte-identical \
         cached response, not execute the handler again"
    );
    assert_eq!(
        CHARGE_CALLS.load(Ordering::SeqCst),
        1,
        "the handler must run exactly once; the retry must be served from the idempotency \
         cache instead of double-charging"
    );
}

/// Reusing an `Idempotency-Key` with a *different* payload over MCP dispatch
/// must be rejected (422), never silently replayed with the first call's
/// stored response for a different amount.
#[tokio::test]
async fn idempotency_replay_rejects_reused_key_with_different_payload_over_mcp_dispatch() {
    CHARGE_CALLS.store(0, Ordering::SeqCst);

    let client = TestApp::new()
        .idempotent()
        .routes(routes![charge_mcp_tool])
        .mount_mcp("/mcp")
        .build();

    let first = call_charge_tool(&client, "retry-key-2", 500).await;
    assert_ne!(
        first["result"]["isError"], true,
        "first call must succeed: {first}"
    );

    let second = call_charge_tool(&client, "retry-key-2", 999).await;
    assert_eq!(
        second["result"]["isError"], true,
        "a reused Idempotency-Key with a different payload must surface as a tool error \
         (422), not a silent replay of the first amount: {second}"
    );
    let text = second["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(
        text.contains("different payload"),
        "the tool error must surface the handler's 422 payload-mismatch body, not swallow it: \
         {text}"
    );
}
