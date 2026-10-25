//! Request and response schemas are chosen by position (issue #2607).
//!
//! One type can have two wire shapes. `#[private]`, `#[classified]` and hidden
//! `#[encrypted]` fields are skipped on serialize only, so a request carries
//! them and a response does not. These tests read the exported spec, not the
//! emitter internals.

#![cfg(all(feature = "db", feature = "openapi"))]

use autumn_web::openapi::{
    ApiDoc, OpenApiConfig, SchemaEntry, SchemaKind, generate_spec, is_opaque_object_schema,
    type_name_of,
};
use autumn_web::api_doc;
use autumn_web::prelude::Json;
use serde_json::Value;

mod schema {
    autumn_web::reexports::diesel::table! {
        dir_accounts (id) {
            id -> Int8,
            email -> Text,
            password_hash -> Text,
        }
    }
    autumn_web::reexports::diesel::table! {
        dir_patrons (id) {
            id -> Integer,
            name -> Text,
            email -> Text,
        }
    }
    autumn_web::reexports::diesel::table! {
        dir_vaults (id) {
            id -> Integer,
            token -> Text,
        }
    }
    autumn_web::reexports::diesel::table! {
        dir_tagged (id) {
            id -> Int8,
            title -> Text,
            tags -> Text,
        }
    }
    autumn_web::reexports::diesel::table! {
        dir_plain (id) {
            id -> Int8,
            name -> Text,
        }
    }
    autumn_web::reexports::diesel::table! {
        dir_deser (id) {
            id -> Int8,
            amount -> Int8,
        }
    }
}

use schema::{dir_accounts, dir_deser, dir_patrons, dir_plain, dir_tagged, dir_vaults};

#[autumn_web::model(table = "dir_accounts")]
pub struct DirAccount {
    #[id]
    pub id: i64,
    pub email: String,
    #[private]
    pub password_hash: String,
}

#[autumn_web::model(table = "dir_patrons")]
pub struct DirPatron {
    pub id: i32,
    pub name: String,
    #[classified]
    pub email: String,
}

#[autumn_web::model(table = "dir_vaults")]
pub struct DirVault {
    pub id: i32,
    #[encrypted]
    pub token: String,
}

#[autumn_web::model(table = "dir_tagged")]
pub struct DirTagged {
    #[id]
    pub id: i64,
    pub title: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub tags: String,
}

#[autumn_web::model(table = "dir_plain")]
pub struct DirPlain {
    #[id]
    pub id: i64,
    pub name: String,
}

fn amount_from_anything<'de, D>(de: D) -> Result<i64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    serde::Deserialize::deserialize(de)
}

#[autumn_web::model(table = "dir_deser")]
pub struct DirDeser {
    #[id]
    pub id: i64,
    #[serde(deserialize_with = "amount_from_anything")]
    pub amount: i64,
}

/// A symmetric wrapper around an asymmetric model.
#[derive(serde::Serialize, serde::Deserialize, autumn_web::openapi::OpenApiSchema)]
pub struct DirEnvelope {
    pub account: DirAccount,
}

#[autumn_web::post("/dir/accounts/echo")]
#[api_doc(mcp)]
async fn echo_dir_account(Json(a): Json<DirAccount>) -> Json<DirAccount> {
    Json(a)
}

#[autumn_web::post("/dir/accounts")]
async fn take_dir_account(Json(_a): Json<DirAccount>) -> &'static str {
    "ok"
}

#[autumn_web::post("/dir/patrons")]
async fn take_dir_patron(Json(_p): Json<DirPatron>) -> &'static str {
    "ok"
}

#[autumn_web::post("/dir/patrons/new/echo")]
async fn echo_new_dir_patron(Json(p): Json<NewDirPatron>) -> Json<NewDirPatron> {
    Json(p)
}

#[autumn_web::post("/dir/vaults/echo")]
async fn echo_dir_vault(Json(v): Json<DirVault>) -> Json<DirVault> {
    Json(v)
}

#[autumn_web::post("/dir/tagged/echo")]
async fn echo_dir_tagged(Json(t): Json<DirTagged>) -> Json<DirTagged> {
    Json(t)
}

#[autumn_web::post("/dir/tagged/new")]
async fn take_new_dir_tagged(Json(_t): Json<NewDirTagged>) -> &'static str {
    "ok"
}

#[autumn_web::get("/dir/tagged/search")]
async fn search_dir_tagged(
    autumn_web::extract::Query(_q): autumn_web::extract::Query<DirTagged>,
) -> &'static str {
    "ok"
}

#[autumn_web::post("/dir/plain/echo")]
async fn echo_dir_plain(Json(p): Json<DirPlain>) -> Json<DirPlain> {
    Json(p)
}

#[autumn_web::post("/dir/deser/echo")]
async fn echo_dir_deser(Json(d): Json<DirDeser>) -> Json<DirDeser> {
    Json(d)
}

#[autumn_web::post("/dir/envelopes/echo")]
async fn echo_dir_envelope(Json(e): Json<DirEnvelope>) -> Json<DirEnvelope> {
    Json(e)
}

// ── helpers ────────────────────────────────────────────────────────────

fn spec_json(docs: &[&ApiDoc]) -> Value {
    let spec = generate_spec(&OpenApiConfig::new("Dir", "1.0.0"), docs);
    serde_json::to_value(&spec).expect("spec serializes")
}

fn request_ref(spec: &Value, path: &str) -> String {
    spec["paths"][path]["post"]["requestBody"]["content"]["application/json"]["schema"]["$ref"]
        .as_str()
        .unwrap_or_else(|| panic!("request $ref at {path}: {spec}"))
        .to_owned()
}

fn response_ref(spec: &Value, path: &str) -> String {
    spec["paths"][path]["post"]["responses"]["200"]["content"]["application/json"]["schema"]["$ref"]
        .as_str()
        .unwrap_or_else(|| panic!("response $ref at {path}: {spec}"))
        .to_owned()
}

fn component<'a>(spec: &'a Value, reference: &str) -> &'a Value {
    let key = reference
        .strip_prefix("#/components/schemas/")
        .unwrap_or_else(|| panic!("not a component ref: {reference}"));
    let schema = &spec["components"]["schemas"][key];
    assert!(schema.is_object(), "component `{key}` missing: {spec}");
    schema
}

fn required(schema: &Value) -> Vec<&str> {
    schema["required"]
        .as_array()
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

fn has_property(schema: &Value, name: &str) -> bool {
    schema["properties"].get(name).is_some()
}

fn response_doc_for<T: ?Sized>(name: &'static str) -> ApiDoc {
    ApiDoc {
        method: "GET",
        path: "/dir/show",
        operation_id: "dir_show",
        success_status: 200,
        response: Some(SchemaEntry {
            name,
            kind: SchemaKind::Ref,
            identity: Some(type_name_of::<T>),
        }),
        ..Default::default()
    }
}

// ── Instance 1: the read schema under-specifies a request ─────────────

#[test]
fn private_field_is_required_in_request_and_absent_from_response() {
    let doc = __autumn_route_info_echo_dir_account().api_doc;
    let spec = spec_json(&[&doc]);

    let req_ref = request_ref(&spec, "/dir/accounts/echo");
    let resp_ref = response_ref(&spec, "/dir/accounts/echo");
    assert_eq!(resp_ref, "#/components/schemas/DirAccount");
    assert_eq!(req_ref, "#/components/schemas/DirAccountRequest");

    let req = component(&spec, &req_ref);
    assert!(has_property(req, "password_hash"), "{req}");
    assert!(required(req).contains(&"password_hash"), "{req}");

    let resp = component(&spec, &resp_ref);
    assert!(!has_property(resp, "password_hash"), "{resp}");
    assert!(!required(resp).contains(&"password_hash"), "{resp}");
}

#[test]
fn request_only_reference_keeps_the_short_key_with_the_request_schema() {
    let doc = __autumn_route_info_take_dir_account().api_doc;
    let spec = spec_json(&[&doc]);

    let req_ref = request_ref(&spec, "/dir/accounts");
    assert_eq!(req_ref, "#/components/schemas/DirAccount");
    let req = component(&spec, &req_ref);
    assert!(required(req).contains(&"password_hash"), "{req}");
    assert!(
        spec["components"]["schemas"]
            .get("DirAccountRequest")
            .is_none(),
        "one position, one component: {spec}"
    );
}

#[test]
fn classified_field_is_required_in_request_and_absent_from_response() {
    let take = __autumn_route_info_take_dir_patron().api_doc;
    let show = response_doc_for::<DirPatron>("DirPatron");
    let spec = spec_json(&[&take, &show]);

    let req = component(&spec, &request_ref(&spec, "/dir/patrons"));
    assert!(required(req).contains(&"email"), "{req}");

    let resp = &spec["components"]["schemas"]["DirPatron"];
    assert!(!has_property(resp, "email"), "{resp}");
}

#[test]
fn hidden_encrypted_field_is_required_in_request_and_absent_from_response() {
    let doc = __autumn_route_info_echo_dir_vault().api_doc;
    let spec = spec_json(&[&doc]);

    let req = component(&spec, &request_ref(&spec, "/dir/vaults/echo"));
    assert!(required(req).contains(&"token"), "{req}");
    let resp = component(&spec, &response_ref(&spec, "/dir/vaults/echo"));
    assert!(!has_property(resp, "token"), "{resp}");
}

// ── Instance 2: the `New*` schema over-specifies a response ───────────

#[test]
fn new_classified_field_is_required_in_request_and_absent_from_response() {
    let doc = __autumn_route_info_echo_new_dir_patron().api_doc;
    let spec = spec_json(&[&doc]);

    let req_ref = request_ref(&spec, "/dir/patrons/new/echo");
    let resp_ref = response_ref(&spec, "/dir/patrons/new/echo");
    assert_ne!(req_ref, resp_ref, "the two shapes differ: {spec}");

    let req = component(&spec, &req_ref);
    assert!(required(req).contains(&"email"), "{req}");

    let resp = component(&spec, &resp_ref);
    assert!(!has_property(resp, "email"), "{resp}");
    assert!(!required(resp).contains(&"email"), "{resp}");
    assert!(required(resp).contains(&"name"), "{resp}");
}

// ── Other serialize-only attributes ──────────────────────────────────

#[test]
fn skip_serializing_if_field_is_required_in_request_only() {
    let doc = __autumn_route_info_echo_dir_tagged().api_doc;
    let spec = spec_json(&[&doc]);

    let req = component(&spec, &request_ref(&spec, "/dir/tagged/echo"));
    assert!(required(req).contains(&"tags"), "{req}");
    let resp = component(&spec, &response_ref(&spec, "/dir/tagged/echo"));
    assert!(has_property(resp, "tags"), "{resp}");
    assert!(!required(resp).contains(&"tags"), "{resp}");
}

/// `NewDirTagged` does not copy `skip_serializing_if`, so serde requires the
/// field in both directions.
#[test]
fn new_struct_requires_a_field_with_model_skip_serializing_if() {
    let doc = __autumn_route_info_take_new_dir_tagged().api_doc;
    let spec = spec_json(&[&doc]);

    let req = component(&spec, &request_ref(&spec, "/dir/tagged/new"));
    assert!(required(req).contains(&"tags"), "{req}");
}

#[test]
fn query_parameters_use_the_request_schema() {
    let doc = __autumn_route_info_search_dir_tagged().api_doc;
    let spec = spec_json(&[&doc]);

    let params = spec["paths"]["/dir/tagged/search"]["get"]["parameters"]
        .as_array()
        .unwrap_or_else(|| panic!("parameters: {spec}"));
    let tags = params
        .iter()
        .find(|p| p["name"] == "tags")
        .unwrap_or_else(|| panic!("tags parameter: {spec}"));
    assert_eq!(tags["required"], true, "{tags}");
}

#[test]
fn deserialize_adapter_makes_the_request_schema_opaque() {
    let doc = __autumn_route_info_echo_dir_deser().api_doc;
    let spec = spec_json(&[&doc]);

    let req = component(&spec, &request_ref(&spec, "/dir/deser/echo"));
    assert!(is_opaque_object_schema(req), "{req}");
    let resp = component(&spec, &response_ref(&spec, "/dir/deser/echo"));
    assert!(has_property(resp, "amount"), "{resp}");
}

// ── Nested references follow the position of their parent ─────────────

#[test]
fn nested_reference_follows_the_parent_position() {
    let doc = __autumn_route_info_echo_dir_envelope().api_doc;
    let spec = spec_json(&[&doc]);

    let req_ref = request_ref(&spec, "/dir/envelopes/echo");
    let resp_ref = response_ref(&spec, "/dir/envelopes/echo");
    assert_ne!(
        req_ref, resp_ref,
        "a nested split splits the parent: {spec}"
    );

    let req_inner = component(&spec, &request_ref(&spec, "/dir/envelopes/echo"))["properties"]
        ["account"]["$ref"]
        .as_str()
        .unwrap_or_else(|| panic!("nested request ref: {spec}"))
        .to_owned();
    assert!(
        required(component(&spec, &req_inner)).contains(&"password_hash"),
        "{spec}"
    );

    let resp_inner = component(&spec, &resp_ref)["properties"]["account"]["$ref"]
        .as_str()
        .unwrap_or_else(|| panic!("nested response ref: {spec}"))
        .to_owned();
    assert_eq!(resp_inner, "#/components/schemas/DirAccount");
    assert!(
        !has_property(component(&spec, &resp_inner), "password_hash"),
        "{spec}"
    );
}

// ── The common case does not get uglier ───────────────────────────────

#[test]
fn symmetric_model_stays_one_component() {
    let doc = __autumn_route_info_echo_dir_plain().api_doc;
    let spec = spec_json(&[&doc]);

    assert_eq!(
        request_ref(&spec, "/dir/plain/echo"),
        "#/components/schemas/DirPlain"
    );
    assert_eq!(
        response_ref(&spec, "/dir/plain/echo"),
        "#/components/schemas/DirPlain"
    );
    let keys: Vec<&String> = spec["components"]["schemas"]
        .as_object()
        .expect("components")
        .keys()
        .filter(|k| k.contains("DirPlain"))
        .collect();
    assert_eq!(keys, vec!["DirPlain"], "{spec}");
}

#[test]
fn request_key_does_not_take_an_existing_component_name() {
    let doc = __autumn_route_info_echo_dir_account().api_doc;
    let config = OpenApiConfig::new("Dir", "1.0.0")
        .register_schema("DirAccountRequest", serde_json::json!({ "type": "string" }));
    let spec = serde_json::to_value(generate_spec(&config, &[&doc])).expect("spec");

    let req_ref = request_ref(&spec, "/dir/accounts/echo");
    assert_ne!(req_ref, "#/components/schemas/DirAccountRequest", "{spec}");
    assert!(
        required(component(&spec, &req_ref)).contains(&"password_hash"),
        "{spec}"
    );
    assert_eq!(
        spec["components"]["schemas"]["DirAccountRequest"]["type"], "string",
        "the user schema stays: {spec}"
    );
}

// ── MCP tools take arguments, so they use the request schema ──────────

#[cfg(feature = "mcp")]
#[test]
fn mcp_input_schema_uses_the_request_schema() {
    let docs = vec![__autumn_route_info_echo_dir_account().api_doc];
    let tools = autumn_web::mcp::derive_tools(&docs, false, None);
    let tool = tools
        .iter()
        .find(|t| t.name() == "echo_dir_account")
        .unwrap_or_else(|| {
            let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
            panic!("tool `echo_dir_account` missing: {names:?}")
        });
    let input = tool.input_schema();
    let body_ref = input["properties"]["body"]["$ref"]
        .as_str()
        .unwrap_or_else(|| panic!("body ref: {input}"));
    let key = body_ref.strip_prefix("#/$defs/").expect("local ref");
    let body = &input["$defs"][key];
    assert!(required(body).contains(&"password_hash"), "{input}");
}
