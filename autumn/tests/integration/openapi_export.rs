//! The spec-fidelity half of `autumn openapi export` (issue #802).
//!
//! Two related contracts, both about a spec that a client generator can
//! actually consume:
//!
//! * `#[derive(OpenApiSchema)]` on a unit-variant enum advertises the closed
//!   string set serde puts on the wire, so an enum in a body or query stops
//!   resolving to the opaque `{"type":"object"}` placeholder.
//! * `openapi::opaque_component_schemas` finds the components that DID degrade
//!   to that placeholder, and names the operations reaching them — the report
//!   `autumn openapi export` prints (and `--strict` fails on) instead of
//!   shipping a half-untyped contract silently.

#![cfg(feature = "openapi")]

use autumn_web::openapi::{
    ApiDoc, OpenApiConfig, OpenApiSchema, SchemaEntry, SchemaKind, generate_spec,
    is_opaque_object_schema, opaque_component_schemas,
};
use serde::{Deserialize, Serialize};

// ── Enum derive ────────────────────────────────────────────────────

// Never constructed: these exist to be *described*, and the assertions read
// their derived schema rather than any value.
#[derive(Serialize, Deserialize, OpenApiSchema)]
#[allow(dead_code)]
enum Priority {
    Low,
    High,
}

#[derive(Serialize, Deserialize, OpenApiSchema)]
#[allow(dead_code)]
#[serde(rename_all = "snake_case")]
enum RenamedStatus {
    Open,
    InProgress,
    #[serde(rename = "done!")]
    Done,
    #[serde(skip)]
    Internal,
}

#[test]
fn unit_enum_derives_a_closed_string_set() {
    let schema = <Priority as OpenApiSchema>::schema();
    assert_eq!(schema["type"], "string");
    assert_eq!(schema["enum"], serde_json::json!(["Low", "High"]));
    // The whole point: this is not the placeholder a bare enum used to get.
    assert!(!is_opaque_object_schema(&schema), "{schema}");
}

#[test]
fn enum_honors_rename_all_rename_and_skip() {
    let schema = <RenamedStatus as OpenApiSchema>::schema();
    assert_eq!(
        schema["enum"],
        serde_json::json!(["open", "in_progress", "done!"]),
        "rename_all converts from PascalCase, a variant `rename` wins over it, \
         and a skipped variant never reaches the wire: {schema}"
    );
}

#[test]
fn enum_schema_name_is_the_bare_type_name() {
    assert_eq!(<Priority as OpenApiSchema>::schema_name(), "Priority");
}

// ── Opaque-component reporting ─────────────────────────────────────

// A struct WITHOUT the derive: the spec back-fills the placeholder for it.
#[derive(Serialize, Deserialize)]
#[allow(dead_code)]
struct UntypedBody {
    _whatever: String,
}

#[derive(Serialize, Deserialize, OpenApiSchema)]
#[allow(dead_code)]
struct TypedBody {
    name: String,
}

fn doc(method: &'static str, path: &'static str, op: &'static str) -> ApiDoc {
    ApiDoc {
        method,
        path,
        operation_id: op,
        success_status: 200,
        ..ApiDoc::default()
    }
}

fn ref_entry(name: &'static str, identity: fn() -> &'static str) -> SchemaEntry {
    SchemaEntry {
        name,
        kind: SchemaKind::Ref,
        identity: Some(identity),
    }
}

#[test]
fn reports_an_opaque_component_and_the_operation_reaching_it() {
    let mut post = doc("POST", "/things", "create_thing");
    post.request_body = Some(ref_entry(
        "UntypedBody",
        autumn_web::openapi::type_name_of::<UntypedBody>,
    ));

    let spec = generate_spec(&OpenApiConfig::new("Demo", "1.0.0"), &[&post]);
    let report = opaque_component_schemas(&spec);

    let entry = report
        .iter()
        .find(|e| e.schema == "UntypedBody")
        .unwrap_or_else(|| panic!("UntypedBody should be opaque: {report:?}"));
    assert_eq!(entry.referenced_by, vec!["POST /things".to_owned()]);
}

#[test]
fn a_derived_component_is_not_reported() {
    let mut post = doc("POST", "/things", "create_thing");
    post.request_body = Some(ref_entry(
        "TypedBody",
        autumn_web::openapi::type_name_of::<TypedBody>,
    ));

    let spec = generate_spec(&OpenApiConfig::new("Demo", "1.0.0"), &[&post]);
    let report = opaque_component_schemas(&spec);

    assert!(
        !report.iter().any(|e| e.schema == "TypedBody"),
        "a derived schema carries real properties: {report:?}"
    );
}

#[test]
fn a_spec_with_no_referenced_types_reports_nothing() {
    let get = doc("GET", "/ping", "ping");
    let spec = generate_spec(&OpenApiConfig::new("Demo", "1.0.0"), &[&get]);
    // `ProblemDetails` is always registered and always has real properties, so
    // a spec that references no user types must come back clean.
    assert_eq!(opaque_component_schemas(&spec), Vec::new());
}

#[test]
fn opaque_predicate_distinguishes_a_fieldless_struct_from_a_placeholder() {
    // A registered object with an empty `properties` map is a real (if empty)
    // contract; only a missing `properties` key is the placeholder.
    assert!(!is_opaque_object_schema(&serde_json::json!({
        "type": "object", "properties": {}
    })));
    assert!(is_opaque_object_schema(&serde_json::json!({
        "type": "object", "title": "Whatever"
    })));
    assert!(!is_opaque_object_schema(&serde_json::json!({
        "type": "string"
    })));
}
