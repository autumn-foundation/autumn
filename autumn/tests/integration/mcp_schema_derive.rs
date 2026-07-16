//! `#[derive(OpenApiSchema)]` gives a plain handler-arg struct a real
//! MCP tool `inputSchema` instead of the generic object placeholder (issue #1972).
//!
//! DB-free: like `mcp_repository`, these tests collect the generated routes'
//! `ApiDoc` metadata and run it through the same production entry point the
//! router uses to assemble `/mcp` (`autumn_web::mcp::derive_tools`).
//!
//! Covers the FIRST-SLICE contract:
//! * A `#[derive(OpenApiSchema)]` struct used as a `Json<T>` body advertises a
//!   field-accurate `body` schema (properties + required), not
//!   `{"type":"object","title":"X"}`.
//! * The same for a `Query<T>` param struct under the `query` property.
//! * A struct WITHOUT the derive still degrades to the placeholder — proving the
//!   derive (not some unrelated change) is what produces the real schema.

#![cfg(feature = "mcp")]

use autumn_web::mcp::derive_tools;
use autumn_web::openapi::OpenApiSchema;
use autumn_web::prelude::*;
use serde::{Deserialize, Serialize};

// A plain (non-`#[model]`) struct with the new derive: one required string and
// one nullable integer — the canonical `struct { a: String, b: Option<i32> }`.
#[derive(Serialize, Deserialize, OpenApiSchema)]
struct DerivedArgs {
    a: String,
    b: Option<i32>,
}

// A plain struct WITHOUT the derive — the pre-#1972 behaviour (placeholder).
#[derive(Serialize, Deserialize)]
struct PlainArgs {
    a: String,
}

#[post("/api/derived-body")]
#[api_doc(mcp, summary = "Body arg with a derived schema")]
async fn derived_body(Json(_body): Json<DerivedArgs>) -> AutumnResult<Json<DerivedArgs>> {
    Ok(Json(DerivedArgs {
        a: "ok".into(),
        b: None,
    }))
}

#[get("/api/derived-query")]
#[api_doc(mcp, summary = "Query arg with a derived schema")]
async fn derived_query(Query(_q): Query<DerivedArgs>) -> AutumnResult<Json<DerivedArgs>> {
    Ok(Json(DerivedArgs {
        a: "ok".into(),
        b: None,
    }))
}

#[post("/api/plain-body")]
#[api_doc(mcp, summary = "Body arg without a derived schema")]
async fn plain_body(Json(_body): Json<PlainArgs>) -> AutumnResult<Json<PlainArgs>> {
    Ok(Json(PlainArgs { a: "ok".into() }))
}

#[test]
fn derived_schema_impl_is_field_accurate() {
    // The derive itself yields the mirrored `#[model]` schema shape.
    let schema = <DerivedArgs as OpenApiSchema>::schema();
    assert_eq!(DerivedArgs::schema_name(), "DerivedArgs");
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["properties"]["a"]["type"], "string");
    // Nullable field → `oneOf [ {integer}, {null} ]`.
    let one_of = schema["properties"]["b"]["oneOf"]
        .as_array()
        .expect("Option<i32> renders as oneOf");
    assert!(one_of.iter().any(|v| v["type"] == "integer"));
    assert!(one_of.iter().any(|v| v["type"] == "null"));
    // Only the non-`Option` field is required.
    let required = schema["required"].as_array().expect("has required list");
    assert!(required.iter().any(|v| v == "a"));
    assert!(!required.iter().any(|v| v == "b"));
}

#[test]
fn mcp_body_input_schema_reflects_derived_struct() {
    let docs = vec![__autumn_route_info_derived_body().api_doc];
    let tools = derive_tools(&docs, false, None);
    let tool = tools.iter().find(|t| t.name() == "derived_body").unwrap();
    let body = &tool.input_schema()["properties"]["body"];

    // A `$ref` into `$defs` OR the inlined object — either way it must resolve to
    // the real fields, never the `{type:object,title}` placeholder.
    let resolved = resolve_ref(tool.input_schema(), body);
    assert_eq!(
        resolved["type"], "object",
        "derived body schema: {resolved}"
    );
    assert_eq!(
        resolved["properties"]["a"]["type"], "string",
        "derived body must expose field `a`: {resolved}"
    );
    assert!(
        resolved["properties"].get("b").is_some(),
        "derived body must expose field `b`: {resolved}"
    );
    assert!(
        resolved.get("title").is_none() || resolved["properties"].is_object(),
        "must not be the bare placeholder: {resolved}"
    );
}

#[test]
fn mcp_query_input_schema_reflects_derived_struct() {
    let docs = vec![__autumn_route_info_derived_query().api_doc];
    let tools = derive_tools(&docs, false, None);
    let tool = tools.iter().find(|t| t.name() == "derived_query").unwrap();
    let query = &tool.input_schema()["properties"]["query"];

    let resolved = resolve_ref(tool.input_schema(), query);
    assert_eq!(
        resolved["type"], "object",
        "derived query schema: {resolved}"
    );
    assert_eq!(
        resolved["properties"]["a"]["type"], "string",
        "derived query must expose field `a`: {resolved}"
    );
}

#[test]
fn mcp_body_without_derive_stays_placeholder() {
    let docs = vec![__autumn_route_info_plain_body().api_doc];
    let tools = derive_tools(&docs, false, None);
    let tool = tools.iter().find(|t| t.name() == "plain_body").unwrap();
    let body = &tool.input_schema()["properties"]["body"];

    let resolved = resolve_ref(tool.input_schema(), body);
    // Pre-#1972 behaviour: no properties, just the object+title placeholder.
    assert_eq!(resolved["type"], "object");
    assert!(
        resolved["properties"].is_null()
            || resolved["properties"]
                .as_object()
                .is_none_or(serde_json::Map::is_empty),
        "a struct without the derive must stay a bare placeholder: {resolved}"
    );
}

/// If `value` is a `{ "$ref": "#/$defs/X" }` pointer, resolve it against the
/// schema's `$defs`; otherwise return the value unchanged.
fn resolve_ref(root: &serde_json::Value, value: &serde_json::Value) -> serde_json::Value {
    if let Some(reference) = value.get("$ref").and_then(serde_json::Value::as_str)
        && let Some(name) = reference.strip_prefix("#/$defs/")
    {
        return root["$defs"][name].clone();
    }
    value.clone()
}
