//! Integration tests for MCP progressive discovery.
//!
//! A core tool carries the `anthropic/alwaysLoad` marker. The server can send
//! `instructions`. In progressive mode `tools/list` holds the core tools and
//! two meta-tools: `search_tools` finds a tool, and `call_tool` runs it.

#![cfg(feature = "mcp")]

use std::convert::Infallible;

use autumn_web::Route;
use autumn_web::log::capture::{LogBuffer, LogCaptureLayer};
use autumn_web::log::filter::ParameterFilter;
use autumn_web::mcp::McpCatalog;
use autumn_web::openapi::OpenApiSchema;
use autumn_web::prelude::*;
use autumn_web::sse::{Event, Sse};
use autumn_web::test::{TestApp, TestClient, TestResponse};
use futures::stream::{self, Stream};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing_subscriber::layer::SubscriberExt as _;

const MODERN: &str = "2026-07-28";
const TASKS: &str = "io.modelcontextprotocol/tasks";
const ALWAYS_LOAD: &str = "anthropic/alwaysLoad";

#[derive(Serialize, Deserialize)]
struct Invoice {
    id: String,
    paid: bool,
}

#[derive(Serialize, Deserialize, OpenApiSchema)]
struct RefundRequest {
    reason: String,
}

#[derive(Serialize, Deserialize)]
struct Refund {
    id: String,
    reason: String,
}

#[derive(Serialize, Deserialize, OpenApiSchema)]
struct DocQuery {
    term: Option<String>,
}

#[get("/api/invoices")]
#[api_doc(mcp, core, tag = "billing", summary = "List unpaid invoices")]
async fn list_invoices() -> AutumnResult<Json<Vec<Invoice>>> {
    Ok(Json(vec![Invoice {
        id: "inv-1".into(),
        paid: false,
    }]))
}

#[post("/api/invoices/{id}/refund")]
#[api_doc(mcp, tag = "billing", summary = "Refund one invoice to the customer")]
async fn refund_invoice(
    Path(id): Path<String>,
    Json(req): Json<RefundRequest>,
) -> AutumnResult<Json<Refund>> {
    Ok(Json(Refund {
        id,
        reason: req.reason,
    }))
}

#[get("/api/docs")]
#[api_doc(mcp, core = false, tag = "docs", summary = "Find help articles")]
async fn find_articles(Query(q): Query<DocQuery>) -> AutumnResult<Json<Vec<String>>> {
    Ok(Json(vec![q.term.unwrap_or_default()]))
}

#[get("/api/secret")]
#[secured]
#[api_doc(mcp, tag = "reports", summary = "Read the audit secret")]
async fn secret_report() -> Json<&'static str> {
    Json("top-secret-value")
}

#[get("/api/report")]
#[api_doc(mcp, task, tag = "reports", summary = "Build the monthly report")]
async fn build_report() -> AutumnResult<Json<Invoice>> {
    Ok(Json(Invoice {
        id: "report".into(),
        paid: true,
    }))
}

#[get("/api/live")]
#[api_doc(mcp, stream, summary = "Stream live matches")]
async fn live_matches() -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    Sse::new(stream::iter(vec![
        Ok(Event::default().data("match 1")),
        Ok(Event::default().data("match 2")),
    ]))
}

#[get("/api/ping")]
async fn ping() -> AutumnResult<Json<&'static str>> {
    Ok(Json("pong"))
}

#[get("/api/core-only")]
#[api_doc(core, summary = "A core flag with no opt-in")]
async fn core_only() -> AutumnResult<Json<&'static str>> {
    Ok(Json("core-only"))
}

#[get("/api/clash")]
#[api_doc(
    mcp,
    operation_id = "search_tools",
    summary = "A user tool with a reserved name"
)]
async fn clashing_tool() -> AutumnResult<Json<&'static str>> {
    Ok(Json("user-tool"))
}

// ── Helpers ───────────────────────────────────────────────────────────

const INSTRUCTIONS: &str = "Billing and help-article tools.";

fn full_app() -> TestClient {
    TestApp::new()
        .routes(routes![list_invoices, refund_invoice, find_articles])
        .mount_mcp("/mcp")
        .build()
}

fn progressive_app() -> TestClient {
    TestApp::new()
        .routes(routes![
            list_invoices,
            refund_invoice,
            find_articles,
            secret_report,
            build_report
        ])
        .mcp_instructions(INSTRUCTIONS)
        .mcp_catalog(McpCatalog::Progressive)
        .mount_mcp("/mcp")
        .build()
}

/// Send one legacy JSON-RPC request and return the body.
async fn rpc(client: &TestClient, method: &str, params: Value) -> Value {
    let resp = client
        .post("/mcp")
        .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
        .send()
        .await;
    resp.assert_ok();
    resp.json::<Value>()
}

fn meta(capabilities: &Value) -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": MODERN,
        "io.modelcontextprotocol/clientCapabilities": capabilities,
    })
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
    // `Mcp-Name` mirrors the tool name, or the task ID of a `tasks/*` call.
    if let Some(name) = params["name"]
        .as_str()
        .or_else(|| params["taskId"].as_str())
    {
        req = req.header("mcp-name", name);
    }
    req.json(&json!({"jsonrpc":"2.0","id":7,"method":method,"params":params}))
        .send()
        .await
}

fn tools(list: &Value) -> &Vec<Value> {
    list["result"]["tools"].as_array().expect("a tools array")
}

fn tool<'a>(list: &'a Value, name: &str) -> &'a Value {
    tools(list)
        .iter()
        .find(|t| t["name"] == name)
        .unwrap_or_else(|| panic!("no tool `{name}` in {list}"))
}

fn names(list: &Value) -> Vec<String> {
    tools(list)
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_owned())
        .collect()
}

async fn search(client: &TestClient, arguments: Value) -> Value {
    rpc(
        client,
        "tools/call",
        json!({"name": "search_tools", "arguments": arguments}),
    )
    .await
}

fn found(out: &Value) -> Vec<String> {
    out["result"]["structuredContent"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("no structured tools in {out}"))
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_owned())
        .collect()
}

// ── The `core` flag ───────────────────────────────────────────────────

#[test]
fn core_attribute_sets_the_api_doc_flag() {
    let routes = routes![list_invoices, find_articles, core_only];
    assert!(routes[0].api_doc.mcp_core);
    assert!(!routes[1].api_doc.mcp_core, "`core = false` keeps it off");
    assert!(routes[2].api_doc.mcp_core && !routes[2].api_doc.mcp_tool);
}

#[test]
fn derived_tool_info_reports_the_core_flag() {
    let docs: Vec<_> = routes![list_invoices, refund_invoice]
        .into_iter()
        .map(|route| route.api_doc)
        .collect();
    let tools = autumn_web::mcp::derive_tools(&docs, false, None);
    assert!(tools[0].is_core());
    assert!(!tools[1].is_core());
}

#[tokio::test]
async fn core_tool_carries_the_always_load_marker() {
    let list = rpc(&full_app(), "tools/list", json!({})).await;
    assert_eq!(tool(&list, "list_invoices")["_meta"][ALWAYS_LOAD], true);
    assert!(
        tool(&list, "refund_invoice").get("_meta").is_none(),
        "{list}"
    );
}

#[tokio::test]
async fn core_marker_is_sent_in_the_modern_era() {
    let resp = modern(&full_app(), "tools/list", json!({})).await;
    resp.assert_ok();
    let list = resp.json::<Value>();
    assert_eq!(tool(&list, "list_invoices")["_meta"][ALWAYS_LOAD], true);
    assert!(
        tool(&list, "refund_invoice").get("_meta").is_none(),
        "{list}"
    );
}

#[tokio::test]
async fn route_builder_opts_in_a_plain_route_as_core() {
    let plain = routes![ping];
    assert!(!plain[0].api_doc.mcp_tool && !plain[0].api_doc.mcp_core);
    let routes = plain.into_iter().map(Route::mcp_core).collect::<Vec<_>>();
    assert!(routes[0].api_doc.mcp_tool && routes[0].api_doc.mcp_core);
    let client = TestApp::new().routes(routes).mount_mcp("/mcp").build();
    let list = rpc(&client, "tools/list", json!({})).await;
    assert_eq!(tool(&list, "ping")["_meta"][ALWAYS_LOAD], true);
}

// ── The full catalog ──────────────────────────────────────────────────

#[tokio::test]
async fn full_catalog_lists_every_tool_and_no_meta_tools() {
    let list = rpc(&full_app(), "tools/list", json!({})).await;
    assert_eq!(
        names(&list),
        ["list_invoices", "refund_invoice", "find_articles"]
    );
}

#[tokio::test]
async fn full_catalog_does_not_serve_meta_tools() {
    let client = full_app();
    for name in ["search_tools", "call_tool"] {
        let out = rpc(
            &client,
            "tools/call",
            json!({"name": name, "arguments": {"name": "list_invoices"}}),
        )
        .await;
        assert_eq!(out["error"]["code"], -32602, "{name} gave {out}");
    }
}

#[tokio::test]
async fn core_without_mcp_does_not_expose_the_route() {
    for catalog in [McpCatalog::Full, McpCatalog::Progressive] {
        let client = TestApp::new()
            .routes(routes![list_invoices, core_only])
            .mcp_catalog(catalog)
            .mount_mcp("/mcp")
            .build();
        let list = rpc(&client, "tools/list", json!({})).await;
        assert!(!names(&list).contains(&"core_only".to_owned()), "{list}");
        let out = rpc(
            &client,
            "tools/call",
            json!({"name": "core_only", "arguments": {}}),
        )
        .await;
        assert_eq!(out["error"]["code"], -32602, "{out}");
    }
}

#[tokio::test]
async fn core_under_the_hatch_carries_the_marker() {
    let client = TestApp::new()
        .routes(routes![core_only])
        .expose_all_as_mcp()
        .mount_mcp("/mcp")
        .build();
    let list = rpc(&client, "tools/list", json!({})).await;
    assert_eq!(tool(&list, "core_only")["_meta"][ALWAYS_LOAD], true);
}

#[tokio::test]
async fn mcp_catalog_alone_mounts_the_endpoint_at_slash_mcp() {
    let client = TestApp::new()
        .routes(routes![list_invoices])
        .mcp_catalog(McpCatalog::Progressive)
        .build();
    let list = rpc(&client, "tools/list", json!({})).await;
    assert_eq!(names(&list), ["search_tools", "call_tool", "list_invoices"]);
}

// ── Instructions ──────────────────────────────────────────────────────

#[tokio::test]
async fn instructions_are_absent_by_default() {
    let out = rpc(
        &full_app(),
        "initialize",
        json!({"protocolVersion": "2025-06-18"}),
    )
    .await;
    assert!(out["result"].get("instructions").is_none(), "{out}");
}

#[tokio::test]
async fn initialize_returns_the_instructions() {
    let client = TestApp::new()
        .routes(routes![list_invoices])
        .mcp_instructions(INSTRUCTIONS)
        .mount_mcp("/mcp")
        .build();
    let out = rpc(
        &client,
        "initialize",
        json!({"protocolVersion": "2025-06-18"}),
    )
    .await;
    assert_eq!(out["result"]["instructions"], INSTRUCTIONS);
}

#[tokio::test]
async fn discover_returns_the_instructions() {
    let client = TestApp::new()
        .routes(routes![list_invoices])
        .mcp_instructions(INSTRUCTIONS)
        .mount_mcp("/mcp")
        .build();
    let resp = modern(&client, "server/discover", json!({})).await;
    resp.assert_ok();
    assert_eq!(resp.json::<Value>()["result"]["instructions"], INSTRUCTIONS);
}

#[tokio::test]
async fn discover_has_no_instructions_by_default() {
    let resp = modern(&full_app(), "server/discover", json!({})).await;
    resp.assert_ok();
    let out = resp.json::<Value>();
    assert!(out["result"].get("instructions").is_none(), "{out}");
}

#[tokio::test]
async fn progressive_mode_appends_search_guidance() {
    let out = rpc(
        &progressive_app(),
        "initialize",
        json!({"protocolVersion": "2025-06-18"}),
    )
    .await;
    let text = out["result"]["instructions"].as_str().unwrap();
    assert!(text.starts_with(INSTRUCTIONS), "{text}");
    assert!(text.contains("search_tools"), "{text}");
    assert!(text.contains("call_tool"), "{text}");
}

#[tokio::test]
async fn progressive_mode_without_instructions_sends_the_guidance() {
    let client = TestApp::new()
        .routes(routes![list_invoices])
        .mcp_catalog(McpCatalog::Progressive)
        .mount_mcp("/mcp")
        .build();
    let resp = modern(&client, "server/discover", json!({})).await;
    let text = resp.json::<Value>()["result"]["instructions"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(text.contains("search_tools"), "{text}");
}

// ── The progressive catalog ───────────────────────────────────────────

#[tokio::test]
async fn progressive_list_holds_meta_tools_and_core_tools_only() {
    let list = rpc(&progressive_app(), "tools/list", json!({})).await;
    assert_eq!(names(&list), ["search_tools", "call_tool", "list_invoices"]);
    for name in ["search_tools", "call_tool", "list_invoices"] {
        assert_eq!(tool(&list, name)["_meta"][ALWAYS_LOAD], true, "{name}");
    }
}

#[tokio::test]
async fn progressive_list_in_the_modern_era() {
    let resp = modern(&progressive_app(), "tools/list", json!({})).await;
    resp.assert_ok();
    let list = resp.json::<Value>();
    assert_eq!(list["result"]["resultType"], "complete", "{list}");
    assert_eq!(names(&list), ["search_tools", "call_tool", "list_invoices"]);
}

#[tokio::test]
async fn meta_tools_carry_safety_annotations() {
    let list = rpc(&progressive_app(), "tools/list", json!({})).await;
    let search = &tool(&list, "search_tools")["annotations"];
    assert_eq!(search["readOnlyHint"], true, "{search}");
    assert_eq!(search["openWorldHint"], false, "{search}");
    let call = &tool(&list, "call_tool")["annotations"];
    assert_eq!(call["readOnlyHint"], false, "{call}");
    assert_eq!(call["destructiveHint"], true, "{call}");
}

#[tokio::test]
async fn search_description_outlines_the_catalog() {
    let list = rpc(&progressive_app(), "tools/list", json!({})).await;
    let text = tool(&list, "search_tools")["description"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(text.contains("5 tools"), "{text}");
    for tag in ["billing", "docs", "reports"] {
        assert!(text.contains(tag), "{tag} missing from {text}");
    }
}

#[tokio::test]
async fn search_matches_a_description_word() {
    let out = search(&progressive_app(), json!({"query": "customer"})).await;
    assert_eq!(out["result"]["isError"], false, "{out}");
    assert_eq!(found(&out), ["refund_invoice"]);
}

#[tokio::test]
async fn equal_scores_keep_the_catalog_order() {
    let out = search(&progressive_app(), json!({"query": "billing"})).await;
    assert_eq!(found(&out), ["list_invoices", "refund_invoice"]);
}

#[tokio::test]
async fn search_matches_a_query_argument() {
    let out = search(&progressive_app(), json!({"query": "term"})).await;
    assert_eq!(found(&out), ["find_articles"]);
}

#[tokio::test]
async fn search_matches_a_tag() {
    let out = search(&progressive_app(), json!({"query": "docs"})).await;
    assert_eq!(found(&out), ["find_articles"]);
}

#[tokio::test]
async fn search_matches_an_argument_name() {
    let out = search(&progressive_app(), json!({"query": "reason"})).await;
    assert_eq!(found(&out), ["refund_invoice"]);
}

#[tokio::test]
async fn search_ranks_hits_by_score() {
    let out = search(&progressive_app(), json!({"query": "invoice report"})).await;
    assert_eq!(
        found(&out),
        [
            "build_report",
            "secret_report",
            "refund_invoice",
            "list_invoices"
        ]
    );
}

#[tokio::test]
async fn search_detail_levels_shape_each_entry() {
    let client = progressive_app();
    let entry = |out: &Value| out["result"]["structuredContent"]["tools"][0].clone();

    let named = entry(&search(&client, json!({"query": "refund", "detail": "name"})).await);
    assert_eq!(named, json!({"name": "refund_invoice"}));

    let summary = entry(&search(&client, json!({"query": "refund"})).await);
    assert_eq!(summary["description"], "Refund one invoice to the customer");
    assert!(summary["annotations"].is_object(), "{summary}");
    assert!(summary.get("inputSchema").is_none(), "{summary}");

    let full = entry(&search(&client, json!({"query": "refund", "detail": "full"})).await);
    assert!(
        full["inputSchema"]["properties"]["id"].is_object(),
        "{full}"
    );
    assert!(
        full["inputSchema"]["properties"]["body"].is_object(),
        "{full}"
    );
}

#[tokio::test]
async fn empty_query_lists_every_tool_up_to_the_limit() {
    let out = search(&progressive_app(), json!({"limit": 2})).await;
    assert_eq!(out["result"]["structuredContent"]["total"], 5, "{out}");
    assert_eq!(found(&out), ["list_invoices", "refund_invoice"]);
}

#[tokio::test]
async fn offset_pages_through_the_whole_catalog() {
    let client = progressive_app();
    let mut seen = Vec::new();
    for offset in [0, 2, 4] {
        let out = search(&client, json!({"limit": 2, "offset": offset})).await;
        assert_eq!(out["result"]["structuredContent"]["total"], 5, "{out}");
        seen.extend(found(&out));
    }
    assert_eq!(
        seen,
        [
            "list_invoices",
            "refund_invoice",
            "find_articles",
            "secret_report",
            "build_report"
        ]
    );
}

#[tokio::test]
async fn search_result_text_mirrors_the_structured_content() {
    let out = search(&progressive_app(), json!({"query": "refund"})).await;
    let text = out["result"]["content"][0]["text"].as_str().unwrap();
    let parsed: Value = serde_json::from_str(text).unwrap();
    assert_eq!(parsed, out["result"]["structuredContent"]);
}

#[tokio::test]
async fn bad_search_arguments_are_invalid_params() {
    let client = progressive_app();
    for arguments in [
        json!({"query": 5}),
        json!({"detail": "everything"}),
        json!({"limit": 0}),
        json!({"limit": 51}),
        json!({"limit": "ten"}),
        json!({"query": "x".repeat(129)}),
        json!({"offset": -1}),
        json!("not an object"),
        json!(null),
    ] {
        let out = search(&client, arguments.clone()).await;
        assert_eq!(out["error"]["code"], -32602, "{arguments} gave {out}");
    }
}

// ── call_tool ─────────────────────────────────────────────────────────

#[tokio::test]
async fn call_tool_runs_a_hidden_tool() {
    let out = rpc(
        &progressive_app(),
        "tools/call",
        json!({"name": "call_tool", "arguments": {
            "name": "refund_invoice",
            "arguments": {"id": "inv-9", "body": {"reason": "duplicate"}},
        }}),
    )
    .await;
    assert_eq!(out["result"]["isError"], false, "{out}");
    let text = out["result"]["content"][0]["text"].as_str().unwrap();
    let refund: Value = serde_json::from_str(text).unwrap();
    assert_eq!(refund, json!({"id": "inv-9", "reason": "duplicate"}));
}

#[tokio::test]
async fn call_tool_forwards_the_caller_session() {
    let client = progressive_app();
    client.login_as("user-1").await;
    let out = rpc(
        &client,
        "tools/call",
        json!({"name": "call_tool", "arguments": {"name": "secret_report", "arguments": {}}}),
    )
    .await;
    assert_ne!(out["result"]["isError"], true, "{out}");
    assert!(out.to_string().contains("top-secret-value"), "{out}");
}

#[tokio::test]
async fn call_tool_keeps_the_route_guard() {
    let out = rpc(
        &progressive_app(),
        "tools/call",
        json!({"name": "call_tool", "arguments": {"name": "secret_report", "arguments": {}}}),
    )
    .await;
    assert_eq!(out["result"]["isError"], true, "{out}");
    assert!(!out.to_string().contains("top-secret-value"), "{out}");
}

#[tokio::test]
async fn call_tool_refuses_unknown_and_meta_tool_names() {
    let client = progressive_app();
    for name in ["no_such_tool", "call_tool", "search_tools"] {
        let out = rpc(
            &client,
            "tools/call",
            json!({"name": "call_tool", "arguments": {"name": name, "arguments": {}}}),
        )
        .await;
        assert_eq!(out["error"]["code"], -32602, "{name} gave {out}");
    }
}

#[tokio::test]
async fn bad_call_tool_arguments_are_invalid_params() {
    let client = progressive_app();
    for arguments in [
        json!({}),
        json!({"name": 3}),
        json!({"name": "list_invoices", "arguments": "nope"}),
        json!({"name": "list_invoices", "arguments": null}),
    ] {
        let out = rpc(
            &client,
            "tools/call",
            json!({"name": "call_tool", "arguments": arguments.clone()}),
        )
        .await;
        assert_eq!(out["error"]["code"], -32602, "{arguments} gave {out}");
    }
}

#[tokio::test]
async fn null_arguments_get_the_same_error_both_ways() {
    let client = progressive_app();
    let direct = rpc(
        &client,
        "tools/call",
        json!({"name": "list_invoices", "arguments": null}),
    )
    .await;
    let wrapped = rpc(
        &client,
        "tools/call",
        json!({"name": "call_tool", "arguments": {"name": "list_invoices", "arguments": null}}),
    )
    .await;
    assert_eq!(direct["error"]["code"], -32602, "{direct}");
    assert_eq!(wrapped["error"]["code"], -32602, "{wrapped}");
}

#[tokio::test]
async fn call_tool_streams_a_streaming_tool() {
    let client = TestApp::new()
        .routes(routes![list_invoices, live_matches])
        .mcp_catalog(McpCatalog::Progressive)
        .mount_mcp("/mcp")
        .build();
    let resp = client
        .post("/mcp")
        .header("accept", "application/json, text/event-stream")
        .json(
            &json!({"jsonrpc": "2.0", "id": 42, "method": "tools/call", "params": {
                "name": "call_tool",
                "arguments": {"name": "live_matches", "arguments": {}},
                "_meta": {"progressToken": "tok-1"},
            }}),
        )
        .send()
        .await;
    resp.assert_ok();
    assert_eq!(
        resp.header("content-type")
            .map(|c| c.starts_with("text/event-stream")),
        Some(true)
    );
    let messages: Vec<Value> = resp
        .text()
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .filter_map(|data| serde_json::from_str(data.trim()).ok())
        .collect();
    let progress: Vec<&Value> = messages
        .iter()
        .filter(|m| m["method"] == "notifications/progress")
        .collect();
    assert_eq!(progress.len(), 2, "{messages:?}");
    assert!(
        progress
            .iter()
            .all(|p| p["params"]["progressToken"] == "tok-1")
    );
    let last = messages
        .iter()
        .find(|m| m["id"] == 42)
        .expect("a final result");
    let text = last["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("match 2"), "{text}");
}

#[tokio::test]
async fn a_hidden_tool_still_answers_a_direct_call() {
    let out = rpc(
        &progressive_app(),
        "tools/call",
        json!({"name": "find_articles", "arguments": {"query": {"term": "setup"}}}),
    )
    .await;
    assert_eq!(out["result"]["isError"], false, "{out}");
    assert!(
        out["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("setup")
    );
}

#[tokio::test]
async fn call_tool_starts_a_task_in_the_modern_era() {
    let client = progressive_app();
    let caps = json!({"extensions": {TASKS: {}}});
    let resp = modern(
        &client,
        "tools/call",
        json!({
            "name": "call_tool",
            "arguments": {"name": "build_report", "arguments": {}},
            "_meta": meta(&caps),
        }),
    )
    .await;
    resp.assert_ok();
    let out = resp.json::<Value>();
    assert_eq!(out["result"]["resultType"], "task", "{out}");

    // The task runs the inner tool.
    let task_id = out["result"]["taskId"].as_str().unwrap().to_owned();
    let mut done = Value::Null;
    for _ in 0..200 {
        let resp = modern(
            &client,
            "tasks/get",
            json!({"taskId": task_id, "_meta": meta(&caps)}),
        )
        .await;
        done = resp.json::<Value>();
        if done["result"]["status"] == "completed" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(done["result"]["status"], "completed", "{done}");
    let text = done["result"]["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    let report: Value = serde_json::from_str(text).unwrap();
    assert_eq!(report, json!({"id": "report", "paid": true}));
}

#[tokio::test]
async fn call_tool_runs_a_task_tool_inline_without_the_capability() {
    let resp = modern(
        &progressive_app(),
        "tools/call",
        json!({"name": "call_tool", "arguments": {"name": "build_report", "arguments": {}}}),
    )
    .await;
    resp.assert_ok();
    let out = resp.json::<Value>();
    assert_eq!(out["result"]["resultType"], "complete", "{out}");
    assert!(
        out["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("report")
    );
}

#[tokio::test]
async fn call_tool_runs_a_plain_tool_in_the_modern_era() {
    let resp = modern(
        &progressive_app(),
        "tools/call",
        json!({"name": "call_tool", "arguments": {
            "name": "refund_invoice",
            "arguments": {"id": "inv-3", "body": {"reason": "late"}},
        }}),
    )
    .await;
    resp.assert_ok();
    let out = resp.json::<Value>();
    assert_eq!(out["result"]["resultType"], "complete", "{out}");
    assert!(
        out["result"]["_meta"]["io.modelcontextprotocol/serverInfo"].is_object(),
        "{out}"
    );
    assert_eq!(out["result"]["isError"], false, "{out}");
}

#[tokio::test]
async fn search_tools_works_in_the_modern_era() {
    let resp = modern(
        &progressive_app(),
        "tools/call",
        json!({"name": "search_tools", "arguments": {"query": "refund"}}),
    )
    .await;
    resp.assert_ok();
    let out = resp.json::<Value>();
    assert_eq!(out["result"]["resultType"], "complete", "{out}");
    assert_eq!(found(&out), ["refund_invoice"]);
}

// ── Reserved names ────────────────────────────────────────────────────

/// Capture the log events of this thread while the guard lives.
fn capture_logs() -> (LogBuffer, tracing::dispatcher::DefaultGuard) {
    let buf = LogBuffer::new(64, ParameterFilter::default());
    let subscriber = tracing_subscriber::registry().with(LogCaptureLayer::new(buf.clone()));
    let guard = tracing::dispatcher::set_default(&tracing::Dispatch::new(subscriber));
    (buf, guard)
}

#[tokio::test]
async fn a_user_tool_with_a_reserved_name_is_skipped_in_progressive_mode() {
    let (logs, guard) = capture_logs();
    let client = TestApp::new()
        .routes(routes![list_invoices, clashing_tool])
        .mcp_catalog(McpCatalog::Progressive)
        .mount_mcp("/mcp")
        .build();
    drop(guard);
    let warned = logs.snapshot(None, None).into_iter().any(|entry| {
        entry.level == "WARN"
            && entry.message.contains("reserves this name")
            && entry.fields.get("tool").and_then(|v| v.as_str()) == Some("search_tools")
    });
    assert!(warned, "{:?}", logs.snapshot(None, None));
    let list = rpc(&client, "tools/list", json!({})).await;
    assert_eq!(names(&list), ["search_tools", "call_tool", "list_invoices"]);
    let out = search(&client, json!({})).await;
    assert_eq!(out["result"]["isError"], false, "{out}");
    assert_eq!(found(&out), ["list_invoices"]);
}

#[tokio::test]
async fn a_user_tool_with_a_reserved_name_is_kept_in_full_mode() {
    let client = TestApp::new()
        .routes(routes![clashing_tool])
        .mount_mcp("/mcp")
        .build();
    let list = rpc(&client, "tools/list", json!({})).await;
    assert_eq!(names(&list), ["search_tools"]);
    let out = rpc(
        &client,
        "tools/call",
        json!({"name": "search_tools", "arguments": {}}),
    )
    .await;
    assert_eq!(
        out["result"]["content"][0]["text"], "\"user-tool\"",
        "{out}"
    );
}

#[tokio::test]
async fn an_oversized_query_is_refused() {
    let query = "a ".repeat(100_000);
    let out = search(&progressive_app(), json!({"query": query})).await;
    assert_eq!(out["error"]["code"], -32602, "{out}");
}
