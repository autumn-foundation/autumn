//! `WebMCP` projection (issue #2619): `tools.json` and the page shim.

#![cfg(feature = "webmcp")]

use autumn_web::openapi::OpenApiSchema;
use autumn_web::prelude::*;
use autumn_web::test::{TestApp, TestClient};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Serialize, Deserialize, OpenApiSchema)]
struct NewItem {
    title: String,
    priority: Option<i32>,
}

#[derive(Serialize, Deserialize, OpenApiSchema)]
struct Item {
    id: i64,
    title: String,
}

#[get("/items")]
#[api_doc(webmcp, summary = "List items")]
async fn list_items() -> Json<Vec<Item>> {
    Json(Vec::new())
}

#[post("/items")]
#[api_doc(mcp, webmcp, summary = "Add an item")]
async fn add_item(Json(item): Json<NewItem>) -> Json<Item> {
    Json(Item {
        id: 1,
        title: item.title,
    })
}

#[delete("/items/{id}")]
#[api_doc(webmcp, summary = "Delete an item")]
async fn delete_item(axum::extract::Path(_id): axum::extract::Path<i64>) -> Json<bool> {
    Json(true)
}

#[get("/mcp-only")]
#[api_doc(mcp, summary = "Only for /mcp")]
async fn mcp_only() -> Json<bool> {
    Json(true)
}

#[get("/never")]
#[api_doc(webmcp = false)]
async fn never() -> Json<bool> {
    Json(true)
}

#[get("/untagged")]
async fn untagged() -> Json<bool> {
    Json(true)
}

#[post("/untagged-write")]
async fn untagged_write() -> Json<bool> {
    Json(true)
}

#[get("/page")]
#[api_doc(webmcp)]
async fn html_page() -> &'static str {
    "<p>hi</p>"
}

#[get("/secret")]
#[secured]
#[api_doc(webmcp, summary = "Read a secret")]
async fn secret() -> Json<&'static str> {
    Json("the-secret")
}

async fn manifest(client: &TestClient) -> Value {
    let res = client.get(autumn_web::webmcp::TOOLS_PATH).send().await;
    res.assert_status(200);
    res.json::<Value>()
}

fn names(manifest: &Value) -> Vec<String> {
    manifest["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_owned())
        .collect()
}

fn tool<'a>(manifest: &'a Value, name: &str) -> &'a Value {
    manifest["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == name)
        .unwrap_or_else(|| panic!("no tool {name}: {manifest}"))
}

#[tokio::test]
async fn only_routes_tagged_webmcp_are_tools() {
    let client = TestApp::new()
        .routes(routes![
            list_items,
            add_item,
            delete_item,
            mcp_only,
            never,
            untagged,
            html_page
        ])
        .mount_webmcp()
        .build();

    let m = manifest(&client).await;
    let mut got = names(&m);
    got.sort();
    assert_eq!(got, ["add_item", "delete_item", "list_items"], "{m}");
    assert_eq!(m["version"], 1);
    assert_eq!(m["csrfHeader"], "x-csrf-token");
}

#[tokio::test]
async fn a_tool_has_the_dispatch_fields_the_shim_reads() {
    let client = TestApp::new()
        .routes(routes![list_items, add_item, delete_item])
        .mount_webmcp()
        .build();
    let m = manifest(&client).await;

    let add = tool(&m, "add_item");
    assert_eq!(add["description"], "Add an item");
    assert_eq!(add["method"], "POST");
    assert_eq!(add["path_template"], "/items");
    assert_eq!(add["has_body"], true);
    assert_eq!(add["has_query"], false);
    assert_eq!(add["annotations"]["readOnlyHint"], false);
    assert_eq!(add["webmcp_annotations"]["consequentialHint"], true);
    assert_eq!(
        add["confirm"], false,
        "a POST with no grant is not destructive"
    );

    let delete = tool(&m, "delete_item");
    assert_eq!(delete["path_params"], serde_json::json!(["id"]));
    assert_eq!(delete["confirm"], true, "a DELETE asks the user first");

    let list = tool(&m, "list_items");
    assert_eq!(list["webmcp_annotations"]["readOnlyHint"], true);
    assert_eq!(list["webmcp_annotations"]["consequentialHint"], false);
    assert_eq!(list["confirm"], false);
}

#[tokio::test]
async fn the_input_schema_is_byte_identical_to_the_mcp_schema() {
    let client = TestApp::new()
        .routes(routes![list_items, add_item])
        .mount_mcp("/mcp")
        .mount_webmcp()
        .build();

    let rpc = client
        .post("/mcp")
        .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}))
        .send()
        .await;
    rpc.assert_ok();
    let listed = rpc.json::<Value>();
    let mcp_add = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "add_item")
        .expect("add_item on /mcp")
        .clone();

    let m = manifest(&client).await;
    let web_add = tool(&m, "add_item");
    assert_eq!(
        serde_json::to_string(&web_add["inputSchema"]).unwrap(),
        serde_json::to_string(&mcp_add["inputSchema"]).unwrap(),
    );
    assert_eq!(web_add["annotations"], mcp_add["annotations"]);
    assert!(
        web_add["inputSchema"]["$defs"]["NewItem"]["properties"]["title"].is_object(),
        "{web_add}"
    );
}

#[tokio::test]
async fn the_hatch_takes_reads_only() {
    let client = TestApp::new()
        .routes(routes![untagged, untagged_write, never])
        .expose_all_as_webmcp()
        .build();
    let m = manifest(&client).await;
    assert_eq!(names(&m), ["untagged"], "{m}");
}

#[tokio::test]
async fn mcp_hatch_does_not_expose_webmcp_tools() {
    let client = TestApp::new()
        .routes(routes![untagged, mcp_only])
        .expose_all_as_mcp()
        .mount_webmcp()
        .build();
    let m = manifest(&client).await;
    assert!(names(&m).is_empty(), "{m}");
}

#[tokio::test]
async fn no_mount_serves_an_empty_list() {
    // A page with only form tools still fetches the list. An empty list
    // gives no 404, so the browser logs no error.
    let client = TestApp::new().routes(routes![list_items]).build();
    let m = manifest(&client).await;
    assert!(names(&m).is_empty(), "{m}");
}

#[tokio::test]
async fn the_manifest_is_private_and_not_indexed() {
    let client = TestApp::new()
        .routes(routes![list_items])
        .mount_webmcp()
        .build();
    let res = client.get(autumn_web::webmcp::TOOLS_PATH).send().await;
    res.assert_status(200)
        .assert_header_contains("content-type", "application/json")
        .assert_header("cache-control", "private, no-cache")
        .assert_header("x-robots-tag", "noindex");
}

#[get("/_autumn/webmcp/tools.json")]
async fn own_tools() -> &'static str {
    "mine"
}

#[tokio::test]
async fn a_user_route_at_the_manifest_path_wins() {
    let client = TestApp::new()
        .routes(routes![own_tools, list_items])
        .mount_webmcp()
        .build();
    let res = client.get(autumn_web::webmcp::TOOLS_PATH).send().await;
    res.assert_status(200);
    assert_eq!(res.text(), "mine");
}

#[tokio::test]
async fn the_shim_is_served_under_the_default_csp() {
    let client = TestApp::new().routes(routes![list_items]).build();
    let res = client.get(autumn_web::webmcp::SHIM_PATH).send().await;
    res.assert_status(200)
        .assert_header_contains("content-type", "javascript")
        .assert_header_contains("content-security-policy", "script-src 'self'");
    let js = res.text();
    assert!(js.contains("registerTool"), "{js}");
    assert!(js.contains("document.modelContext"), "{js}");
    assert!(js.contains("navigator.modelContext"), "{js}");
    assert!(!js.contains("<script"), "{js}");
    for call in [
        "console.log",
        "console.warn",
        "console.error",
        "console.info",
    ] {
        assert!(
            !js.contains(call),
            "the shim writes no console output: {call}"
        );
    }

    // The hashed URL serves the same bytes.
    let hashed = autumn_web::assets::asset_url("js/autumn-webmcp.js");
    assert_ne!(hashed, autumn_web::webmcp::SHIM_PATH);
    let res = client.get(&hashed).send().await;
    res.assert_status(200);
    assert_eq!(res.text(), js);
}

#[tokio::test]
async fn a_secured_tool_rejects_an_anonymous_call_and_serves_a_session() {
    // The shim calls the route itself, so the route's own guard decides.
    let client = TestApp::new()
        .routes(routes![secret])
        .mount_webmcp()
        .build();
    let m = manifest(&client).await;
    assert_eq!(tool(&m, "secret")["path_template"], "/secret");

    let res = client.get("/secret").send().await;
    res.assert_status(401);

    client.login_as("user-1").await;
    let res = client.get("/secret").send().await;
    res.assert_status(200);
    assert!(res.text().contains("the-secret"));
}
