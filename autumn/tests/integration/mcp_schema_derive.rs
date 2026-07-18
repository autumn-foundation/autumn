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

// Serde-rename fidelity (issue #1972, Part 2 / Item 1): a container
// `rename_all`, a field-level `rename`, and a raw-ident field must all be
// reflected in the advertised property names + `required` entries.
#[derive(Serialize, Deserialize, OpenApiSchema)]
#[serde(rename_all = "camelCase")]
struct RenamedArgs {
    word_count: i64,
    #[serde(rename = "kind")]
    category: String,
    r#type: String,
    maybe_flag: Option<bool>,
}

#[test]
fn derived_schema_honors_serde_renames_and_raw_idents() {
    let schema = <RenamedArgs as OpenApiSchema>::schema();
    let props = schema["properties"].as_object().expect("properties object");

    // container rename_all = camelCase → snake_case field advertised camelCased.
    assert!(props.contains_key("wordCount"), "camelCased key: {schema}");
    assert!(!props.contains_key("word_count"), "raw key gone: {schema}");
    // field-level rename wins over rename_all.
    assert!(props.contains_key("kind"), "field rename wins: {schema}");
    assert!(!props.contains_key("category"), "raw field gone: {schema}");
    // raw ident `r#type` advertises the bare `type`, never `r#type`.
    assert!(props.contains_key("type"), "raw-ident stripped: {schema}");
    assert!(
        !props.contains_key("r#type"),
        "no r# prefix leaks: {schema}"
    );
    // optional field is still camelCased.
    assert!(
        props.contains_key("maybeFlag"),
        "optional camelCased: {schema}"
    );

    // `required` uses the same resolved names (and excludes the Option).
    let required: Vec<&str> = schema["required"]
        .as_array()
        .expect("required list")
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();
    assert!(
        required.contains(&"wordCount"),
        "required camelCased: {required:?}"
    );
    assert!(
        required.contains(&"kind"),
        "required field-rename: {required:?}"
    );
    assert!(
        required.contains(&"type"),
        "required raw-ident: {required:?}"
    );
    assert!(
        !required.contains(&"maybeFlag"),
        "optional not required: {required:?}"
    );
    // no stale raw names leak into required.
    assert!(!required.contains(&"word_count"));
    assert!(!required.contains(&"category"));
    assert!(!required.contains(&"r#type"));
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

// Collision-proof schema identity (issue #1972, Part 2 / Item 2): two derived
// structs sharing a final identifier (`create::Args` / `update::Args`) used as
// route args must resolve to DISTINCT, correctly-referenced component schemas —
// neither may shadow the other in the back-fill.
mod create {
    use super::{Deserialize, OpenApiSchema, Serialize};
    #[derive(Serialize, Deserialize, OpenApiSchema)]
    pub struct Args {
        pub create_only_field: String,
    }
}

mod update {
    use super::{Deserialize, OpenApiSchema, Serialize};
    #[derive(Serialize, Deserialize, OpenApiSchema)]
    pub struct Args {
        pub update_only_field: i64,
    }
}

#[get("/api/collide-create")]
#[api_doc(mcp, summary = "Query arg named Args (create module)")]
async fn collide_create(Query(_q): Query<create::Args>) -> AutumnResult<Json<serde_json::Value>> {
    Ok(Json(serde_json::json!({})))
}

#[post("/api/collide-update")]
#[api_doc(mcp, summary = "Body arg named Args (update module)")]
async fn collide_update(Json(_b): Json<update::Args>) -> AutumnResult<Json<serde_json::Value>> {
    Ok(Json(serde_json::json!({})))
}

#[test]
fn colliding_derive_names_resolve_to_distinct_schemas() {
    let docs = vec![
        __autumn_route_info_collide_create().api_doc,
        __autumn_route_info_collide_update().api_doc,
    ];
    let tools = derive_tools(&docs, false, None);

    let create_tool = tools.iter().find(|t| t.name() == "collide_create").unwrap();
    let update_tool = tools.iter().find(|t| t.name() == "collide_update").unwrap();

    let create_query = &create_tool.input_schema()["properties"]["query"];
    let update_body = &update_tool.input_schema()["properties"]["body"];

    // The two refs must be DISTINCT component keys — otherwise one shadows the
    // other and both would resolve to the same schema.
    let create_ref = create_query["$ref"]
        .as_str()
        .expect("create query is a $ref");
    let update_ref = update_body["$ref"].as_str().expect("update body is a $ref");
    assert_ne!(
        create_ref, update_ref,
        "distinct types must not share a component key: {create_ref} == {update_ref}"
    );

    // Each ref must resolve to its OWN field, never the other's.
    let create_schema = resolve_ref(create_tool.input_schema(), create_query);
    let update_schema = resolve_ref(update_tool.input_schema(), update_body);
    assert!(
        create_schema["properties"]
            .get("create_only_field")
            .is_some(),
        "create Args must expose its own field: {create_schema}"
    );
    assert!(
        create_schema["properties"]
            .get("update_only_field")
            .is_none(),
        "create Args must NOT be shadowed by update Args: {create_schema}"
    );
    assert!(
        update_schema["properties"]
            .get("update_only_field")
            .is_some(),
        "update Args must expose its own field: {update_schema}"
    );
    assert!(
        update_schema["properties"]
            .get("create_only_field")
            .is_none(),
        "update Args must NOT be shadowed by create Args: {update_schema}"
    );
}

// A `$ref` emitted *inside* a derived schema body must resolve through the same
// collision index in the MCP `$defs` projection (issue #1972 P1 follow-up): a
// wrapper tool whose field is a `create::Args` must inline the CREATE Args under
// its own `$defs` key, distinct from a sibling tool's `update::Args`.
mod wrap_create_mcp {
    use super::{Deserialize, OpenApiSchema, Serialize};
    #[derive(Serialize, Deserialize, OpenApiSchema)]
    pub struct Args {
        pub create_only_field: String,
    }
}

mod wrap_update_mcp {
    use super::{Deserialize, OpenApiSchema, Serialize};
    #[derive(Serialize, Deserialize, OpenApiSchema)]
    pub struct Args {
        pub update_only_field: i64,
    }
}

#[derive(Serialize, Deserialize, OpenApiSchema)]
struct WrapEnvelope {
    payload: wrap_create_mcp::Args,
    note: String,
}

#[post("/api/mcp-wrap-envelope")]
#[api_doc(mcp, summary = "Envelope whose field is a nested create Args")]
async fn wrap_envelope_tool(Json(_e): Json<WrapEnvelope>) -> AutumnResult<Json<serde_json::Value>> {
    Ok(Json(serde_json::json!({})))
}

#[post("/api/mcp-wrap-update")]
#[api_doc(mcp, summary = "Body arg named Args (update module)")]
async fn wrap_update_tool(
    Json(_b): Json<wrap_update_mcp::Args>,
) -> AutumnResult<Json<serde_json::Value>> {
    Ok(Json(serde_json::json!({})))
}

#[test]
fn nested_derived_ref_resolves_in_mcp_defs() {
    let docs = vec![
        __autumn_route_info_wrap_envelope_tool().api_doc,
        __autumn_route_info_wrap_update_tool().api_doc,
    ];
    let tools = derive_tools(&docs, false, None);
    let envelope_tool = tools
        .iter()
        .find(|t| t.name() == "wrap_envelope_tool")
        .unwrap();
    let update_tool = tools
        .iter()
        .find(|t| t.name() == "wrap_update_tool")
        .unwrap();

    // Resolve the envelope body → its nested `payload` ref, both through `$defs`.
    let schema = envelope_tool.input_schema();
    let envelope_schema = resolve_ref(schema, &schema["properties"]["body"]);
    let payload_ref = &envelope_schema["properties"]["payload"];
    let payload_schema = resolve_ref(schema, payload_ref);
    assert!(
        payload_schema["properties"]
            .get("create_only_field")
            .is_some(),
        "nested MCP ref must resolve to CREATE Args (not a placeholder, not update): {payload_schema}"
    );
    assert!(
        payload_schema["properties"]
            .get("update_only_field")
            .is_none(),
        "nested MCP ref must NOT resolve to UPDATE Args: {payload_schema}"
    );

    // The nested payload inlines under a DISTINCT `$defs` key from the sibling
    // update tool's body — neither shadows the other.
    let payload_key = payload_ref["$ref"]
        .as_str()
        .expect("payload is a $ref")
        .trim_start_matches("#/$defs/");
    let update_body = &update_tool.input_schema()["properties"]["body"];
    let update_key = update_body["$ref"]
        .as_str()
        .expect("update body is a $ref")
        .trim_start_matches("#/$defs/");
    assert_ne!(
        payload_key, update_key,
        "create and update Args must inline under distinct $defs keys: {payload_key} == {update_key}"
    );
    // And the update tool's body resolves to its OWN field.
    let update_schema = resolve_ref(update_tool.input_schema(), update_body);
    assert!(
        update_schema["properties"]
            .get("update_only_field")
            .is_some(),
        "update tool body must expose its own field: {update_schema}"
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
