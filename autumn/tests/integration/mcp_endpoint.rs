//! Integration tests for the MCP (Model Context Protocol) tool surface
//! (issue #1117): expose typed endpoints as MCP tools so agents can call
//! the real, authenticated handler pipeline.
//!
//! Covers the acceptance criteria:
//! * Per-endpoint opt-in via `#[api_doc(mcp)]`; nothing exposed implicitly.
//! * `mount_mcp("/mcp")` serves a spec-compliant Streamable-HTTP endpoint
//!   handling `initialize`, `tools/list`, and `tools/call`.
//! * Tool `name`/`description`/`inputSchema` are derived from `ApiDoc`.
//! * `tools/call` dispatches through the real handler pipeline.
//! * Bearer-token auth (`RequireApiToken`) applies to agent calls.
//! * HTTP method → MCP safety annotations.
//! * JSON-only eligibility; HTML routes auto-excluded.
//! * Whole-API hatch (`expose_all_as_mcp`) still requires opt-in for
//!   mutating verbs and honors exclusions.

#![cfg(feature = "mcp")]

use std::sync::Arc;

use autumn_web::auth::{InMemoryApiTokenStore, RequireApiToken, issue_api_token};
use autumn_web::config::AutumnConfig;
use autumn_web::openapi::OpenApiConfig;
use autumn_web::prelude::*;
use autumn_web::test::{TestApp, TestClient};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone)]
struct Todo {
    id: u32,
    title: String,
}

#[derive(Serialize, Deserialize)]
struct NewTodo {
    title: String,
}

#[get("/api/todos")]
#[api_doc(mcp, summary = "List all todos")]
async fn list_todos() -> AutumnResult<Json<Vec<Todo>>> {
    Ok(Json(vec![Todo {
        id: 1,
        title: "first".into(),
    }]))
}

#[get("/api/todos/{id}")]
#[api_doc(mcp, summary = "Fetch one todo")]
async fn get_todo(Path(id): Path<u32>) -> AutumnResult<Json<Todo>> {
    Ok(Json(Todo {
        id,
        title: format!("todo {id}"),
    }))
}

#[post("/api/todos")]
#[api_doc(mcp, summary = "Create a todo")]
async fn create_todo(Json(body): Json<NewTodo>) -> AutumnResult<Json<Todo>> {
    Ok(Json(Todo {
        id: 42,
        title: body.title,
    }))
}

#[delete("/api/todos/{id}")]
#[api_doc(mcp, summary = "Delete a todo")]
async fn delete_todo(Path(id): Path<u32>) -> AutumnResult<Json<Todo>> {
    Ok(Json(Todo {
        id,
        title: "deleted".into(),
    }))
}

// Requires the `CspNonce` extractor: it fails (500/tool error) unless the
// dispatch pipeline injected a nonce into request extensions. Used to guard the
// MCP-dispatch nonce regression — the framework's outermost `SecurityHeadersLayer`
// (which injects the nonce) is applied after the dispatch clone is taken, so the
// clone must re-attach it.
#[get("/api/nonce-tool")]
#[api_doc(mcp, summary = "Requires the CSP nonce")]
async fn nonce_tool(_nonce: autumn_web::security::CspNonce) -> AutumnResult<Json<Todo>> {
    Ok(Json(Todo {
        id: 99,
        title: "nonce-ok".into(),
    }))
}

// A user route that already owns the `/mcp` path, used to exercise the
// mount-path collision preflight.
#[get("/mcp")]
async fn mcp_named_route() -> AutumnResult<Json<Todo>> {
    Ok(Json(Todo {
        id: 0,
        title: "preexisting".into(),
    }))
}

// Opted-out JSON endpoint: eligible but explicitly excluded.
#[get("/api/secret")]
#[api_doc(mcp = false)]
async fn secret() -> AutumnResult<Json<Todo>> {
    Ok(Json(Todo {
        id: 0,
        title: "secret".into(),
    }))
}

// Not opted in at all — must never be exposed.
#[get("/api/private")]
async fn private_route() -> AutumnResult<Json<Todo>> {
    Ok(Json(Todo {
        id: 0,
        title: "private".into(),
    }))
}

// HTML route opted in: ineligible (no JSON response), auto-excluded.
#[get("/page")]
#[api_doc(mcp)]
async fn html_page() -> &'static str {
    "<h1>hi</h1>"
}

// Appends a `Set-Cookie` to every response in the pipeline; used to verify a
// single `tools/call` propagates the replayed handler's cookie updates while a
// batch does not.
async fn add_test_cookie(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let mut resp = next.run(req).await;
    resp.headers_mut().append(
        axum::http::header::SET_COOKIE,
        axum::http::HeaderValue::from_static("mcp_session=abc; Path=/"),
    );
    resp
}

// Appends a distinctive `Server-Timing` metric to every response in the
// pipeline. In production the primary `ServerTimingLayer` inside the dispatch
// clone adds the real `db;dur;desc="N queries"` metric the same way (via
// `HeaderMap::append`); the `app;dur=1.500` stand-in lets a DB-free test assert
// that a `tools/call` forwards the inner pipeline's non-`total` `Server-Timing`
// metric onto the outer `/mcp` response instead of discarding it. The
// distinctive `total;dur=424242.000` stands in for the inner-dispatch `total`
// that must be *dropped* (the outer fallback emits the real `/mcp` `total`),
// and shares one header value with the kept metric so the endpoint's
// comma-split stripping is exercised.
async fn add_test_server_timing(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let mut resp = next.run(req).await;
    resp.headers_mut().append(
        axum::http::HeaderName::from_static("server-timing"),
        axum::http::HeaderValue::from_static("app;dur=1.500, total;dur=424242.000"),
    );
    resp
}

// Test config with the `Server-Timing` header (#1348) force-enabled and CSRF
// disabled so a `tools/call` POST dispatches cleanly.
fn config_with_server_timing() -> AutumnConfig {
    let mut config = AutumnConfig {
        profile: Some("test".into()),
        ..Default::default()
    };
    config.security.csrf.enabled = false;
    config.observability.server_timing = Some(true);
    config
}

fn count_server_timing_headers(resp: &autumn_web::test::TestResponse) -> usize {
    resp.headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("server-timing"))
        .count()
}

async fn rpc(client: &TestClient, body: serde_json::Value) -> serde_json::Value {
    let resp = client.post("/mcp").json(&body).send().await;
    resp.assert_ok();
    resp.json::<serde_json::Value>()
}

// A realistic `static_gate`: a page-cache gate that redirects unauthenticated
// GET/HEAD page navigations to a login page, but leaves non-GET requests (such
// as the `/mcp` JSON-RPC POST transport) untouched. Used to prove the gate is
// excluded from MCP `tools/call` dispatch in fully-dynamic mode.
async fn redirect_unauthenticated_pages(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let is_get = matches!(
        *req.method(),
        axum::http::Method::GET | axum::http::Method::HEAD
    );
    if is_get && req.headers().get("x-authed").is_none() {
        return axum::response::Response::builder()
            .status(axum::http::StatusCode::FOUND)
            .header(axum::http::header::LOCATION, "/login")
            .body(axum::body::Body::empty())
            .unwrap();
    }
    next.run(req).await
}

#[tokio::test]
async fn initialize_returns_server_info_and_tools_capability() {
    let client = TestApp::new()
        .routes(routes![list_todos, create_todo])
        .mount_mcp("/mcp")
        .build();

    let out = rpc(
        &client,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "protocolVersion": "2025-06-18", "capabilities": {} }
        }),
    )
    .await;

    assert_eq!(out["jsonrpc"], "2.0");
    assert_eq!(out["id"], 1);
    assert!(out["result"]["capabilities"]["tools"].is_object());
    assert!(out["result"]["serverInfo"]["name"].is_string());
    assert_eq!(out["result"]["protocolVersion"], "2025-06-18");
}

#[tokio::test]
async fn tools_list_derives_from_api_doc_and_honors_opt_in() {
    let client = TestApp::new()
        .routes(routes![
            list_todos,
            get_todo,
            create_todo,
            delete_todo,
            secret,
            private_route,
            html_page
        ])
        .mount_mcp("/mcp")
        .build();

    let out = rpc(
        &client,
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
    )
    .await;

    let tools = out["result"]["tools"].as_array().expect("tools array");
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();

    // Opted-in JSON endpoints are exposed.
    assert!(names.contains(&"list_todos"));
    assert!(names.contains(&"get_todo"));
    assert!(names.contains(&"create_todo"));
    assert!(names.contains(&"delete_todo"));
    // Explicitly excluded, not opted in, and HTML routes are absent.
    assert!(!names.contains(&"secret"), "mcp = false must exclude");
    assert!(!names.contains(&"private_route"), "no opt-in => excluded");
    assert!(!names.contains(&"html_page"), "HTML route is ineligible");

    // Description + inputSchema derived from ApiDoc.
    let create = tools.iter().find(|t| t["name"] == "create_todo").unwrap();
    assert_eq!(create["description"], "Create a todo");
    assert_eq!(create["inputSchema"]["type"], "object");
    assert!(
        create["inputSchema"]["properties"]["body"].is_object(),
        "request body becomes a `body` property"
    );

    let get = tools.iter().find(|t| t["name"] == "get_todo").unwrap();
    assert!(
        get["inputSchema"]["properties"]["id"].is_object(),
        "path param becomes a property"
    );
}

#[tokio::test]
async fn safety_annotations_track_http_method() {
    let client = TestApp::new()
        .routes(routes![list_todos, create_todo, delete_todo])
        .mount_mcp("/mcp")
        .build();

    let out = rpc(
        &client,
        serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/list"}),
    )
    .await;
    let tools = out["result"]["tools"].as_array().unwrap();
    let by = |n: &str| tools.iter().find(|t| t["name"] == n).unwrap().clone();

    assert_eq!(by("list_todos")["annotations"]["readOnlyHint"], true);
    assert_eq!(by("create_todo")["annotations"]["readOnlyHint"], false);
    assert_eq!(by("delete_todo")["annotations"]["readOnlyHint"], false);
    assert_eq!(by("delete_todo")["annotations"]["destructiveHint"], true);
}

#[tokio::test]
async fn tools_call_dispatches_read_tool_through_real_pipeline() {
    let client = TestApp::new()
        .routes(routes![get_todo])
        .mount_mcp("/mcp")
        .build();

    let out = rpc(
        &client,
        serde_json::json!({
            "jsonrpc":"2.0","id":4,"method":"tools/call",
            "params": {"name":"get_todo","arguments":{"id":"7"}}
        }),
    )
    .await;

    assert_ne!(out["result"]["isError"], true);
    let text = out["result"]["content"][0]["text"].as_str().unwrap();
    let payload: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(payload["id"], 7);
    assert_eq!(payload["title"], "todo 7");
}

// Declares an empty-body 204 contract but actually returns a 200 with an
// HTML body — the mislabeled-status case the dispatch-side enforcement
// exists for.
#[get("/api/cache/purge")]
#[api_doc(mcp, status = 204, summary = "Purge the cache")]
async fn purge_cache() -> &'static str {
    "<html>internal dashboard</html>"
}

#[tokio::test]
async fn empty_body_contract_tool_never_leaks_a_body() {
    // A schema-less route is only MCP-eligible when it declares an empty-body
    // status (204/205), and the advertised result of such a tool is empty
    // text. Dispatch enforces the contract rather than trusting the declared
    // status, so a mislabeled HTML route cannot leak its body to the agent.
    let client = TestApp::new()
        .routes(routes![purge_cache])
        .mount_mcp("/mcp")
        .build();

    let out = rpc(
        &client,
        serde_json::json!({
            "jsonrpc":"2.0","id":11,"method":"tools/call",
            "params": {"name":"purge_cache","arguments":{}}
        }),
    )
    .await;

    assert_ne!(out["result"]["isError"], true);
    assert_eq!(
        out["result"]["content"][0]["text"], "",
        "empty-body-contract tool must return empty text, not the handler body"
    );
}

#[tokio::test]
async fn static_gate_is_excluded_from_mcp_dispatch_in_dynamic_mode() {
    // A fully-dynamic app (no SSG `dist`) with a redirect-style `static_gate`
    // plus MCP. The gate must guard ordinary page navigation but must NOT turn a
    // `tools/call` into a redirect: the gate is applied after the MCP dispatch
    // clone is taken, so dispatch replays the handler without it — matching the
    // SSG/ISG path and the documented "not applied to MCP dispatch" contract.
    let client = TestApp::new()
        .routes(routes![get_todo])
        .static_gate(axum::middleware::from_fn(redirect_unauthenticated_pages))
        .mount_mcp("/mcp")
        .build();

    // Direct, unauthenticated GET to the gated route: the gate redirects it.
    let direct = client.get("/api/todos/7").send().await;
    direct.assert_status(302);

    // The same handler invoked (unauthenticated) via MCP `tools/call` dispatches
    // through the real pipeline WITHOUT the gate, returning the tool result
    // rather than a 302-turned-tool-error.
    let out = rpc(
        &client,
        serde_json::json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params": {"name":"get_todo","arguments":{"id":"7"}}
        }),
    )
    .await;

    assert_ne!(out["result"]["isError"], true);
    let text = out["result"]["content"][0]["text"].as_str().unwrap();
    let payload: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(payload["id"], 7);
    assert_eq!(payload["title"], "todo 7");
}

#[tokio::test]
async fn tools_call_dispatch_injects_csp_nonce_when_enabled() {
    // Regression (round-four): the framework's outermost `SecurityHeadersLayer`
    // — which injects `CspNonce` into request extensions — is applied AFTER the
    // MCP dispatch clone is taken (so the page gate stays out of dispatch). The
    // clone re-attaches that layer, so an MCP-exposed handler using the `CspNonce`
    // extractor dispatches successfully instead of failing with a 500/tool error
    // when `security.headers.csp_nonce` is enabled.
    let mut config = AutumnConfig::default();
    config.security.headers.csp_nonce.enabled = true;
    let client = TestApp::new()
        .routes(routes![nonce_tool])
        .config(config)
        .mount_mcp("/mcp")
        .build();

    let out = rpc(
        &client,
        serde_json::json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params": {"name":"nonce_tool","arguments":{}}
        }),
    )
    .await;

    assert_ne!(
        out["result"]["isError"], true,
        "tools/call dispatch must inject CspNonce: {out}"
    );
}

#[tokio::test]
async fn tools_call_dispatches_write_tool_with_body() {
    let client = TestApp::new()
        .routes(routes![create_todo])
        .mount_mcp("/mcp")
        .build();

    let out = rpc(
        &client,
        serde_json::json!({
            "jsonrpc":"2.0","id":5,"method":"tools/call",
            "params": {"name":"create_todo","arguments":{"body":{"title":"new one"}}}
        }),
    )
    .await;

    assert_ne!(out["result"]["isError"], true);
    let text = out["result"]["content"][0]["text"].as_str().unwrap();
    let payload: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(payload["id"], 42);
    assert_eq!(payload["title"], "new one");
}

#[tokio::test]
async fn single_tools_call_propagates_set_cookie() {
    // A session-renewal / CSRF-refresh handler sets Set-Cookie; a single
    // tools/call must replay it onto the outer HTTP response so cookie-based
    // MCP flows behave like the equivalent direct call.
    let client = TestApp::new()
        .routes(routes![get_todo])
        .layer(axum::middleware::from_fn(add_test_cookie))
        .mount_mcp("/mcp")
        .build();

    let resp = client
        .post("/mcp")
        .json(&serde_json::json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params": {"name":"get_todo","arguments":{"id":"7"}}
        }))
        .send()
        .await;
    resp.assert_ok();
    assert_eq!(resp.header("set-cookie"), Some("mcp_session=abc; Path=/"));
}

#[tokio::test]
async fn single_tools_call_forwards_server_timing() {
    // #1348: a `tools/call` dispatches through a clone of the app router that
    // carries the primary `ServerTimingLayer`. That inner layer builds the full
    // metric set (in production, including `db;dur;desc="N queries"`) on the
    // dispatched response — but the JSON-RPC envelope is rebuilt, so the inner
    // header would be discarded. The endpoint forwards the inner *non-`total`*
    // metrics while dropping the inner `total`, so the outer fallback emits the
    // real `/mcp` `total` (which includes the endpoint's body buffering) rather
    // than exposing the inner-dispatch `total` that under-reports latency.
    let client = TestApp::new()
        .config(config_with_server_timing())
        .routes(routes![get_todo])
        .layer(axum::middleware::from_fn(add_test_server_timing))
        .mount_mcp("/mcp")
        .build();

    let resp = client
        .post("/mcp")
        .json(&serde_json::json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params": {"name":"get_todo","arguments":{"id":"7"}}
        }))
        .send()
        .await;
    resp.assert_ok();

    let joined = resp
        .headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("server-timing"))
        .map(|(_, v)| v.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    // The distinctive inner non-`total` metric (stand-in for the real `db;dur`)
    // must reach the outer response rather than being dropped with the envelope.
    assert!(
        joined.contains("app;dur=1.500"),
        "inner pipeline non-`total` `Server-Timing` metric must be forwarded onto `/mcp`: {joined:?}"
    );
    // The inner-dispatch `total` must be dropped, NOT forwarded: the distinctive
    // inner value never appears on `/mcp`.
    assert!(
        !joined.contains("424242"),
        "inner-dispatch `total` must be dropped, not forwarded onto `/mcp`: {joined:?}"
    );
    // Exactly one `total` remains — the outer fallback's real `/mcp` total. No
    // duplicate `total` from forwarding the inner header alongside the fallback.
    assert_eq!(
        joined.matches("total;dur=").count(),
        1,
        "exactly one `total` (the outer fallback's) expected on `/mcp`: {joined:?}"
    );
}

#[tokio::test]
async fn tools_call_server_timing_no_duplicate_total() {
    // Without any inner non-`total` metric to forward (no DB queries, no handler
    // metric), the endpoint forwards nothing and the outer fallback emits the
    // real `/mcp` `total`, so the response carries a single `Server-Timing` line
    // with exactly one `total`.
    let client = TestApp::new()
        .config(config_with_server_timing())
        .routes(routes![get_todo])
        .mount_mcp("/mcp")
        .build();

    let resp = client
        .post("/mcp")
        .json(&serde_json::json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params": {"name":"get_todo","arguments":{"id":"7"}}
        }))
        .send()
        .await;
    resp.assert_ok();

    assert_eq!(
        count_server_timing_headers(&resp),
        1,
        "expected exactly one Server-Timing header line on `/mcp`: {:?}",
        resp.headers
    );
    let header = resp
        .header("server-timing")
        .expect("outer fallback Server-Timing should be present");
    assert_eq!(
        header.matches("total;dur=").count(),
        1,
        "exactly one `total` metric expected: {header:?}"
    );
}

#[tokio::test]
async fn batch_containing_tools_call_is_rejected() {
    // A batch carrying a `tools/call` is refused outright (-32600). Batching
    // would let one envelope amplify memory (each call buffers up to the 10 MiB
    // cap, all retained until the batch serializes) and rate-limit budget (the
    // envelope is counted once, so each replay would skip the per-route
    // limiter). The newest protocol revision dropped JSON-RPC batching, so no
    // conformant client batches calls; keeping `tools/call` single-message means
    // the per-call limiter and the response cap both still apply.
    let client = TestApp::new()
        .routes(routes![get_todo])
        .layer(axum::middleware::from_fn(add_test_cookie))
        .mount_mcp("/mcp")
        .build();

    let resp = client
        .post("/mcp")
        .json(&serde_json::json!([{
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params": {"name":"get_todo","arguments":{"id":"7"}}
        }]))
        .send()
        .await;
    resp.assert_ok();
    let out = resp.json::<serde_json::Value>();
    assert_eq!(out["error"]["code"], -32600);
    // The call never dispatched, so the pipeline's `Set-Cookie` never ran.
    assert_eq!(resp.header("set-cookie"), None);
}

#[tokio::test]
async fn batch_of_metadata_methods_is_still_supported() {
    // Only `tools/call` batches are refused; harmless metadata methods that
    // older protocol revisions may batch (initialize/tools/list/ping) still
    // return one correlated response per entry.
    let client = TestApp::new()
        .routes(routes![list_todos])
        .mount_mcp("/mcp")
        .build();

    let out = rpc(
        &client,
        serde_json::json!([
            {"jsonrpc":"2.0","id":1,"method":"tools/list"},
            {"jsonrpc":"2.0","id":2,"method":"ping"}
        ]),
    )
    .await;
    let arr = out
        .as_array()
        .expect("a batch returns an array of responses");
    assert_eq!(arr.len(), 2);
    assert!(
        arr.iter()
            .any(|r| r["id"] == 1 && r["result"]["tools"].is_array())
    );
    assert!(arr.iter().any(|r| r["id"] == 2 && r["result"].is_object()));
}

#[tokio::test]
async fn unknown_tool_returns_error_result() {
    let client = TestApp::new()
        .routes(routes![get_todo])
        .mount_mcp("/mcp")
        .build();

    let out = rpc(
        &client,
        serde_json::json!({
            "jsonrpc":"2.0","id":6,"method":"tools/call",
            "params": {"name":"nope","arguments":{}}
        }),
    )
    .await;

    // Either a JSON-RPC error or an isError tool result is acceptable.
    assert!(out.get("error").is_some() || out["result"]["isError"] == true);
}

// ── Bearer-token auth reuse + real-pipeline enforcement ───────────

#[get("/todos")]
#[api_doc(mcp, summary = "List protected todos")]
async fn secure_list() -> AutumnResult<Json<Vec<Todo>>> {
    Ok(Json(vec![Todo {
        id: 99,
        title: "secret-but-authorized".into(),
    }]))
}

#[tokio::test]
async fn tools_call_enforces_bearer_token_via_real_pipeline() {
    let store = Arc::new(InMemoryApiTokenStore::default());
    let token = issue_api_token(store.as_ref(), "agent:bot").await.unwrap();

    // A scoped group carries the real `RequireApiToken` layer *and* keeps
    // the route in the registry, so MCP derives the tool from its ApiDoc.
    let client = TestApp::new()
        .scoped(
            "/secure",
            RequireApiToken::new(store.clone()),
            routes![secure_list],
        )
        .mount_mcp("/mcp")
        .build();

    // Without a token → the real RequireApiToken layer rejects (isError).
    let denied = rpc(
        &client,
        serde_json::json!({
            "jsonrpc":"2.0","id":7,"method":"tools/call",
            "params": {"name":"secure_list","arguments":{}}
        }),
    )
    .await;
    assert_eq!(denied["result"]["isError"], true, "no token must be denied");

    // With a valid bearer token forwarded by the agent → authorized.
    let resp = client
        .post("/mcp")
        .header("authorization", &format!("Bearer {token}"))
        .json(&serde_json::json!({
            "jsonrpc":"2.0","id":8,"method":"tools/call",
            "params": {"name":"secure_list","arguments":{}}
        }))
        .send()
        .await;
    resp.assert_ok();
    let out = resp.json::<serde_json::Value>();
    assert_ne!(out["result"]["isError"], true, "valid token must succeed");
    let text = out["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("secret-but-authorized"));
}

// ── Whole-API hatch ───────────────────────────────────────────────

#[tokio::test]
async fn expose_all_includes_reads_but_requires_opt_in_for_writes() {
    let client = TestApp::new()
        .routes(routes![
            list_todos,
            get_todo,
            create_todo,
            secret,
            private_route
        ])
        .expose_all_as_mcp()
        .mount_mcp("/mcp")
        .build();

    let out = rpc(
        &client,
        serde_json::json!({"jsonrpc":"2.0","id":9,"method":"tools/list"}),
    )
    .await;
    let tools = out["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();

    // GET endpoints auto-included under the hatch...
    assert!(
        names.contains(&"private_route"),
        "GET auto-included by hatch"
    );
    assert!(names.contains(&"list_todos"));
    assert!(names.contains(&"get_todo"));
    // create_todo carries explicit opt-in so it is still allowed.
    assert!(names.contains(&"create_todo"));
    // Explicit exclusion still wins, even under the hatch.
    assert!(
        !names.contains(&"secret"),
        "mcp = false honored under hatch"
    );
}

#[post("/api/bulk")]
async fn bulk_write(Json(_body): Json<NewTodo>) -> AutumnResult<Json<Todo>> {
    Ok(Json(Todo {
        id: 7,
        title: "bulk".into(),
    }))
}

#[tokio::test]
async fn expose_all_excludes_unopted_mutating_verbs() {
    let client = TestApp::new()
        .routes(routes![list_todos, bulk_write])
        .expose_all_as_mcp()
        .mount_mcp("/mcp")
        .build();

    let out = rpc(
        &client,
        serde_json::json!({"jsonrpc":"2.0","id":10,"method":"tools/list"}),
    )
    .await;
    let tools = out["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();

    assert!(names.contains(&"list_todos"));
    assert!(
        !names.contains(&"bulk_write"),
        "mutating verb without opt-in is excluded even under the hatch"
    );
}

// ── JSON-RPC robustness + path validation ─────────────────────────

#[tokio::test]
async fn empty_batch_returns_invalid_request() {
    let client = TestApp::new()
        .routes(routes![list_todos])
        .mount_mcp("/mcp")
        .build();

    // An empty JSON-RPC batch is itself an Invalid Request (-32600).
    let out = rpc(&client, serde_json::json!([])).await;
    assert_eq!(out["error"]["code"], -32600);
}

#[tokio::test]
async fn unsupported_protocol_version_header_is_rejected() {
    // MCP Streamable-HTTP requires a 400 for an unsupported `MCP-Protocol-Version`
    // on a non-initialize request, so an incompatible client can't run tools
    // under semantics the server never negotiated.
    let client = TestApp::new()
        .routes(routes![get_todo])
        .mount_mcp("/mcp")
        .build();

    let resp = client
        .post("/mcp")
        .header("mcp-protocol-version", "2025-11-25")
        .json(&serde_json::json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params": {"name":"get_todo","arguments":{"id":"7"}}
        }))
        .send()
        .await;
    resp.assert_status(400);
}

#[tokio::test]
async fn initialize_is_exempt_from_protocol_version_header_check() {
    // The handshake negotiates the version in-body; pre-validating its header
    // would make negotiating a newer client down to a supported version
    // impossible, so an unsupported header on `initialize` must still proceed.
    let client = TestApp::new()
        .routes(routes![list_todos])
        .mount_mcp("/mcp")
        .build();

    let resp = client
        .post("/mcp")
        .header("mcp-protocol-version", "2025-11-25")
        .json(&serde_json::json!({
            "jsonrpc":"2.0","id":1,"method":"initialize",
            "params": {"protocolVersion":"2025-11-25","capabilities":{}}
        }))
        .send()
        .await;
    resp.assert_ok();
    let out = resp.json::<serde_json::Value>();
    // Negotiated down to the server's newest supported version.
    assert_eq!(out["result"]["protocolVersion"], "2025-06-18");
}

#[tokio::test]
async fn supported_protocol_version_header_is_accepted() {
    // A negotiated, supported version header passes; a missing header is also
    // allowed (the spec's pre-header default is supported).
    let client = TestApp::new()
        .routes(routes![get_todo])
        .mount_mcp("/mcp")
        .build();

    let resp = client
        .post("/mcp")
        .header("mcp-protocol-version", "2025-06-18")
        .json(&serde_json::json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params": {"name":"get_todo","arguments":{"id":"7"}}
        }))
        .send()
        .await;
    resp.assert_ok();
    let out = resp.json::<serde_json::Value>();
    assert_ne!(out["result"]["isError"], true);
}

#[tokio::test]
async fn malformed_request_returns_invalid_request() {
    let client = TestApp::new()
        .routes(routes![list_todos])
        .mount_mcp("/mcp")
        .build();

    // A bare scalar is not a valid JSON-RPC message.
    let out = rpc(&client, serde_json::json!(5)).await;
    assert_eq!(out["error"]["code"], -32600);
}

#[tokio::test]
#[should_panic(expected = "InvalidMcpPath")]
async fn mount_path_without_leading_slash_is_rejected() {
    // axum would otherwise panic at route time; we surface it as a
    // recoverable RouterBuildError instead.
    let _ = TestApp::new()
        .routes(routes![list_todos])
        .mount_mcp("mcp")
        .build();
}

#[tokio::test]
#[should_panic(expected = "McpPathCollision")]
async fn mount_path_colliding_with_existing_route_is_rejected() {
    // An app route already owns `/mcp`; mounting the MCP endpoint there would
    // panic at merge time, so we surface a recoverable RouterBuildError.
    let _ = TestApp::new()
        .routes(routes![mcp_named_route])
        .mount_mcp("/mcp")
        .build();
}

#[tokio::test]
#[should_panic(expected = "McpPathCollision")]
async fn mount_path_colliding_with_framework_route_is_rejected() {
    // Mounting on a framework-owned GET path (the health probe) must also be
    // caught by the collision preflight, not just user routes.
    let _ = TestApp::new()
        .routes(routes![list_todos])
        .mount_mcp("/health")
        .build();
}

#[tokio::test]
#[should_panic(expected = "McpPathCollision")]
async fn mount_path_colliding_with_openapi_path_is_rejected() {
    // The OpenAPI JSON endpoint merges as a GET before the MCP router, so a
    // mount path that collides with it must be rejected up front.
    let _ = TestApp::new()
        .routes(routes![list_todos])
        .openapi(OpenApiConfig::new("Demo", "1.0.0").openapi_json_path("/mcp"))
        .mount_mcp("/mcp")
        .build();
}

#[tokio::test]
#[should_panic(expected = "McpPathCollision")]
async fn mount_path_under_nested_router_is_rejected() {
    // A raw router nested at `/api` owns every path under it and is mounted
    // before the MCP router, so mounting at `/api/mcp` would be shadowed by
    // (or panic against) the nest. The preflight must catch it like the
    // OpenAPI nest-collision check does.
    let nested = axum::Router::new().route("/thing", axum::routing::get(|| async { "ok" }));
    let _ = TestApp::new()
        .routes(routes![list_todos])
        .nest("/api", nested)
        .mount_mcp("/api/mcp")
        .build();
}

#[tokio::test]
#[should_panic(expected = "McpPathCollision")]
async fn mount_path_under_static_prefix_is_rejected() {
    // The framework unconditionally nests the static-file service at `/static`
    // before the MCP router merges, so mounting there would be shadowed by (or
    // panic against) that service. The preflight must reserve `/static`.
    let _ = TestApp::new()
        .routes(routes![list_todos])
        .mount_mcp("/static/mcp")
        .build();
}

#[tokio::test]
#[should_panic(expected = "McpPathCollision")]
async fn mount_path_colliding_with_dev_inspector_is_rejected() {
    // Under the dev profile, the request inspector merges a GET at
    // `dev.inspector_path` before the MCP router, so mounting there would panic
    // at merge time. The preflight must reserve the active inspector path.
    let config = AutumnConfig {
        profile: Some("dev".to_owned()),
        ..AutumnConfig::default()
    };
    let inspector_path = config.dev.inspector_path.clone();
    let _ = TestApp::new()
        .routes(routes![list_todos])
        .config(config)
        .mount_mcp(inspector_path)
        .build();
}

#[tokio::test]
async fn secure_mcp_rejection_carries_cors_headers() {
    // An allowlisted browser client's preflight succeeds, but an unauthenticated
    // POST is rejected by `secure_mcp` before `serve_mcp` runs. The rejection
    // must still carry `Access-Control-Allow-Origin`, or the browser masks the
    // 401 as an opaque CORS failure instead of surfacing the real status.
    let store = Arc::new(InMemoryApiTokenStore::default());
    let mut config = AutumnConfig::default();
    config.cors.allowed_origins = vec!["https://app.example".to_owned()];
    let client = TestApp::new()
        .routes(routes![list_todos])
        .config(config)
        .mount_mcp("/mcp")
        .secure_mcp(RequireApiToken::new(store.clone()))
        .build();

    let resp = client
        .post("/mcp")
        .header("origin", "https://app.example")
        .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}))
        .send()
        .await;
    resp.assert_status(401);
    assert_eq!(
        resp.header("access-control-allow-origin"),
        Some("https://app.example")
    );
}

#[tokio::test]
async fn secure_mcp_rejections_are_rate_limited() {
    // Credential-guessing against secure_mcp must be throttled: auth rejections
    // never reach the dispatch clone's limiter, so the /mcp envelope is itself
    // rate-limited. The 429 must also carry CORS (outermost grant).
    let store = Arc::new(InMemoryApiTokenStore::default());
    let mut config = AutumnConfig::default();
    config.cors.allowed_origins = vec!["https://app.example".to_owned()];
    config.security.rate_limit.enabled = true;
    config.security.rate_limit.burst = 1;
    config.security.rate_limit.requests_per_second = 0.1;
    config.security.rate_limit.trust_forwarded_headers = true;
    let client = TestApp::new()
        .routes(routes![list_todos])
        .config(config)
        .mount_mcp("/mcp")
        .secure_mcp(RequireApiToken::new(store.clone()))
        .build();

    // First unauthenticated POST: within the burst, so auth rejects it (401).
    let first = client
        .post("/mcp")
        .header("origin", "https://app.example")
        .header("x-forwarded-for", "203.0.113.9")
        .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}))
        .send()
        .await;
    first.assert_status(401);

    // Second from the same client: the envelope limiter denies it before auth
    // even runs (429), and the rejection still carries the CORS grant.
    let second = client
        .post("/mcp")
        .header("origin", "https://app.example")
        .header("x-forwarded-for", "203.0.113.9")
        .json(&serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}))
        .send()
        .await;
    second.assert_status(429);
    assert_eq!(
        second.header("access-control-allow-origin"),
        Some("https://app.example")
    );
}

#[tokio::test]
async fn tools_call_accepts_body_above_axum_default_limit() {
    // The MCP route is merged after the upload-limit middleware, so without an
    // explicit limit axum's built-in 2 MiB cap would reject a `tools/call`
    // envelope that the app's configured 32 MiB JSON limit would accept. A ~3
    // MiB payload (above 2 MiB, below the default) must dispatch successfully.
    let client = TestApp::new()
        .routes(routes![create_todo])
        .mount_mcp("/mcp")
        .build();

    let big_title = "x".repeat(3 * 1024 * 1024);
    let resp = client
        .post("/mcp")
        .json(&serde_json::json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params": {"name":"create_todo","arguments":{"body":{"title": big_title}}}
        }))
        .send()
        .await;
    resp.assert_ok();
    let out = resp.json::<serde_json::Value>();
    assert_ne!(out["result"]["isError"], true);
}

#[tokio::test]
#[should_panic(expected = "InvalidMcpPath")]
async fn dynamic_mount_path_is_rejected() {
    // A capture/catch-all mount path would shadow a whole path class; only a
    // single static endpoint is allowed.
    let _ = TestApp::new()
        .routes(routes![list_todos])
        .mount_mcp("/{tenant}/mcp")
        .build();
}

#[tokio::test]
#[should_panic(expected = "InvalidMcpPath")]
async fn colon_capture_mount_path_is_rejected() {
    // A v0.7-style `:capture` segment makes axum 0.8's `Router::route` panic
    // during assembly; surface the recoverable error instead.
    let _ = TestApp::new()
        .routes(routes![list_todos])
        .mount_mcp("/api/:mcp")
        .build();
}

#[tokio::test]
async fn tools_call_requires_body_for_write_tools() {
    let client = TestApp::new()
        .routes(routes![create_todo])
        .mount_mcp("/mcp")
        .build();

    // `body` is advertised as required; omitting it is an invalid-params error
    // rather than a silently-dispatched empty body.
    let out = rpc(
        &client,
        serde_json::json!({
            "jsonrpc":"2.0","id":11,"method":"tools/call",
            "params": {"name":"create_todo","arguments":{}}
        }),
    )
    .await;
    assert_eq!(out["error"]["code"], -32602);
}

#[tokio::test]
async fn input_schema_reuses_registered_openapi_component() {
    // A schema registered on the app's OpenApiConfig must flow into the tool's
    // inputSchema, identical to the served OpenAPI doc — not a placeholder.
    let client = TestApp::new()
        .routes(routes![create_todo])
        .openapi(OpenApiConfig::new("Demo", "1.0.0").register_schema(
            "NewTodo",
            serde_json::json!({
                "type": "object",
                "properties": { "title": { "type": "string" } },
                "required": ["title"],
            }),
        ))
        .mount_mcp("/mcp")
        .build();

    let out = rpc(
        &client,
        serde_json::json!({"jsonrpc":"2.0","id":12,"method":"tools/list"}),
    )
    .await;
    let tools = out["result"]["tools"].as_array().unwrap();
    let create = tools.iter().find(|t| t["name"] == "create_todo").unwrap();
    // The registered component (with its `title` property) is inlined into
    // `$defs`, rather than the `{type:object,title:NewTodo}` placeholder.
    assert_eq!(
        create["inputSchema"]["$defs"]["NewTodo"]["properties"]["title"]["type"],
        "string"
    );
}

#[tokio::test]
async fn non_object_arguments_are_rejected() {
    let client = TestApp::new()
        .routes(routes![get_todo])
        .mount_mcp("/mcp")
        .build();

    let out = rpc(
        &client,
        serde_json::json!({
            "jsonrpc":"2.0","id":13,"method":"tools/call",
            "params": {"name":"get_todo","arguments":"not-an-object"}
        }),
    )
    .await;
    assert_eq!(out["error"]["code"], -32602);
}

#[tokio::test]
async fn missing_jsonrpc_version_is_invalid_request() {
    let client = TestApp::new()
        .routes(routes![list_todos])
        .mount_mcp("/mcp")
        .build();

    // No `jsonrpc` member → not a valid JSON-RPC 2.0 request.
    let out = rpc(
        &client,
        serde_json::json!({ "id": 1, "method": "tools/list" }),
    )
    .await;
    assert_eq!(out["error"]["code"], -32600);
}

#[tokio::test]
async fn malformed_batch_item_returns_error_not_silent_accept() {
    let client = TestApp::new()
        .routes(routes![list_todos])
        .mount_mcp("/mcp")
        .build();

    // `[5]` is not a valid notification; it must produce an error response.
    let out = rpc(&client, serde_json::json!([5])).await;
    assert_eq!(out[0]["error"]["code"], -32600);
}

#[tokio::test]
async fn disallowed_origin_is_forbidden() {
    let client = TestApp::new()
        .routes(routes![list_todos])
        .mount_mcp("/mcp")
        .build();

    // Test config has no CORS allowed_origins, so any browser Origin is rejected
    // (DNS-rebinding protection). Non-browser agents (no Origin) are unaffected.
    let resp = client
        .post("/mcp")
        .header("origin", "https://evil.example")
        .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}))
        .send()
        .await;
    resp.assert_status(403);
}

#[tokio::test]
async fn allowlisted_origin_gets_cors_grant_on_response() {
    // The endpoint sits outside the global CORS layer, so the actual JSON-RPC
    // response (not just the OPTIONS preflight) must carry
    // `Access-Control-Allow-Origin` or a browser will block reading it.
    let mut config = AutumnConfig::default();
    config.cors.allowed_origins = vec!["https://app.example".to_owned()];
    let client = TestApp::new()
        .routes(routes![list_todos])
        .config(config)
        .mount_mcp("/mcp")
        .build();

    let resp = client
        .post("/mcp")
        .header("origin", "https://app.example")
        .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}))
        .send()
        .await;
    resp.assert_ok();
    assert_eq!(
        resp.header("access-control-allow-origin"),
        Some("https://app.example")
    );
}

#[tokio::test]
async fn proxy_resolved_same_origin_is_allowed() {
    // Behind a TLS-terminating proxy that rewrites `Host` to an internal
    // authority and supplies the public origin via `X-Forwarded-*`, a
    // same-origin browser MCP client must not be 403'd. The MCP route is merged
    // after the centralized proxy layer, so the endpoint applies its own
    // `TrustedProxiesLayer` to resolve the outer request's host/scheme.
    let mut config = AutumnConfig::default();
    config.security.trusted_proxies.trust_forwarded_headers = true;
    config.security.trusted_hosts.hosts = vec!["app.example".to_owned()];
    // CORS allowlist stays empty: this must pass via the same-origin shortcut,
    // not a cross-origin grant.
    let client = TestApp::new()
        .routes(routes![list_todos])
        .config(config)
        .mount_mcp("/mcp")
        .build();

    let resp = client
        .post("/mcp")
        .header("host", "internal.svc.cluster.local")
        .header("x-forwarded-host", "app.example")
        .header("x-forwarded-proto", "https")
        .header("origin", "https://app.example")
        .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}))
        .send()
        .await;
    // Resolved as same-origin → allowed (not the 403 a raw-Host fallback gives).
    resp.assert_ok();
}

#[tokio::test]
async fn untrusted_host_is_rejected_even_without_origin() {
    // Parity with normal routes' `TrustedHostService`: a request whose Host
    // isn't trusted is refused (400) even when it carries no `Origin` (so the
    // DNS-rebinding Origin check is skipped). Without this gate a no-`Origin`
    // agent could call `initialize`/`tools/list` with an arbitrary Host and
    // enumerate the tool catalog.
    let mut config = AutumnConfig::default();
    config.security.trusted_hosts.hosts = vec!["app.example".to_owned()];
    let client = TestApp::new()
        .routes(routes![list_todos])
        .config(config)
        .mount_mcp("/mcp")
        .build();

    let resp = client
        .post("/mcp")
        .header("host", "evil.example")
        .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}))
        .send()
        .await;
    resp.assert_status(400);
}

#[tokio::test]
async fn http2_authority_without_host_header_is_honored() {
    // An HTTP/2 client carries the target host in the request URI `:authority`
    // and may omit the `Host` header. The endpoint must resolve the host from
    // the URI authority — exactly as `TrustedHostService` does for direct
    // routes — instead of treating it as a missing host and 400'ing a trusted
    // authority. (`resolve_client_host` only consults the `Host` header, so
    // `ResolvedClientIdentity.host` is `None` here and the authority fallback is
    // what permits the request.)
    let mut config = AutumnConfig::default();
    config.security.trusted_hosts.hosts = vec!["app.example".to_owned()];
    let client = TestApp::new()
        .routes(routes![list_todos])
        .config(config)
        .mount_mcp("/mcp")
        .build();

    // An absolute-form request target populates `uri().authority()`; no `Host`
    // header is sent, mirroring an HTTP/2 `:authority`-only request.
    let resp = client
        .post("http://app.example/mcp")
        .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}))
        .send()
        .await;
    resp.assert_ok();
}

#[tokio::test]
async fn http2_untrusted_authority_without_host_header_is_rejected() {
    // The flip side: an untrusted `:authority` (no `Host` header) is still
    // rejected, so the authority fallback can't be used to dodge the gate.
    let mut config = AutumnConfig::default();
    config.security.trusted_hosts.hosts = vec!["app.example".to_owned()];
    let client = TestApp::new()
        .routes(routes![list_todos])
        .config(config)
        .mount_mcp("/mcp")
        .build();

    let resp = client
        .post("http://evil.example/mcp")
        .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}))
        .send()
        .await;
    resp.assert_status(400);
}

#[tokio::test]
async fn structured_path_argument_is_rejected() {
    // Path params are advertised as `{"type":"string"}`; a `null`/object/array
    // must return `-32602` rather than replaying a literal `null`/JSON-text
    // path segment against a real resource.
    let client = TestApp::new()
        .routes(routes![get_todo])
        .mount_mcp("/mcp")
        .build();

    let out = rpc(
        &client,
        serde_json::json!({
            "jsonrpc":"2.0","id":9,"method":"tools/call",
            "params": {"name":"get_todo","arguments":{"id":{"nested":"object"}}}
        }),
    )
    .await;

    assert_eq!(out["error"]["code"], -32602);
}

#[tokio::test]
async fn secure_mcp_still_answers_unauthenticated_cors_preflight() {
    // A browser sends the CORS preflight unauthenticated; gating OPTIONS behind
    // `secure_mcp` would 401 it and the real POST would never fire. The
    // preflight route must stay outside the auth layer.
    let store = Arc::new(InMemoryApiTokenStore::default());
    let mut config = AutumnConfig::default();
    config.cors.allowed_origins = vec!["https://app.example".to_owned()];
    let client = TestApp::new()
        .routes(routes![list_todos])
        .config(config)
        .mount_mcp("/mcp")
        .secure_mcp(RequireApiToken::new(store.clone()))
        .build();

    let resp = client
        .options("/mcp")
        .header("origin", "https://app.example")
        .header("access-control-request-method", "POST")
        .send()
        .await;
    // Not 401: the preflight is answered and grants the allowlisted origin.
    resp.assert_status(204);
    assert_eq!(
        resp.header("access-control-allow-origin"),
        Some("https://app.example")
    );

    // The JSON-RPC surface is still gated.
    client
        .post("/mcp")
        .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}))
        .send()
        .await
        .assert_status(401);
}

#[tokio::test]
async fn secure_mcp_gates_the_whole_endpoint() {
    let store = Arc::new(InMemoryApiTokenStore::default());
    let token = issue_api_token(store.as_ref(), "agent:bot").await.unwrap();

    let client = TestApp::new()
        .routes(routes![list_todos])
        .mount_mcp("/mcp")
        .secure_mcp(RequireApiToken::new(store.clone()))
        .build();

    // Even the catalog requires the token now.
    client
        .post("/mcp")
        .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}))
        .send()
        .await
        .assert_status(401);

    let ok = client
        .post("/mcp")
        .header("authorization", &format!("Bearer {token}"))
        .json(&serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}))
        .send()
        .await;
    ok.assert_ok();
    let out = ok.json::<serde_json::Value>();
    assert!(out["result"]["tools"].is_array());
}
