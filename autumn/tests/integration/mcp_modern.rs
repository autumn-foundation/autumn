//! Integration tests for MCP protocol revision `2026-07-28` (the modern era).
//!
//! A modern request carries its protocol version and client capabilities in
//! `params._meta`. There is no `initialize` handshake. Legacy clients keep
//! the `initialize` flow (see `mcp_endpoint`).

#![cfg(feature = "mcp")]

use std::convert::Infallible;

use autumn_web::prelude::*;
use autumn_web::sse::{Event, Sse};
use autumn_web::test::{TestApp, TestClient, TestResponse};
use futures::stream::{self, Stream};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const MODERN: &str = "2026-07-28";
const TASKS: &str = "io.modelcontextprotocol/tasks";

#[derive(Serialize, Deserialize)]
struct Report {
    rows: u32,
}

#[get("/api/report")]
#[api_doc(mcp, task, summary = "Build a report")]
async fn build_report() -> AutumnResult<Json<Report>> {
    Ok(Json(Report { rows: 3 }))
}

#[get("/api/quick")]
#[api_doc(mcp, summary = "Quick read")]
async fn quick_read() -> AutumnResult<Json<Report>> {
    Ok(Json(Report { rows: 1 }))
}

#[get("/api/search")]
#[api_doc(mcp, stream, summary = "Streaming search")]
async fn streaming_search() -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    Sse::new(stream::iter(vec![
        Ok(Event::default().data("match 1")),
        Ok(Event::default().data("match 2")),
    ]))
}

// ── Helpers ───────────────────────────────────────────────────────────

fn app() -> TestClient {
    TestApp::new()
        .routes(routes![build_report, quick_read, streaming_search])
        .mount_mcp("/mcp")
        .build()
}

fn meta(capabilities: &Value) -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": MODERN,
        "io.modelcontextprotocol/clientInfo": { "name": "test-client", "version": "1.0.0" },
        "io.modelcontextprotocol/clientCapabilities": capabilities,
    })
}

/// The header `Mcp-Name` mirrors, if the method has one.
fn mcp_name(method: &str, params: &Value) -> Option<String> {
    match method {
        "tools/call" => params["name"].as_str().map(str::to_owned),
        m if m.starts_with("tasks/") => params["taskId"].as_str().map(str::to_owned),
        _ => None,
    }
}

/// Send a conforming modern request: `_meta` plus the standard headers.
async fn modern(client: &TestClient, method: &str, mut params: Value) -> TestResponse {
    if params.get("_meta").is_none() {
        params["_meta"] = meta(&json!({}));
    }
    let mut req = client
        .post("/mcp")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", MODERN)
        .header("mcp-method", method);
    if let Some(name) = mcp_name(method, &params) {
        req = req.header("mcp-name", &name);
    }
    req.json(&json!({"jsonrpc":"2.0","id":7,"method":method,"params":params}))
        .send()
        .await
}

fn body(resp: &TestResponse) -> Value {
    resp.json::<Value>()
}

fn assert_server_info(result: &Value) {
    let info = &result["_meta"]["io.modelcontextprotocol/serverInfo"];
    assert_eq!(info["name"], "autumn-mcp", "{result}");
    assert!(info["version"].is_string(), "{result}");
}

// ── server/discover ───────────────────────────────────────────────────

#[tokio::test]
async fn discover_lists_versions_capabilities_and_identity() {
    let client = app();
    let resp = modern(&client, "server/discover", json!({})).await;
    resp.assert_ok();
    let out = body(&resp);
    let result = &out["result"];
    assert_eq!(out["id"], 7);
    assert_eq!(result["resultType"], "complete", "{out}");
    assert_eq!(result["supportedVersions"], json!([MODERN]), "{out}");
    assert_eq!(
        result["capabilities"]["tools"],
        json!({"listChanged": false})
    );
    assert_eq!(
        result["capabilities"]["extensions"][TASKS],
        json!({}),
        "{out}"
    );
    assert!(result["ttlMs"].as_u64().is_some(), "{out}");
    assert_eq!(result["cacheScope"], "private", "{out}");
    assert_server_info(result);
}

#[tokio::test]
async fn discover_without_meta_is_invalid_params() {
    let client = app();
    let resp = client
        .post("/mcp")
        .header("mcp-method", "server/discover")
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"server/discover"}))
        .send()
        .await;
    resp.assert_status(400);
    assert_eq!(body(&resp)["error"]["code"], -32602);
}

// ── tools/list and tools/call ─────────────────────────────────────────

#[tokio::test]
async fn tools_list_carries_result_type_and_cache_hints() {
    let client = app();
    let resp = modern(&client, "tools/list", json!({})).await;
    resp.assert_ok();
    let out = body(&resp);
    let result = &out["result"];
    assert_eq!(result["resultType"], "complete", "{out}");
    assert!(result["ttlMs"].as_u64().is_some(), "{out}");
    // The catalog can sit behind auth this module cannot see.
    assert_eq!(result["cacheScope"], "private", "{out}");
    assert_server_info(result);
    let names: Vec<&str> = result["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"quick_read"), "{names:?}");
}

#[tokio::test]
async fn initialize_with_the_modern_header_stays_legacy() {
    let client = app();
    let resp = client
        .post("/mcp")
        .header("mcp-protocol-version", MODERN)
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"initialize",
                      "params":{"protocolVersion":MODERN}}))
        .send()
        .await;
    resp.assert_ok();
    // Negotiated down to the newest legacy version.
    assert_eq!(body(&resp)["result"]["protocolVersion"], "2025-06-18");
}

#[tokio::test]
async fn duplicate_standard_headers_are_rejected() {
    let client = app();
    let resp = client
        .post("/mcp")
        .header("mcp-protocol-version", MODERN)
        .header("mcp-method", "tools/call")
        .header("mcp-name", "quick_read")
        .header("mcp-name", "build_report")
        .json(&json!({"jsonrpc":"2.0","id":4,"method":"tools/call",
                      "params":{"name":"quick_read","arguments":{},"_meta":meta(&json!({}))}}))
        .send()
        .await;
    resp.assert_status(400);
    assert_eq!(body(&resp)["error"]["code"], -32020);
}

#[tokio::test]
async fn malformed_modern_notification_is_invalid() {
    let client = app();
    let resp = client
        .post("/mcp")
        .header("mcp-protocol-version", MODERN)
        .json(&json!({"method": 5}))
        .send()
        .await;
    resp.assert_status(400);
    assert_eq!(body(&resp)["error"]["code"], -32600);
}

#[tokio::test]
async fn secured_endpoint_marks_the_tool_list_private() {
    use autumn_web::auth::{InMemoryApiTokenStore, RequireApiToken, issue_api_token};
    use std::sync::Arc;
    let store = Arc::new(InMemoryApiTokenStore::default());
    let token = issue_api_token(store.as_ref(), "agent:bot").await.unwrap();
    let client = TestApp::new()
        .routes(routes![quick_read])
        .mount_mcp("/mcp")
        .secure_mcp(RequireApiToken::new(store))
        .build();
    let resp = client
        .post("/mcp")
        .header("authorization", &format!("Bearer {token}"))
        .header("mcp-protocol-version", MODERN)
        .header("mcp-method", "tools/list")
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":meta(&json!({}))}}))
        .send()
        .await;
    resp.assert_ok();
    assert_eq!(body(&resp)["result"]["cacheScope"], "private");
}

#[tokio::test]
async fn tools_call_result_carries_result_type() {
    let client = app();
    let resp = modern(
        &client,
        "tools/call",
        json!({"name":"quick_read","arguments":{}}),
    )
    .await;
    resp.assert_ok();
    let out = body(&resp);
    let result = &out["result"];
    assert_eq!(result["resultType"], "complete", "{out}");
    assert_eq!(result["isError"], false, "{out}");
    assert_server_info(result);
}

#[tokio::test]
async fn streaming_result_carries_result_type() {
    let client = app();
    let resp = modern(
        &client,
        "tools/call",
        json!({"name":"streaming_search","arguments":{}}),
    )
    .await;
    resp.assert_ok();
    let text = resp.text();
    let last = text
        .lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .filter_map(|d| serde_json::from_str::<Value>(d.trim()).ok())
        .next_back()
        .expect("a final SSE message");
    assert_eq!(last["id"], 7, "{last}");
    assert_eq!(last["result"]["resultType"], "complete", "{last}");
    assert_server_info(&last["result"]);
}

// ── Tasks in the modern era ───────────────────────────────────────────

#[tokio::test]
async fn task_flow_works_in_the_modern_era() {
    let client = app();
    let caps = json!({"extensions": {TASKS: {}}});
    let created = body(
        &modern(
            &client,
            "tools/call",
            json!({"name":"build_report","arguments":{},"_meta":meta(&caps)}),
        )
        .await,
    );
    assert_eq!(created["result"]["resultType"], "task", "{created}");
    let id = created["result"]["taskId"].as_str().unwrap().to_owned();

    let mut done = Value::Null;
    for _ in 0..1000 {
        done = body(
            &modern(
                &client,
                "tasks/get",
                json!({"taskId": id, "_meta": meta(&caps)}),
            )
            .await,
        );
        if done["result"]["status"] == "completed" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(done["result"]["status"], "completed", "{done}");
    assert_eq!(done["result"]["resultType"], "complete", "{done}");
    assert_eq!(done["result"]["result"]["resultType"], "complete", "{done}");
}

#[tokio::test]
async fn tasks_without_the_capability_is_a_400() {
    let client = app();
    let resp = modern(&client, "tasks/get", json!({"taskId": "x"})).await;
    resp.assert_status(400);
    assert_eq!(body(&resp)["error"]["code"], -32021);
}

// ── Validation ────────────────────────────────────────────────────────

#[tokio::test]
async fn missing_client_capabilities_is_invalid_params() {
    let client = app();
    let resp = modern(
        &client,
        "tools/list",
        json!({"_meta": {"io.modelcontextprotocol/protocolVersion": MODERN}}),
    )
    .await;
    resp.assert_status(400);
    assert_eq!(body(&resp)["error"]["code"], -32602);
}

#[tokio::test]
async fn modern_header_without_meta_is_invalid_params() {
    let client = app();
    let resp = client
        .post("/mcp")
        .header("mcp-protocol-version", MODERN)
        .header("mcp-method", "tools/list")
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}))
        .send()
        .await;
    resp.assert_status(400);
    assert_eq!(body(&resp)["error"]["code"], -32602);
}

#[tokio::test]
async fn unsupported_version_lists_the_supported_ones() {
    let client = app();
    let mut meta = meta(&json!({}));
    meta["io.modelcontextprotocol/protocolVersion"] = json!("2099-01-01");
    let resp = client
        .post("/mcp")
        .header("mcp-protocol-version", "2099-01-01")
        .header("mcp-method", "tools/list")
        .json(&json!({"jsonrpc":"2.0","id":3,"method":"tools/list","params":{"_meta":meta}}))
        .send()
        .await;
    resp.assert_status(400);
    let out = body(&resp);
    assert_eq!(out["id"], 3);
    assert_eq!(out["error"]["code"], -32022, "{out}");
    assert_eq!(out["error"]["data"]["supported"], json!([MODERN]), "{out}");
    assert_eq!(out["error"]["data"]["requested"], "2099-01-01", "{out}");
}

/// Send a modern `tools/call` with the given header overrides.
async fn call_with_headers(client: &TestClient, headers: &[(&str, &str)]) -> TestResponse {
    let mut req = client.post("/mcp");
    for (name, value) in headers {
        req = req.header(name, value);
    }
    req.json(&json!({"jsonrpc":"2.0","id":4,"method":"tools/call",
                     "params":{"name":"quick_read","arguments":{},"_meta":meta(&json!({}))}}))
        .send()
        .await
}

#[tokio::test]
async fn header_mismatches_are_rejected() {
    let client = app();
    let cases: [&[(&str, &str)]; 6] = [
        // No MCP-Protocol-Version.
        &[("mcp-method", "tools/call"), ("mcp-name", "quick_read")],
        // MCP-Protocol-Version differs from `_meta`.
        &[
            ("mcp-protocol-version", "2025-06-18"),
            ("mcp-method", "tools/call"),
            ("mcp-name", "quick_read"),
        ],
        // No Mcp-Method.
        &[("mcp-protocol-version", MODERN), ("mcp-name", "quick_read")],
        // Mcp-Method differs from the body.
        &[
            ("mcp-protocol-version", MODERN),
            ("mcp-method", "tools/list"),
            ("mcp-name", "quick_read"),
        ],
        // No Mcp-Name on tools/call.
        &[
            ("mcp-protocol-version", MODERN),
            ("mcp-method", "tools/call"),
        ],
        // Mcp-Name differs from params.name.
        &[
            ("mcp-protocol-version", MODERN),
            ("mcp-method", "tools/call"),
            ("mcp-name", "build_report"),
        ],
    ];
    for headers in cases {
        let resp = call_with_headers(&client, headers).await;
        resp.assert_status(400);
        let out = body(&resp);
        assert_eq!(out["error"]["code"], -32020, "{headers:?}: {out}");
        assert_eq!(out["id"], 4, "{out}");
    }
}

#[tokio::test]
async fn base64_mcp_name_is_decoded() {
    let client = app();
    // "quick_read" in Base64.
    let resp = call_with_headers(
        &client,
        &[
            ("mcp-protocol-version", MODERN),
            ("mcp-method", "tools/call"),
            ("mcp-name", "=?base64?cXVpY2tfcmVhZA==?="),
        ],
    )
    .await;
    resp.assert_ok();
    assert_eq!(body(&resp)["result"]["resultType"], "complete");
}

#[tokio::test]
async fn unknown_method_is_a_404() {
    // `initialize` selects the legacy era: see
    // `initialize_with_the_modern_header_stays_legacy`.
    let client = app();
    for method in ["ping", "resources/list", "subscriptions/listen"] {
        let resp = modern(&client, method, json!({})).await;
        resp.assert_status(404);
        assert_eq!(body(&resp)["error"]["code"], -32601, "{method}");
    }
}

#[tokio::test]
async fn modern_batch_is_refused() {
    let client = app();
    let resp = client
        .post("/mcp")
        .header("mcp-protocol-version", MODERN)
        .json(&json!([{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":meta(&json!({}))}}]))
        .send()
        .await;
    resp.assert_status(400);
    assert_eq!(body(&resp)["error"]["code"], -32600);
}

#[tokio::test]
async fn modern_errors_without_an_id_omit_it() {
    let client = app();
    // A batch has no single id.
    let batch = client
        .post("/mcp")
        .header("mcp-protocol-version", MODERN)
        .json(&json!([{"jsonrpc":"2.0","id":1,"method":"tools/list"}]))
        .send()
        .await;
    batch.assert_status(400);
    assert!(body(&batch).get("id").is_none(), "{}", batch.text());
    // A body that is not JSON.
    let garbage = client
        .post("/mcp")
        .header("mcp-protocol-version", MODERN)
        .header("content-type", "application/json")
        .body("{not json")
        .send()
        .await;
    garbage.assert_status(400);
    let out = body(&garbage);
    assert_eq!(out["error"]["code"], -32700, "{out}");
    assert!(out.get("id").is_none(), "{out}");
}

#[tokio::test]
async fn unsupported_version_wins_over_missing_capabilities() {
    let client = app();
    let resp = client
        .post("/mcp")
        .header("mcp-protocol-version", "2027-01-01")
        .header("mcp-method", "tools/list")
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"tools/list",
                      "params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2027-01-01"}}}))
        .send()
        .await;
    resp.assert_status(400);
    assert_eq!(body(&resp)["error"]["code"], -32022);
}

#[tokio::test]
async fn fractional_id_is_invalid() {
    let client = app();
    let resp = client
        .post("/mcp")
        .header("mcp-protocol-version", MODERN)
        .header("mcp-method", "tools/list")
        .json(&json!({"jsonrpc":"2.0","id":1.5,"method":"tools/list","params":{"_meta":meta(&json!({}))}}))
        .send()
        .await;
    resp.assert_status(400);
    assert_eq!(body(&resp)["error"]["code"], -32600);
}

#[tokio::test]
async fn unserved_method_is_a_404_even_without_mcp_name() {
    let client = TestApp::new()
        .routes(routes![quick_read])
        .mount_mcp("/mcp")
        .build();
    // `resources/read` names `Mcp-Name`, but the server does not serve it.
    // No task tool here, so `tasks/get` is not served either.
    for (method, params) in [
        (
            "resources/read",
            json!({"uri": "file:///x", "_meta": meta(&json!({}))}),
        ),
        (
            "tasks/get",
            json!({"taskId": "x", "_meta": meta(&json!({"extensions": {TASKS: {}}}))}),
        ),
    ] {
        let resp = client
            .post("/mcp")
            .header("mcp-protocol-version", MODERN)
            .header("mcp-method", method)
            .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
            .send()
            .await;
        resp.assert_status(404);
        assert_eq!(body(&resp)["error"]["code"], -32601, "{method}");
    }
}

#[tokio::test]
async fn streaming_response_disables_proxy_buffering() {
    let client = app();
    let resp = modern(
        &client,
        "tools/call",
        json!({"name":"streaming_search","arguments":{}}),
    )
    .await;
    resp.assert_ok();
    assert_eq!(resp.header("x-accel-buffering"), Some("no"));
}

#[tokio::test]
async fn legacy_initialize_still_works() {
    let client = app();
    let resp = client
        .post("/mcp")
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"initialize",
                      "params":{"protocolVersion":"2025-06-18"}}))
        .send()
        .await;
    resp.assert_ok();
    let out = body(&resp);
    assert_eq!(out["result"]["protocolVersion"], "2025-06-18", "{out}");
    assert!(out["result"].get("resultType").is_none(), "{out}");
}
