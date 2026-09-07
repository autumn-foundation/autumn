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

// A list-valued serde meta before the representation key. The guard has to
// consume `rename_all(...)` to reach `tag`; if it does not, `parse_nested_meta`
// aborts on the unread group and the enum is wrongly advertised as a string
// enum. Both that and a *disagreeing* split rename are now compile errors, so
// the rejection cases live in trybuild; this fixture pins the shape that is
// still accepted — a split whose two sides agree round-trips, so there is one
// spelling to advertise and no asymmetry to refuse.
#[derive(Serialize, Deserialize, OpenApiSchema)]
#[serde(rename_all(serialize = "snake_case", deserialize = "snake_case"))]
#[allow(dead_code)]
enum AgreeingSplitRename {
    InProgress,
}

// Rejected shapes (trybuild owns the compile-fail cases, listed here so the
// contract is readable in one place):
//
//   #[serde(rename_all(serialize = "snake_case"), tag = "kind")]   // guard bypass
//   #[serde(rename_all(serialize = "snake_case", deserialize = "camelCase"))]
//   #[serde(rename_all(serialize = "snake_case"))]                 // ONE side is
//                                                                  // asymmetric too:
//                                                                  // serde still
//                                                                  // deserializes the
//                                                                  // original spelling

#[test]
fn a_split_rename_that_agrees_is_accepted_and_applied() {
    let schema = <AgreeingSplitRename as OpenApiSchema>::schema();
    assert_eq!(
        schema["enum"],
        serde_json::json!(["in_progress"]),
        "both sides say snake_case, so the advertised value round-trips: {schema}"
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

/// A derived component whose field `$ref`s an opaque one: the operation reaches
/// the placeholder only through the component graph, never directly.
#[derive(Serialize, Deserialize, OpenApiSchema)]
#[allow(dead_code)]
struct Wrapper {
    inner: UntypedBody,
}

#[test]
fn attributes_an_opaque_component_reached_through_another_component() {
    let mut post = doc("POST", "/wrapped", "create_wrapped");
    post.request_body = Some(ref_entry(
        "Wrapper",
        autumn_web::openapi::type_name_of::<Wrapper>,
    ));

    let spec = generate_spec(&OpenApiConfig::new("Demo", "1.0.0"), &[&post]);
    let report = opaque_component_schemas(&spec);

    let entry = report
        .iter()
        .find(|e| e.schema == "UntypedBody")
        .unwrap_or_else(|| panic!("nested opaque type should be reported: {report:?}"));
    assert_eq!(
        entry.referenced_by,
        vec!["POST /wrapped".to_owned()],
        "the operation reaches it only via Wrapper, but its contract is still \
         the degraded one: {report:?}"
    );
}

#[test]
fn a_registered_map_schema_is_not_opaque() {
    // `{"type":"object","additionalProperties":…}` has no `properties`, but it
    // is a real contract a generator can render — flagging it would make
    // `--strict` fail CI over a fully typed map.
    assert!(!is_opaque_object_schema(&serde_json::json!({
        "type": "object",
        "additionalProperties": { "type": "string" }
    })));
    assert!(!is_opaque_object_schema(&serde_json::json!({
        "type": "object",
        "oneOf": [{ "type": "object", "properties": {} }]
    })));
    // Still a placeholder with the description the generator may attach.
    assert!(is_opaque_object_schema(&serde_json::json!({
        "type": "object", "title": "X", "description": "d"
    })));
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

// ── Requiredness follows serde, not the Rust type ──────────────────

// A non-`Option` field with `#[serde(skip_serializing_if)]` is now a compile
// error on this derive, so the positive case lives in `repository_openapi.rs`
// against a `#[model]`, whose read schema describes a response only.
//
// The reason for the split: `skip_serializing_if` governs serialization alone.
// A response may omit the field; serde still rejects a REQUEST that omits it.
// The `#[model]` query struct is response-only — the generated API takes
// `New*` / `Update*` as request bodies — so dropping it from `required` is
// sound there. This derive is applied to request types too, so one schema
// cannot be right for both and the shape is refused:
//
//   #[derive(OpenApiSchema)]
//   struct Bad {
//       #[serde(skip_serializing_if = "Vec::is_empty")]
//       tags: Vec<String>,          // must NOT compile
//   }
//
// On `Option<T>` there is no conflict — already not required — and it compiles:
#[derive(Serialize, Deserialize, OpenApiSchema)]
#[allow(dead_code)]
struct OptionalWithSkip {
    always: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    sometimes: Option<String>,
}

#[test]
fn skip_serializing_if_on_an_option_is_accepted_and_already_optional() {
    let schema = <OptionalWithSkip as OpenApiSchema>::schema();
    assert!(schema["properties"]["sometimes"].is_object(), "{schema}");

    let required: Vec<&str> = schema["required"]
        .as_array()
        .map(|a| a.iter().filter_map(serde_json::Value::as_str).collect())
        .unwrap_or_default();
    assert!(required.contains(&"always"), "{schema}");
    assert!(
        !required.contains(&"sometimes"),
        "an Option is not required regardless of the skip predicate: {schema}"
    );
}

// ── Scalar field types that are not Rust primitives ────────────────

// Only `OpenApiSchema` — the derive under test needs no serde impls, and this
// crate's `uuid` dependency is built without its `serde` feature.
#[derive(OpenApiSchema)]
#[allow(dead_code)]
struct Timestamped {
    created_at: chrono::NaiveDateTime,
    seen_on: chrono::NaiveDate,
    id: uuid::Uuid,
}

#[test]
fn datetime_and_uuid_fields_are_inline_scalars_not_dangling_refs() {
    let schema = <Timestamped as OpenApiSchema>::schema();
    let props = &schema["properties"];

    assert_eq!(props["created_at"]["type"], "string");
    assert_eq!(props["seen_on"]["format"], "date");
    assert_eq!(props["id"]["format"], "uuid");

    // A naive value carries NO RFC 3339 format: chrono writes it without a UTC
    // offset, so claiming `date-time` would describe a payload a strict
    // validator rejects and a generator would type as timezone-aware.
    assert!(
        props["created_at"].get("format").is_none(),
        "NaiveDateTime must not claim an offset-bearing format: {schema}"
    );
    assert!(
        props["created_at"]["description"]
            .as_str()
            .is_some_and(|d| d.contains("no UTC offset")),
        "the shape it does serialize is documented instead: {schema}"
    );

    // The point: none of them emits a `$ref` to a component nothing registers,
    // which the back-fill would resolve to the opaque placeholder. A
    // `created_at` column is near-universal on `#[model]` types.
    for field in ["created_at", "seen_on", "id"] {
        assert!(
            props[field].get("$ref").is_none(),
            "{field} must not be a dangling ref: {schema}"
        );
    }
}
