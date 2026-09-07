//! Integration tests for `#[repository]` CRUD routes' `OpenAPI` metadata.
//!
//! The repository macro auto-generates five HTTP handlers (list, get,
//! create, update, delete) when `api = "/path"` is supplied. This test
//! pins their `ApiDoc` shape so the generated spec accurately reflects
//! the JSON request/response bodies that Autumn mounts.

#![cfg(all(feature = "db", feature = "openapi"))]

use autumn_web::openapi::SchemaKind;

mod schema {
    autumn_web::reexports::diesel::table! {
        widgets (id) {
            id -> Int8,
            name -> Text,
        }
    }
}

use schema::widgets;

#[autumn_web::model]
pub struct Widget {
    #[id]
    pub id: i64,
    pub name: String,
}

#[autumn_web::repository(Widget, api = "/api/widgets")]
pub trait WidgetRepository {}

#[test]
fn list_route_returns_paginated_page_envelope() {
    // #1237: the list operation documents the `Page` envelope response and the
    // `page`/`size` query parameters, not a bare unbounded array.
    let route = __autumn_route_info_widget_api_list();
    assert_eq!(route.api_doc.method, "GET");
    assert_eq!(route.api_doc.path, "/api/widgets");
    assert_eq!(route.api_doc.success_status, 200);
    let resp = route
        .api_doc
        .response
        .as_ref()
        .expect("list must document its JSON response");
    assert_eq!(
        resp.kind,
        SchemaKind::Ref,
        "list response is the Page envelope, referenced by name"
    );
    assert_eq!(resp.name, "WidgetPage");

    let query = route
        .api_doc
        .query_schema
        .as_ref()
        .expect("list must document its pagination query params");
    assert_eq!(query.name, "PageRequest");
    assert_eq!(query.kind, SchemaKind::Ref);

    // The register_schemas hook must materialize the envelope + query schemas.
    let register = route
        .api_doc
        .register_schemas
        .expect("list must register its Page/PageRequest schemas");
    let mut registry = autumn_web::openapi::SchemaRegistry::default();
    register(&mut registry);
    let schemas = registry.schemas();
    let page_schema = schemas
        .get("WidgetPage")
        .expect("WidgetPage envelope schema must be registered");
    assert_eq!(page_schema["properties"]["total_pages"]["type"], "integer");
    assert_eq!(
        page_schema["properties"]["content"]["items"]["$ref"],
        "#/components/schemas/Widget"
    );
    let query_schema = schemas
        .get("PageRequest")
        .expect("PageRequest query schema must be registered");
    assert!(query_schema["properties"]["page"].is_object());
    assert!(query_schema["properties"]["size"].is_object());
}

#[test]
fn get_route_returns_single_widget_ref() {
    let route = __autumn_route_info_widget_api_get();
    assert_eq!(route.api_doc.method, "GET");
    assert_eq!(route.api_doc.path_params, &["id"]);
    let resp = route.api_doc.response.as_ref().expect("get response");
    assert_eq!(resp.name, "Widget");
    assert_eq!(resp.kind, SchemaKind::Ref);
    assert!(route.api_doc.request_body.is_none());
}

#[test]
fn repository_api_path_helpers_percent_encode_ids() {
    assert_eq!(__autumn_path_widget_api_get("a/b"), "/api/widgets/a%2Fb");
    assert_eq!(
        __autumn_path_widget_api_update("hello world/é"),
        "/api/widgets/hello%20world%2F%C3%A9"
    );
    assert_eq!(
        __autumn_path_widget_api_delete("a?b#c"),
        "/api/widgets/a%3Fb%23c"
    );
}

#[test]
fn create_route_takes_new_widget_returns_widget() {
    let route = __autumn_route_info_widget_api_create();
    assert_eq!(route.api_doc.method, "POST");
    assert_eq!(route.api_doc.success_status, 201);
    let body = route
        .api_doc
        .request_body
        .as_ref()
        .expect("create must document a request body");
    assert_eq!(body.name, "NewWidget");
    assert_eq!(body.kind, SchemaKind::Ref);
    let resp = route.api_doc.response.as_ref().expect("create response");
    assert_eq!(resp.name, "Widget");
}

#[test]
fn update_route_takes_update_widget_and_id() {
    let route = __autumn_route_info_widget_api_update();
    assert_eq!(route.api_doc.method, "PUT");
    assert_eq!(route.api_doc.path_params, &["id"]);
    let body = route
        .api_doc
        .request_body
        .as_ref()
        .expect("update must document a request body");
    assert_eq!(body.name, "UpdateWidget");
    let resp = route.api_doc.response.as_ref().expect("update response");
    assert_eq!(resp.name, "Widget");
}

#[test]
fn delete_route_has_no_body_and_uses_204() {
    let route = __autumn_route_info_widget_api_delete();
    assert_eq!(route.api_doc.method, "DELETE");
    assert_eq!(route.api_doc.path_params, &["id"]);
    assert_eq!(route.api_doc.success_status, 204);
    assert!(route.api_doc.request_body.is_none());
    assert!(route.api_doc.response.is_none());
}

#[test]
fn model_impl_open_api_schema_returns_object_type() {
    use autumn_web::openapi::OpenApiSchema;
    let schema = Widget::schema();
    assert_eq!(schema["type"], "object");
}

#[test]
fn model_schema_includes_all_fields_as_properties() {
    use autumn_web::openapi::OpenApiSchema;
    let schema = Widget::schema();
    let props = schema["properties"]
        .as_object()
        .expect("properties must be an object");
    assert!(props.contains_key("id"), "should have id property");
    assert!(props.contains_key("name"), "should have name property");
}

#[test]
fn model_schema_maps_i64_to_integer() {
    use autumn_web::openapi::OpenApiSchema;
    let schema = Widget::schema();
    assert_eq!(schema["properties"]["id"]["type"], "integer");
}

#[test]
fn model_schema_maps_string_to_string_type() {
    use autumn_web::openapi::OpenApiSchema;
    let schema = Widget::schema();
    assert_eq!(schema["properties"]["name"]["type"], "string");
}

#[test]
fn model_schema_lists_non_optional_fields_as_required() {
    use autumn_web::openapi::OpenApiSchema;
    let schema = Widget::schema();
    let required = schema["required"]
        .as_array()
        .expect("required must be an array");
    let req_names: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
    assert!(req_names.contains(&"id"));
    assert!(req_names.contains(&"name"));
}

#[test]
fn new_model_schema_excludes_id_field() {
    use autumn_web::openapi::OpenApiSchema;
    let schema = NewWidget::schema();
    let props = schema["properties"].as_object().expect("properties");
    assert!(!props.contains_key("id"), "NewWidget should not have id");
    assert!(props.contains_key("name"));
}

#[test]
fn update_model_schema_has_no_required_fields() {
    use autumn_web::openapi::OpenApiSchema;
    let schema = UpdateWidget::schema();
    assert!(
        schema["required"].is_null(),
        "UpdateWidget should have no required fields, got: {:?}",
        schema["required"]
    );
}

// Model with Vec<T> and Option<T> fields to exercise array/nullable schema emission.
mod schema2 {
    autumn_web::reexports::diesel::table! {
        tagged_widgets (id) {
            id -> Int8,
            tags -> Array<Text>,
            description -> Nullable<Text>,
        }
    }
}

use schema2::tagged_widgets;

#[autumn_web::model(table = "tagged_widgets")]
pub struct TaggedWidget {
    #[id]
    pub id: i64,
    pub tags: Vec<String>,
    pub description: Option<String>,
}

#[test]
fn vec_field_emits_array_schema() {
    use autumn_web::openapi::OpenApiSchema;
    let schema = TaggedWidget::schema();
    let tags = &schema["properties"]["tags"];
    assert_eq!(tags["type"], "array", "Vec<String> should emit type:array");
    assert_eq!(
        tags["items"]["type"], "string",
        "Vec<String> items should be string"
    );
}

// ── MCP opt-in: `mcp` / `mcp = "read"` on the repository macro ────

mod schema3 {
    autumn_web::reexports::diesel::table! {
        gadgets (id) {
            id -> Int8,
            name -> Text,
        }
    }
}

use schema3::gadgets;

#[autumn_web::model]
pub struct Gadget {
    #[id]
    pub id: i64,
    pub name: String,
}

#[autumn_web::repository(Gadget, api = "/api/gadgets", mcp)]
pub trait GadgetRepository {}

mod schema4 {
    autumn_web::reexports::diesel::table! {
        sprockets (id) {
            id -> Int8,
            name -> Text,
        }
    }
}

use schema4::sprockets;

#[autumn_web::model]
pub struct Sprocket {
    #[id]
    pub id: i64,
    pub name: String,
}

#[autumn_web::repository(Sprocket, api = "/api/sprockets", mcp = "read")]
pub trait SprocketRepository {}

#[test]
fn repository_mcp_opts_in_all_five_crud_routes() {
    assert!(__autumn_route_info_gadget_api_list().api_doc.mcp_tool);
    assert!(__autumn_route_info_gadget_api_get().api_doc.mcp_tool);
    assert!(__autumn_route_info_gadget_api_create().api_doc.mcp_tool);
    assert!(__autumn_route_info_gadget_api_update().api_doc.mcp_tool);
    assert!(__autumn_route_info_gadget_api_delete().api_doc.mcp_tool);
}

#[test]
fn repository_mcp_read_exposes_only_list_and_get() {
    assert!(__autumn_route_info_sprocket_api_list().api_doc.mcp_tool);
    assert!(__autumn_route_info_sprocket_api_get().api_doc.mcp_tool);
    assert!(!__autumn_route_info_sprocket_api_create().api_doc.mcp_tool);
    assert!(!__autumn_route_info_sprocket_api_update().api_doc.mcp_tool);
    assert!(!__autumn_route_info_sprocket_api_delete().api_doc.mcp_tool);
}

#[test]
fn repository_without_mcp_defaults_off() {
    assert!(!__autumn_route_info_widget_api_list().api_doc.mcp_tool);
    assert!(!__autumn_route_info_widget_api_get().api_doc.mcp_tool);
    assert!(!__autumn_route_info_widget_api_create().api_doc.mcp_tool);
    assert!(!__autumn_route_info_widget_api_update().api_doc.mcp_tool);
    assert!(!__autumn_route_info_widget_api_delete().api_doc.mcp_tool);
}

#[test]
fn option_field_emits_nullable_schema() {
    use autumn_web::openapi::OpenApiSchema;
    let schema = TaggedWidget::schema();
    let desc = &schema["properties"]["description"];
    let one_of = desc["oneOf"]
        .as_array()
        .expect("Option<T> should emit oneOf");
    assert_eq!(one_of.len(), 2);
    assert!(
        one_of.iter().any(|v| v["type"] == "null"),
        "oneOf should include null type"
    );
    assert!(
        one_of.iter().any(|v| v["type"] == "string"),
        "oneOf should include string type"
    );
}

// ── A re-shaped model declines automatic registration (issue #802) ─────

mod schema_reshaped {
    autumn_web::reexports::diesel::table! {
        reshaped_rows (id) {
            id -> Int8,
            title -> Text,
        }
    }
}

use schema_reshaped::reshaped_rows;

/// What `ReshapedRow` actually serializes to — a single string, not an object.
#[derive(serde::Serialize)]
pub struct WireReshaped(String);

impl From<ReshapedRow> for WireReshaped {
    fn from(row: ReshapedRow) -> Self {
        Self(row.title)
    }
}

#[autumn_web::model(table = "reshaped_rows")]
#[serde(into = "WireReshaped")]
pub struct ReshapedRow {
    #[id]
    pub id: i64,
    pub title: String,
}

/// A model whose container serde attribute re-shapes the response must NOT
/// register its field-by-field schema, because that schema describes a payload
/// the server never sends.
///
/// Declining registration rather than erroring: the model falls back to the
/// opaque `{"type":"object"}` placeholder, which is the pre-#802 behaviour and
/// is honest — `autumn openapi export` names every placeholder it emits, so the
/// author is told. Publishing the wrong shape would be worse than publishing
/// none, since a generated client acts on it.
///
/// Only the SERIALIZE side matters here: this schema describes a response, and
/// the generated API takes `New*` / `Update*` as request bodies. `from` /
/// `try_from` (deserialize-side) and a split `rename_all` (whose serialize side
/// is the one a response uses) therefore still register normally.
#[test]
fn a_reshaped_model_does_not_register_its_field_schema() {
    let registered =
        autumn_web::openapi::registered_derived_schema(std::any::type_name::<ReshapedRow>());
    assert!(
        registered.is_none(),
        "`#[serde(into = ...)]` makes the field-by-field schema wrong, so it must not be \
         advertised: {registered:?}"
    );

    // The write companions do not inherit container attributes, so their
    // schemas are still accurate and still register.
    assert!(
        autumn_web::openapi::registered_derived_schema(std::any::type_name::<NewReshapedRow>())
            .is_some(),
        "NewReshapedRow is unaffected by the model's container attribute"
    );
}

// ── The Update* patch schema is nullable on both sides (issue #802) ────

/// Every mutable field of an `Update*` companion is a `Patch<T>`, and `null`
/// carries meaning in BOTH directions: `Deserialize` maps `null` to
/// `Patch::Clear`, and `Serialize` writes `null` for `Unchanged` and `Clear`.
///
/// So the registered schema has to admit null. Advertising a plain, non-nullable
/// `T` would have a validator reject a legitimate "clear this column" request
/// AND reject any response that serializes an `Update*` with a field left
/// unchanged — the second reachable because the descriptor now selects this
/// schema for a route returning `Json<UpdateBookmark>`.
#[test]
fn the_update_schema_admits_null_for_every_patch_field() {
    let schema = autumn_web::openapi::registered_derived_schema(std::any::type_name::<
        UpdateConditionalRow,
    >())
    .expect("the update companion registers its schema");
    let props = schema["properties"]
        .as_object()
        .expect("an object schema with properties");

    for name in ["title", "tags"] {
        let prop = props.get(name).unwrap_or_else(|| panic!("{name} present"));
        let admits_null = prop["oneOf"]
            .as_array()
            .is_some_and(|branches| branches.iter().any(|b| b["type"] == "null"));
        assert!(
            admits_null,
            "`{name}` is a Patch<T>, so the schema must admit null: {prop}"
        );
    }
}

// ── Field-level re-shaping on the read schema (issue #802) ─────────────

mod schema_field_reshaped {
    autumn_web::reexports::diesel::table! {
        adapter_rows (id) {
            id -> Int8,
            amount -> Int8,
        }
    }
    autumn_web::reexports::diesel::table! {
        deser_adapter_rows (id) {
            id -> Int8,
            amount -> Int8,
        }
    }
}

use schema_field_reshaped::{adapter_rows, deser_adapter_rows};

/// Writes the `i64` as a JSON *string*, so the field's Rust type no longer
/// describes what reaches the wire.
///
/// `&i64` is not a style choice: serde's `serialize_with` calls the function
/// with a reference to the field, so taking it by value does not compile.
#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's serialize_with contract dictates the &T parameter"
)]
fn amount_as_string<S>(value: &i64, ser: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    ser.serialize_str(&value.to_string())
}

fn amount_from_anything<'de, D>(de: D) -> Result<i64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    serde::Deserialize::deserialize(de)
}

#[autumn_web::model(table = "adapter_rows")]
pub struct AdapterRow {
    #[id]
    pub id: i64,
    #[serde(serialize_with = "amount_as_string")]
    pub amount: i64,
}

#[autumn_web::model(table = "deser_adapter_rows")]
pub struct DeserAdapterRow {
    #[id]
    pub id: i64,
    #[serde(deserialize_with = "amount_from_anything")]
    pub amount: i64,
}

/// A serialization adapter on any field the response carries re-shapes that
/// property, so the field-by-field schema must not be registered — the same
/// rule the container attributes follow, asked of the fields.
///
/// `deserialize_with` is the control: it changes only what a request is parsed
/// from, and this schema describes a response, so it still registers.
#[test]
fn a_field_serialization_adapter_blocks_registration() {
    let registered =
        autumn_web::openapi::registered_derived_schema(std::any::type_name::<AdapterRow>());
    assert!(
        registered.is_none(),
        "`#[serde(serialize_with)]` decides what reaches the wire, so a schema built from \
         the Rust type must not be advertised: {registered:?}"
    );

    assert!(
        autumn_web::openapi::registered_derived_schema(std::any::type_name::<DeserAdapterRow>())
            .is_some(),
        "`#[serde(deserialize_with)]` leaves the response shape alone, so the read schema is \
         still accurate and must still register"
    );
}

// ── Conditional omission on the read schema (issue #802) ───────────────

mod schema_conditional {
    autumn_web::reexports::diesel::table! {
        conditional_rows (id) {
            id -> Int8,
            title -> Text,
            tags -> Text,
        }
    }
}

use schema_conditional::conditional_rows;

#[autumn_web::model(table = "conditional_rows")]
pub struct ConditionalRow {
    #[id]
    pub id: i64,
    pub title: String,
    /// Omitted from a response whenever the predicate matches.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub tags: String,
}

/// The read schema keeps the property but must NOT require it.
///
/// Sound because this schema describes a RESPONSE only — the generated API
/// takes `New*` / `Update*` as request bodies, never the query struct. A
/// response tripping the predicate omits `tags`, and a strict generated client
/// would reject that response if the schema demanded it.
///
/// `#[derive(OpenApiSchema)]` deliberately refuses this shape instead, because
/// the same type there may be a `Json<T>` request and `skip_serializing_if`
/// governs serialization alone — serde still rejects a request that omits it.
#[test]
fn read_schema_keeps_conditionally_omitted_field_but_does_not_require_it() {
    use autumn_web::openapi::OpenApiSchema;
    let schema = ConditionalRow::schema();

    assert!(
        schema["properties"]["tags"].is_object(),
        "the field does reach some responses: {schema}"
    );

    let required: Vec<&str> = schema["required"]
        .as_array()
        .expect("required")
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();
    assert!(required.contains(&"title"), "{required:?}");
    assert!(
        !required.contains(&"tags"),
        "a response may omit it, so a client must not demand it: {required:?}"
    );
}

// ── Serde-rename fidelity on `#[model]` (issue #1972, Part 2 / Item 1) ──

mod schema_rename {
    autumn_web::reexports::diesel::table! {
        renamed_rows (id) {
            id -> Int8,
            word_count -> Int8,
            category -> Text,
            active -> Nullable<Bool>,
        }
    }
}

use schema_rename::renamed_rows;

#[autumn_web::model(table = "renamed_rows")]
#[serde(rename_all = "camelCase")]
pub struct RenamedRow {
    #[id]
    pub id: i64,
    pub word_count: i64,
    #[serde(rename = "kind")]
    pub category: String,
    pub active: Option<bool>,
}

#[test]
fn model_schema_honors_serde_rename_all_and_field_rename() {
    use autumn_web::openapi::OpenApiSchema;
    let schema = RenamedRow::schema();
    let props = schema["properties"].as_object().expect("properties");

    // container rename_all = camelCase.
    assert!(props.contains_key("wordCount"), "camelCased: {schema}");
    assert!(!props.contains_key("word_count"), "no raw key: {schema}");
    // field-level rename wins over rename_all.
    assert!(props.contains_key("kind"), "field rename wins: {schema}");
    assert!(!props.contains_key("category"), "raw field gone: {schema}");
    // optional field still camelCased (id is not renamed — already flat).
    assert!(props.contains_key("active"), "optional present: {schema}");

    let required: Vec<&str> = schema["required"]
        .as_array()
        .expect("required")
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();
    assert!(
        required.contains(&"wordCount"),
        "required camelCased: {required:?}"
    );
    assert!(
        required.contains(&"kind"),
        "required field rename: {required:?}"
    );
    assert!(
        !required.contains(&"active"),
        "optional not required: {required:?}"
    );
    assert!(!required.contains(&"word_count"));
    assert!(!required.contains(&"category"));
}

/// The write companions advertise RAW identifiers, not the model's serde names
/// (issue #802) — the inverse of `model_schema_honors_serde_rename_all_and_field_rename`
/// above, and deliberately so.
///
/// This test previously asserted the opposite, that `NewRenamedRow::schema()`
/// was camelCased like the model. That was wrong about the struct it describes:
/// the generated `New*` / `Update*` structs get `#[derive(Serialize,
/// Deserialize)]` but NOT the model's container attributes, and their fields do
/// not carry field-level `#[serde(rename)]` either — so serde decodes a POST
/// body under the bare Rust identifiers. `form_for_derive.rs` proves it at
/// runtime, round-tripping `title=…&author_name=…&word_count=…` into a `NewX`.
///
/// The mismatch was harmless while nothing consumed these schemas. Once
/// `#[model]` registers them, a camelCased create schema makes every generated
/// client's POST fail with a missing-field error, so the schema has to describe
/// the struct that actually exists.
///
/// The read schema still honors renames, and correctly: the query struct DOES
/// receive the container attributes.
#[test]
fn new_model_schema_advertises_raw_identifiers_not_serde_renames() {
    use autumn_web::openapi::OpenApiSchema;
    let schema = NewRenamedRow::schema();
    let props = schema["properties"].as_object().expect("properties");

    // What `NewRenamedRow` actually deserializes.
    assert!(
        props.contains_key("word_count"),
        "raw identifier on New: {schema}"
    );
    assert!(
        props.contains_key("category"),
        "field rename NOT applied on New: {schema}"
    );
    // The model's serde spellings must not appear — they would be rejected.
    assert!(!props.contains_key("wordCount"), "no camelCase: {schema}");
    assert!(!props.contains_key("kind"), "no field rename: {schema}");

    assert!(!props.contains_key("id"), "New excludes id: {schema}");
}

// ── Model referenced by BOTH a `#[repository]` and a hand-written
//    `#[api_doc]` handler stays ONE short component (issue #1972, Part 2 / Item 2)
//
// The repository's model-typed refs (get/create/update responses, new/update
// bodies) now carry the same `type_name` identity as a hand-written handler's
// `Json<T>` ref, so `build_schema_component_index` sees a single identity under
// base `Gadget` — no false collision, no module-path-qualified duplicate. Before
// the fix the repository refs keyed on the short name while the handler ref keyed
// on `type_name`, splitting `Gadget` into `Gadget` + `<module>.Gadget`.

#[autumn_web::post("/api/gadgets/echo")]
async fn echo_gadget(
    autumn_web::prelude::Json(g): autumn_web::prelude::Json<Gadget>,
) -> autumn_web::prelude::Json<Gadget> {
    autumn_web::prelude::Json(g)
}

#[test]
fn model_shared_by_repository_and_handwritten_route_is_one_short_component() {
    use autumn_web::openapi::{OpenApiConfig, generate_spec};

    // `Gadget` is referenced by its repository (create response, `/api/gadgets`)
    // and by the hand-written `echo_gadget` handler (`Json<Gadget>` body, at
    // `/api/gadgets/echo`).
    let create = __autumn_route_info_gadget_api_create().api_doc;
    let echo = __autumn_route_info_echo_gadget().api_doc;
    let config = OpenApiConfig::new("Demo", "1.0.0");
    let spec = generate_spec(&config, &[&create, &echo]);

    // Repository create route's 201 response references the model.
    let repo_ref = spec.paths["/api/gadgets"].post.as_ref().unwrap().responses["201"]
        .content["application/json"]
        .schema["$ref"]
        .as_str()
        .expect("repository create response $ref")
        .to_owned();
    // Hand-written route's request body references the same model.
    let handwritten_ref = spec.paths["/api/gadgets/echo"]
        .post
        .as_ref()
        .unwrap()
        .request_body
        .as_ref()
        .unwrap()
        .content["application/json"]
        .schema["$ref"]
        .as_str()
        .expect("hand-written request-body $ref")
        .to_owned();

    // Both resolve to the SAME single short component key — no module-path leak,
    // no duplicate opaque component.
    assert_eq!(
        repo_ref, "#/components/schemas/Gadget",
        "repository ref must stay the short `Gadget` key"
    );
    assert_eq!(
        handwritten_ref, "#/components/schemas/Gadget",
        "hand-written ref must share the short `Gadget` key, not `<module>.Gadget`"
    );

    // Exactly one component key for `Gadget` — nothing module-qualified alongside it.
    let components = spec.components.expect("components");
    assert!(
        components.schemas.contains_key("Gadget"),
        "single `Gadget` component present"
    );
    let gadget_like: Vec<&String> = components
        .schemas
        .keys()
        .filter(|k| {
            k.ends_with("Gadget") && k.as_str() != "NewGadget" && k.as_str() != "UpdateGadget"
        })
        .collect();
    assert_eq!(
        gadget_like,
        vec![&"Gadget".to_owned()],
        "the model must not split into a second module-qualified component: {gadget_like:?}"
    );
}
