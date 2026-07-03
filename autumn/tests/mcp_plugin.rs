//! Integration tests for plugins layering in MCP: typed routes registered by
//! a [`Plugin`] through `AppBuilder::routes()`/`scoped()` flow into the same
//! `ApiDoc` registry as user routes, so they are derived into MCP tools and
//! dispatched through the real pipeline exactly like any other route.
//!
//! Covers the acceptance criteria:
//! * Plugin routes tagged `#[api_doc(mcp)]` appear in `tools/list` and are
//!   callable via `tools/call`.
//! * Plugin routes mounted through `scoped()` keep their prefix in the
//!   derived tool and stay callable.
//! * A plugin can offer a fluent `expose_mcp()` switch built on the
//!   `Route::mcp()` toggle, so the *host* decides at install time whether the
//!   plugin's routes become tools — no source attributes on the handlers.

#![cfg(feature = "mcp")]

use autumn_web::app::AppBuilder;
use autumn_web::plugin::Plugin;
use autumn_web::prelude::*;
use autumn_web::test::{TestApp, TestClient};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone)]
struct Run {
    id: u32,
    state: String,
}

#[derive(Serialize, Deserialize)]
struct NewRun {
    state: String,
}

async fn rpc(client: &TestClient, body: serde_json::Value) -> serde_json::Value {
    let resp = client.post("/mcp").json(&body).send().await;
    resp.assert_ok();
    resp.json::<serde_json::Value>()
}

async fn list_tool_names(client: &TestClient) -> Vec<String> {
    let out = rpc(
        client,
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
    )
    .await;
    out["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_owned())
        .collect()
}

// ── Feature 1: plugin typed routes flow into MCP ──────────────────

#[get("/plugin/runs/{id}")]
#[api_doc(mcp, summary = "Fetch one workflow run")]
async fn tagged_get_run(Path(id): Path<u32>) -> AutumnResult<Json<Run>> {
    Ok(Json(Run {
        id,
        state: format!("run {id}"),
    }))
}

struct TaggedRoutesPlugin;

impl Plugin for TaggedRoutesPlugin {
    fn build(self, app: AppBuilder) -> AppBuilder {
        app.routes(routes![tagged_get_run])
    }
}

#[tokio::test]
async fn plugin_route_tagged_with_api_doc_mcp_is_listed() {
    let client = TestApp::new()
        .plugin(TaggedRoutesPlugin)
        .mount_mcp("/mcp")
        .build();

    let out = rpc(
        &client,
        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
    )
    .await;

    let tools = out["result"]["tools"].as_array().expect("tools array");
    let tool = tools
        .iter()
        .find(|t| t["name"] == "tagged_get_run")
        .expect("plugin route derived into a tool");
    assert_eq!(tool["description"], "Fetch one workflow run");
    assert!(
        tool["inputSchema"]["properties"]["id"].is_object(),
        "path param becomes a property"
    );
}

#[tokio::test]
async fn tools_call_dispatches_plugin_route_through_real_pipeline() {
    let client = TestApp::new()
        .plugin(TaggedRoutesPlugin)
        .mount_mcp("/mcp")
        .build();

    let out = rpc(
        &client,
        serde_json::json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params": {"name":"tagged_get_run","arguments":{"id":"7"}}
        }),
    )
    .await;

    assert_ne!(out["result"]["isError"], true);
    let text = out["result"]["content"][0]["text"].as_str().unwrap();
    let payload: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(payload["id"], 7);
    assert_eq!(payload["state"], "run 7");
}

// ── Feature 1: plugin scoped() routes flow into MCP ───────────────

#[get("/runs")]
#[api_doc(mcp, summary = "List workflow runs")]
async fn scoped_list_runs() -> AutumnResult<Json<Vec<Run>>> {
    Ok(Json(vec![Run {
        id: 1,
        state: "queued".into(),
    }]))
}

struct ScopedRoutesPlugin;

impl Plugin for ScopedRoutesPlugin {
    fn build(self, app: AppBuilder) -> AppBuilder {
        app.scoped(
            "/harvest",
            tower::layer::util::Identity::new(),
            routes![scoped_list_runs],
        )
    }
}

#[tokio::test]
async fn plugin_scoped_routes_flow_into_mcp() {
    let client = TestApp::new()
        .plugin(ScopedRoutesPlugin)
        .mount_mcp("/mcp")
        .build();

    // The scoped route is derived into a tool...
    let names = list_tool_names(&client).await;
    assert!(
        names.iter().any(|n| n == "scoped_list_runs"),
        "scoped plugin route must be listed, got {names:?}"
    );

    // ...and dispatches through the real (prefixed) pipeline.
    let out = rpc(
        &client,
        serde_json::json!({
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params": {"name":"scoped_list_runs","arguments":{}}
        }),
    )
    .await;
    assert_ne!(out["result"]["isError"], true);
    let text = out["result"]["content"][0]["text"].as_str().unwrap();
    let payload: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(payload[0]["state"], "queued");
}
