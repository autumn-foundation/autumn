//! MCP `tools/call` dispatch honours structured query arguments (issue #1972).
//!
//! `build_request` renders a tool's `query` object into the dispatched URL. It
//! already expanded an array field to repeated keys, but `Query<T>` could not
//! decode repeated keys — so a tool whose derived `inputSchema` advertised
//! `tags: array` dispatched a request its own handler rejected with 400. Nested
//! objects were worse: they were stringified to JSON-in-a-string, which nothing
//! downstream could parse.
//!
//! These tests drive the full loop — derived schema → `tools/call` →
//! `build_request` → real handler pipeline → echoed JSON — so the advertised
//! contract and the wire encoding are proven to agree.

#![cfg(feature = "mcp")]

use autumn_web::openapi::OpenApiSchema;
use autumn_web::prelude::*;
use autumn_web::test::{TestApp, TestClient};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, OpenApiSchema)]
struct QueryFilter {
    status: String,
    limit: Option<u32>,
}

#[derive(Serialize, Deserialize, OpenApiSchema)]
struct QueryItem {
    sku: String,
    qty: u32,
}

#[derive(Serialize, Deserialize, OpenApiSchema)]
struct StructuredSearch {
    q: String,
    page: Option<u32>,
    tags: Option<Vec<String>>,
    filter: Option<QueryFilter>,
    items: Option<Vec<QueryItem>>,
}

#[get("/api/structured-search")]
#[api_doc(mcp, summary = "Search with structured query arguments")]
async fn structured_search(
    Query(args): Query<StructuredSearch>,
) -> AutumnResult<Json<StructuredSearch>> {
    Ok(Json(args))
}

fn client() -> TestClient {
    TestApp::new()
        .routes(routes![structured_search])
        .mount_mcp("/mcp")
        .build()
}

async fn rpc(client: &TestClient, body: serde_json::Value) -> serde_json::Value {
    let resp = client.post("/mcp").json(&body).send().await;
    resp.assert_ok();
    resp.json::<serde_json::Value>()
}

/// Call the tool with `arguments.query` and return the handler's echoed struct.
async fn call_query(query: serde_json::Value) -> serde_json::Value {
    let out = rpc(
        &client(),
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": "structured_search", "arguments": { "query": query } }
        }),
    )
    .await;
    assert_ne!(
        out["result"]["isError"],
        serde_json::Value::Bool(true),
        "tool call must not error: {out}"
    );
    let text = out["result"]["content"][0]["text"]
        .as_str()
        .expect("text content");
    serde_json::from_str(text).expect("handler echoes JSON")
}

/// The regression the issue names: an array query argument expands to repeated
/// keys, and the handler must now decode them into its `Vec<String>` field.
#[tokio::test]
async fn array_query_argument_round_trips_to_a_vec_field() {
    let echoed = call_query(serde_json::json!({ "q": "x", "tags": ["a", "b"] })).await;
    assert_eq!(echoed["tags"], serde_json::json!(["a", "b"]));
}

/// A nested object query argument round-trips instead of arriving as
/// JSON-in-a-string the extractor cannot parse.
#[tokio::test]
async fn nested_object_query_argument_round_trips() {
    let echoed =
        call_query(serde_json::json!({ "q": "x", "filter": { "status": "open", "limit": 5 } }))
            .await;
    assert_eq!(echoed["filter"]["status"], "open");
    assert_eq!(echoed["filter"]["limit"], 5);
}

/// An array-of-objects query argument round-trips through the indexed form.
#[tokio::test]
async fn array_of_objects_query_argument_round_trips() {
    let echoed = call_query(serde_json::json!({
        "q": "x",
        "items": [ { "sku": "A-1", "qty": 2 }, { "sku": "B-2", "qty": 3 } ]
    }))
    .await;
    assert_eq!(echoed["items"][0]["sku"], "A-1");
    assert_eq!(echoed["items"][0]["qty"], 2);
    assert_eq!(echoed["items"][1]["sku"], "B-2");
    assert_eq!(echoed["items"][1]["qty"], 3);
}

/// An explicit JSON `null` for an optional field means "absent", not the
/// literal four-character string `null` (which coerced to a 400).
#[tokio::test]
async fn null_query_argument_is_omitted_rather_than_stringified() {
    let echoed = call_query(serde_json::json!({ "q": "x", "page": null })).await;
    assert!(
        echoed["page"].is_null(),
        "page must decode as None: {echoed}"
    );
    assert_eq!(echoed["q"], "x");
}

/// Scalars keep their existing flat rendering.
#[tokio::test]
async fn scalar_query_arguments_still_round_trip() {
    let echoed = call_query(serde_json::json!({ "q": "hello", "page": 3 })).await;
    assert_eq!(echoed["q"], "hello");
    assert_eq!(echoed["page"], 3);
}

/// Values a query string cannot carry are refused up front rather than
/// dispatched as something the caller did not ask for: an empty array would
/// vanish (400-ing a required field), and a null element would shorten the
/// sequence and shift every later element.
#[tokio::test]
async fn inexpressible_query_arguments_are_refused_not_silently_altered() {
    for (label, query) in [
        ("empty array", serde_json::json!({ "q": "x", "tags": [] })),
        (
            "empty object",
            serde_json::json!({ "q": "x", "filter": {} }),
        ),
        (
            "null array element",
            serde_json::json!({ "q": "x", "tags": ["a", null] }),
        ),
    ] {
        let out = rpc(
            &client(),
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": { "name": "structured_search", "arguments": { "query": query } }
            }),
        )
        .await;
        assert!(
            out["error"].is_object() || out["result"]["isError"] == serde_json::Value::Bool(true),
            "{label} must be refused, not silently altered: {out}"
        );
    }
}

/// The advertised `inputSchema` exposes the nested field structure — the tool
/// contract the dispatch path now actually honours.
#[tokio::test]
async fn tool_schema_advertises_the_nested_query_structure() {
    let out = rpc(
        &client(),
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
    )
    .await;
    let tools = out["result"]["tools"].as_array().expect("tools array");
    let tool = tools
        .iter()
        .find(|t| t["name"] == "structured_search")
        .expect("tool present");
    let schema = &tool["inputSchema"];

    // `query` resolves (possibly through `$defs`) to the real field set.
    let query = &schema["properties"]["query"];
    let resolved = query["$ref"]
        .as_str()
        .and_then(|r| r.strip_prefix("#/$defs/"))
        .map_or(query, |name| &schema["$defs"][name]);
    assert_eq!(resolved["type"], "object", "query schema: {schema}");
    assert!(
        resolved["properties"]["filter"].is_object(),
        "nested `filter` field is advertised: {schema}"
    );
    assert_eq!(
        resolved["properties"]["tags"]["oneOf"][0]["type"], "array",
        "sequence field is advertised as an array: {schema}"
    );
}
