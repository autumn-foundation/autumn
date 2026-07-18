//! Integration tests for `OpenAPI` auto-generation (S-056).
//!
//! Covers:
//! * Route macros emit the expected `ApiDoc` metadata.
//! * `#[api_doc(...)]` overrides flow through to the generated spec.
//! * `AppBuilder::openapi(...)` mounts `/v3/api-docs` and `/swagger-ui`.

#![cfg(feature = "openapi")]

use autumn_web::Route;
use autumn_web::openapi::{ApiDoc, OpenApiConfig, SchemaKind};
use autumn_web::prelude::*;
use autumn_web::test::TestApp;

// ── Route-level metadata extraction ────────────────────────────────

#[get("/hello")]
async fn hello() -> &'static str {
    "hi"
}

#[get("/users/{id}")]
async fn get_user(Path(id): Path<i32>) -> String {
    format!("User {id}")
}

#[get("/posts/{year}/{slug}")]
async fn get_post(_params: Path<(i32, String)>) -> &'static str {
    "post"
}

#[post("/items")]
async fn create_item(Json(body): Json<serde_json::Value>) -> axum::Json<serde_json::Value> {
    axum::Json(body)
}

#[get("/admin")]
#[api_doc(summary = "Admin area", tag = "admin", status = 201)]
async fn admin() -> &'static str {
    "admin"
}

#[get("/hidden")]
#[api_doc(hidden)]
async fn hidden_route() -> &'static str {
    "hidden"
}

#[get("/tagged")]
#[api_doc(tags = ["users", "auth"], description = "Multi-tagged route")]
async fn tagged() -> &'static str {
    "tagged"
}

// Exercise the *reversed* attribute order: `#[api_doc]` above `#[get]`.
// Rust expands `#[api_doc]` first; the standalone macro must reorder
// so the route macro still sees the overrides.
#[api_doc(summary = "Top-first api_doc", tag = "top")]
#[get("/top-first")]
async fn top_first() -> &'static str {
    "top"
}

#[api_doc(hidden)]
#[post("/top-hidden")]
async fn top_hidden() -> &'static str {
    "hidden"
}

// Responses wrapped in `(StatusCode, Json<T>)` — common for 201 Created
// handlers — should still be inferred.
#[post("/things")]
async fn create_thing() -> (http::StatusCode, axum::Json<serde_json::Value>) {
    (http::StatusCode::CREATED, axum::Json(serde_json::json!({})))
}

// Qualified form: `#[api_doc(...)]` on top, `#[autumn_web::get(...)]`
// qualified below. The reorder helper must recognize the qualified
// path via its last segment so the overrides still flow through.
#[api_doc(summary = "Fully-qualified get")]
#[autumn_web::get("/qualified")]
async fn qualified_get() -> &'static str {
    "ok"
}

// `Json<Vec<T>>` — the generator must emit an array schema rather
// than collapsing to a `$ref` to `Vec`.
#[derive(serde::Serialize, serde::Deserialize)]
struct Widget {
    id: i32,
}

#[get("/widgets")]
async fn list_widgets() -> axum::Json<Vec<Widget>> {
    axum::Json(vec![])
}

#[post("/widgets")]
async fn post_widgets(axum::Json(_body): axum::Json<Vec<Widget>>) -> http::StatusCode {
    http::StatusCode::OK
}

// `Valid<Json<T>>` is Autumn's documented validation pattern. The
// generator must see straight through the wrapper so the resulting
// spec still reports a request body.
#[derive(serde::Deserialize, serde::Serialize, validator::Validate)]
struct NewWidget {
    #[validate(length(min = 1))]
    name: String,
}

#[post("/validated-widgets")]
async fn create_validated_widget(
    _body: autumn_web::Valid<autumn_web::Json<NewWidget>>,
) -> http::StatusCode {
    http::StatusCode::CREATED
}

// ── Query parameter inference ─────────────────────────────────────────

#[derive(serde::Deserialize)]
#[allow(dead_code)]
struct SearchParams {
    q: Option<String>,
    page: Option<i32>,
}

#[get("/search")]
async fn search(_params: Query<SearchParams>) -> &'static str {
    "results"
}

// ── Security scheme detection ─────────────────────────────────────────

#[get("/protected")]
#[secured]
async fn protected_handler() -> AutumnResult<&'static str> {
    Ok("secret")
}

#[get("/admin-only")]
#[secured("admin")]
async fn admin_handler() -> AutumnResult<&'static str> {
    Ok("admin")
}

#[secured]
#[get("/protected-top-first")]
async fn top_first_protected_handler() -> AutumnResult<&'static str> {
    Ok("secret")
}

#[secured("admin", "editor")]
#[get("/admin-top-first")]
async fn top_first_admin_handler() -> AutumnResult<&'static str> {
    Ok("admin")
}

#[test]
fn get_macro_populates_api_doc() {
    let route = __autumn_route_info_hello();
    assert_eq!(route.api_doc.method, "GET");
    assert_eq!(route.api_doc.path, "/hello");
    assert_eq!(route.api_doc.operation_id, "hello");
    assert_eq!(route.api_doc.success_status, 200);
    assert!(!route.api_doc.hidden);
    assert!(route.api_doc.path_params.is_empty());
}

#[test]
fn path_parameters_are_extracted() {
    let route = __autumn_route_info_get_user();
    assert_eq!(route.api_doc.path_params, &["id"]);
}

#[test]
fn multiple_path_parameters_are_extracted() {
    let route = __autumn_route_info_get_post();
    assert_eq!(route.api_doc.path_params, &["year", "slug"]);
}

#[test]
fn json_request_body_is_inferred() {
    let route = __autumn_route_info_create_item();
    let body = route
        .api_doc
        .request_body
        .as_ref()
        .expect("Json<...> body should be inferred");
    assert_eq!(body.name, "Value");
    assert_eq!(body.kind, SchemaKind::Ref);
}

#[test]
fn json_response_is_inferred() {
    let route = __autumn_route_info_create_item();
    let resp = route
        .api_doc
        .response
        .as_ref()
        .expect("Json<...> return should be inferred");
    assert_eq!(resp.name, "Value");
}

#[test]
fn api_doc_attribute_applies_summary_and_tag() {
    let route = __autumn_route_info_admin();
    assert_eq!(route.api_doc.summary, Some("Admin area"));
    assert_eq!(route.api_doc.tags, &["admin"]);
    assert_eq!(route.api_doc.success_status, 201);
}

#[test]
fn api_doc_attribute_can_hide_route() {
    let route = __autumn_route_info_hidden_route();
    assert!(route.api_doc.hidden);
}

#[test]
fn api_doc_attribute_accepts_tag_list() {
    let route = __autumn_route_info_tagged();
    assert_eq!(route.api_doc.tags, &["users", "auth"]);
    assert_eq!(route.api_doc.description, Some("Multi-tagged route"));
}

#[test]
fn api_doc_survives_when_placed_above_route_attribute() {
    let route = __autumn_route_info_top_first();
    assert_eq!(
        route.api_doc.summary,
        Some("Top-first api_doc"),
        "`#[api_doc]` above `#[get]` must not be dropped"
    );
    assert_eq!(route.api_doc.tags, &["top"]);
}

#[test]
fn api_doc_hidden_survives_when_placed_above_route_attribute() {
    let route = __autumn_route_info_top_hidden();
    assert!(route.api_doc.hidden);
}

#[test]
fn status_tuple_response_is_inferred_as_json() {
    let route = __autumn_route_info_create_thing();
    let resp = route
        .api_doc
        .response
        .as_ref()
        .expect("(StatusCode, Json<T>) should be inferred");
    assert_eq!(resp.name, "Value");
    assert_eq!(resp.kind, SchemaKind::Ref);
}

#[test]
fn api_doc_survives_above_qualified_route_attribute() {
    let route = __autumn_route_info_qualified_get();
    assert_eq!(
        route.api_doc.summary,
        Some("Fully-qualified get"),
        "qualified route attr should still be detected by the reorder helper"
    );
}

#[test]
fn json_vec_response_is_emitted_as_array_schema() {
    let route = __autumn_route_info_list_widgets();
    let resp = route
        .api_doc
        .response
        .as_ref()
        .expect("Json<Vec<T>> must infer a response");
    assert!(
        matches!(resp.kind, SchemaKind::Array(_)),
        "Json<Vec<T>> must become Array, got {:?}",
        resp.kind
    );

    // Render through the spec generator to confirm the actual JSON.
    let config = OpenApiConfig::new("Demo", "1.0.0");
    let spec = autumn_web::openapi::generate_spec(&config, &[&route.api_doc]);
    let media =
        &spec.paths["/widgets"].get.as_ref().unwrap().responses["200"].content["application/json"];
    assert_eq!(media.schema["type"], "array");
    assert_eq!(
        media.schema["items"]["$ref"], "#/components/schemas/Widget",
        "array items must still ref the element type"
    );
}

#[test]
fn json_vec_request_body_is_emitted_as_array_schema() {
    let route = __autumn_route_info_post_widgets();
    let body = route
        .api_doc
        .request_body
        .as_ref()
        .expect("Json<Vec<T>> request body must infer");
    assert!(matches!(body.kind, SchemaKind::Array(_)));
}

#[test]
fn valid_json_request_body_is_inferred() {
    let route = __autumn_route_info_create_validated_widget();
    let body = route
        .api_doc
        .request_body
        .as_ref()
        .expect("Valid<Json<T>> request body should be inferred");
    assert_eq!(body.name, "NewWidget");
    assert_eq!(body.kind, SchemaKind::Ref);
}

// ── Spec generation pipeline ───────────────────────────────────────

#[test]
fn generate_spec_emits_paths_for_every_method() {
    let routes: Vec<Route> = routes![hello, get_user, create_item, admin, hidden_route];
    let docs: Vec<&ApiDoc> = routes.iter().map(|r| &r.api_doc).collect();
    let config = OpenApiConfig::new("Test API", "0.1.0");
    let spec = autumn_web::openapi::generate_spec(&config, &docs);

    assert_eq!(spec.info.title, "Test API");
    assert_eq!(spec.info.version, "0.1.0");
    assert!(spec.paths.contains_key("/hello"));
    assert!(spec.paths.contains_key("/users/{id}"));
    assert!(spec.paths.contains_key("/items"));
    assert!(spec.paths.contains_key("/admin"));
    assert!(
        !spec.paths.contains_key("/hidden"),
        "`#[api_doc(hidden)]` should exclude routes"
    );

    // Admin returned a 201
    let admin_op = spec.paths["/admin"].get.as_ref().unwrap();
    assert!(admin_op.responses.contains_key("201"));
    assert_eq!(admin_op.summary.as_deref(), Some("Admin area"));

    // Path parameter surfaced
    let user_op = spec.paths["/users/{id}"].get.as_ref().unwrap();
    assert_eq!(user_op.parameters.len(), 1);
    assert_eq!(user_op.parameters[0].name, "id");
}

// ── Endpoint integration ──────────────────────────────────────────

#[tokio::test]
async fn openapi_json_endpoint_returns_spec() {
    let client = TestApp::new()
        .routes(routes![hello, get_user])
        .openapi(OpenApiConfig::new("Demo", "1.0.0"))
        .build();

    let response = client.get("/openapi.json").send().await;
    response.assert_ok();

    let body: serde_json::Value = response.json();
    assert_eq!(body["openapi"], "3.1.0");
    assert_eq!(body["info"]["title"], "Demo");
    assert!(body["paths"]["/hello"].is_object());
    assert!(body["paths"]["/users/{id}"].is_object());
}

#[tokio::test]
async fn swagger_ui_endpoint_returns_html_referencing_spec_url() {
    let client = TestApp::new()
        .routes(routes![hello])
        .openapi(OpenApiConfig::new("Demo", "1.0.0"))
        .build();

    let response = client.get("/swagger-ui").send().await;
    response.assert_ok();

    let body = response.text();
    let csp = response
        .header("content-security-policy")
        .expect("default security headers should include a CSP");
    assert!(csp.contains("script-src 'self'"), "csp = {csp}");
    assert!(body.contains("/swagger-ui/swagger-ui.css"));
    assert!(body.contains("/swagger-ui/swagger-ui-bundle.js"));
    assert!(body.contains("/swagger-ui/swagger-initializer.js"));
    assert!(!body.contains("unpkg.com"));
    assert!(!body.contains("window.onload = function()"));
}

#[tokio::test]
async fn swagger_ui_assets_are_served_same_origin() {
    let client = TestApp::new()
        .routes(routes![hello])
        .openapi(OpenApiConfig::new("Demo", "1.0.0"))
        .build();

    client
        .get("/swagger-ui/swagger-ui.css")
        .send()
        .await
        .assert_ok()
        .assert_header("content-type", "text/css; charset=utf-8");
    client
        .get("/swagger-ui/swagger-ui-bundle.js")
        .send()
        .await
        .assert_ok()
        .assert_header("content-type", "application/javascript; charset=utf-8");
    let init = client
        .get("/swagger-ui/swagger-initializer.js")
        .send()
        .await;
    init.assert_ok()
        .assert_header("content-type", "application/javascript; charset=utf-8");
    assert!(init.text().contains(r#""/openapi.json""#));
}

#[tokio::test]
async fn openapi_not_mounted_without_explicit_call() {
    let client = TestApp::new().routes(routes![hello]).build();
    let response = client.get("/openapi.json").send().await;
    assert_eq!(
        response.status,
        http::StatusCode::NOT_FOUND,
        "/openapi.json should 404 until AppBuilder::openapi(...) is called"
    );
}

#[tokio::test]
async fn custom_openapi_paths_are_honored() {
    let config = OpenApiConfig::new("Demo", "1.0.0")
        .openapi_json_path("/api/openapi.json")
        .swagger_ui_path(Some("/docs".to_owned()));

    let client = TestApp::new()
        .routes(routes![hello])
        .openapi(config)
        .build();

    client.get("/api/openapi.json").send().await.assert_ok();
    client.get("/docs").send().await.assert_ok();
    client.get("/docs/swagger-ui.css").send().await.assert_ok();
    client
        .get("/docs/swagger-initializer.js")
        .send()
        .await
        .assert_ok();

    // The default path should return 404 when a custom path is set.
    let default_json = client.get("/openapi.json").send().await;
    assert_eq!(default_json.status, http::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn swagger_ui_can_be_disabled() {
    let config = OpenApiConfig::new("Demo", "1.0.0").swagger_ui_path(None);
    let client = TestApp::new()
        .routes(routes![hello])
        .openapi(config)
        .build();

    client.get("/openapi.json").send().await.assert_ok();
    let ui = client.get("/swagger-ui").send().await;
    assert_eq!(ui.status, http::StatusCode::NOT_FOUND);
}

// ── Default path ──────────────────────────────────────────────────

#[test]
fn openapi_json_default_path_is_openapi_json() {
    let config = OpenApiConfig::new("Demo", "1.0.0");
    assert_eq!(
        config.openapi_json_path, "/openapi.json",
        "default openapi JSON path must be /openapi.json per issue #523"
    );
}

// ── Query parameter inference ─────────────────────────────────────

#[test]
fn query_extractor_populates_query_schema() {
    let route = __autumn_route_info_search();
    let query = route
        .api_doc
        .query_schema
        .as_ref()
        .expect("Query<T> extractor must populate query_schema");
    assert_eq!(query.name, "SearchParams");
    assert_eq!(query.kind, SchemaKind::Ref);
}

#[test]
fn query_params_appear_in_generated_spec() {
    let route = __autumn_route_info_search();
    let config = OpenApiConfig::new("Demo", "1.0.0");
    let spec = autumn_web::openapi::generate_spec(&config, &[&route.api_doc]);
    let op = spec.paths["/search"].get.as_ref().unwrap();
    let query_param = op
        .parameters
        .iter()
        .find(|p| p.location == "query")
        .expect("Query<T> handler must produce at least one query parameter");
    assert_eq!(query_param.name, "SearchParams");
    assert_eq!(query_param.location, "query");
    assert!(
        !query_param.required,
        "query params from structs are optional"
    );
    assert_eq!(
        query_param.style.as_deref(),
        Some("form"),
        "Query<T> must use style:form so fields serialize as individual keys"
    );
    assert_eq!(
        query_param.explode,
        Some(true),
        "Query<T> must use explode:true so ?q=foo&page=2 not ?SearchParams=..."
    );
}

// ── Security scheme detection ─────────────────────────────────────

#[test]
fn secured_route_has_secured_flag() {
    let route = __autumn_route_info_protected_handler();
    assert!(
        route.api_doc.secured,
        "routes decorated with #[secured] must have secured = true"
    );
}

#[test]
fn secured_route_with_role_has_required_roles() {
    let route = __autumn_route_info_admin_handler();
    assert!(route.api_doc.secured);
    assert_eq!(
        route.api_doc.required_roles,
        &["admin"],
        "#[secured(\"admin\")] must populate required_roles"
    );
}

#[test]
fn secured_route_above_route_attribute_has_secured_flag() {
    let route = __autumn_route_info_top_first_protected_handler();
    assert!(
        route.api_doc.secured,
        "#[secured] above the route attribute must still set secured = true"
    );
}

#[test]
fn secured_route_above_route_attribute_preserves_required_roles() {
    let route = __autumn_route_info_top_first_admin_handler();
    assert!(route.api_doc.secured);
    assert_eq!(
        route.api_doc.required_roles,
        &["admin", "editor"],
        "#[secured(...)] above the route attribute must preserve required_roles"
    );
}

#[test]
fn secured_operation_carries_security_requirement() {
    let route = __autumn_route_info_protected_handler();
    let config = OpenApiConfig::new("Demo", "1.0.0");
    let spec = autumn_web::openapi::generate_spec(&config, &[&route.api_doc]);
    let op = spec.paths["/protected"].get.as_ref().unwrap();
    assert!(
        !op.security.is_empty(),
        "secured operation must list at least one security requirement"
    );
    let req = &op.security[0];
    assert!(
        req.contains_key("SessionAuth"),
        "security requirement must reference session-cookie auth"
    );
    assert!(
        !req.contains_key("BearerAuth"),
        "secured routes use sessions, not bearer JWTs"
    );
}

#[test]
fn secured_spec_includes_session_cookie_auth_scheme() {
    let route = __autumn_route_info_protected_handler();
    let config = OpenApiConfig::new("Demo", "1.0.0");
    let spec = autumn_web::openapi::generate_spec(&config, &[&route.api_doc]);
    let comps = spec
        .components
        .as_ref()
        .expect("components must be present");
    assert!(
        comps.security_schemes.contains_key("SessionAuth"),
        "SessionAuth security scheme must be registered when any route is secured"
    );
    assert!(
        !comps.security_schemes.contains_key("BearerAuth"),
        "secured routes must not be documented as bearer JWT routes"
    );
    let scheme = &comps.security_schemes["SessionAuth"];
    assert_eq!(scheme["type"], "apiKey");
    assert_eq!(scheme["in"], "cookie");
    assert_eq!(scheme["name"], "autumn.sid");
}

#[test]
fn unsecured_spec_has_no_security_schemes() {
    let route = __autumn_route_info_hello();
    let config = OpenApiConfig::new("Demo", "1.0.0");
    let spec = autumn_web::openapi::generate_spec(&config, &[&route.api_doc]);
    if let Some(comps) = spec.components {
        assert!(
            comps.security_schemes.is_empty(),
            "unsecured routes must not emit any security schemes"
        );
    }
}

// ── Spec validation (all $ref backed by components) ───────────────

fn assert_all_refs_defined(
    value: &serde_json::Value,
    schemas: &std::collections::BTreeMap<String, serde_json::Value>,
) {
    if let Some(ref_str) = value.get("$ref").and_then(|v| v.as_str()) {
        let prefix = "#/components/schemas/";
        if let Some(name) = ref_str.strip_prefix(prefix) {
            assert!(
                schemas.contains_key(name),
                "$ref to '{name}' has no backing component schema"
            );
        }
    }
    if let Some(obj) = value.as_object() {
        for v in obj.values() {
            assert_all_refs_defined(v, schemas);
        }
    }
    if let Some(arr) = value.as_array() {
        for v in arr {
            assert_all_refs_defined(v, schemas);
        }
    }
}

#[test]
fn all_refs_in_spec_are_backed_by_component_schemas() {
    use autumn_web::Route;
    let routes: Vec<Route> = routes![
        hello,
        get_user,
        create_item,
        admin,
        list_widgets,
        post_widgets,
        create_validated_widget
    ];
    let docs: Vec<&ApiDoc> = routes.iter().map(|r| &r.api_doc).collect();
    let config = OpenApiConfig::new("Test API", "0.1.0");
    let spec = autumn_web::openapi::generate_spec(&config, &docs);

    let spec_json = serde_json::to_value(&spec).unwrap();
    let schemas = spec.components.map(|c| c.schemas).unwrap_or_default();
    assert_all_refs_defined(&spec_json, &schemas);
}

#[test]
fn generated_spec_reuses_problem_details_schema_for_errors() {
    let routes: Vec<Route> = routes![hello, protected_handler];
    let docs: Vec<&ApiDoc> = routes.iter().map(|r| &r.api_doc).collect();
    let config = OpenApiConfig::new("Test API", "0.1.0");
    let spec = autumn_web::openapi::generate_spec(&config, &docs);
    let components = spec.components.expect("components must be present");

    let problem = components
        .schemas
        .get("ProblemDetails")
        .expect("canonical ProblemDetails schema must be registered");
    assert_eq!(problem["required"][0], "type");
    assert!(
        problem["required"]
            .as_array()
            .unwrap()
            .contains(&"code".into())
    );
    assert!(
        problem["required"]
            .as_array()
            .unwrap()
            .contains(&"errors".into())
    );

    let op = spec.paths["/protected"].get.as_ref().unwrap();
    for status in [
        "400", "401", "403", "404", "413", "415", "422", "500", "503",
    ] {
        let response = op
            .responses
            .get(status)
            .unwrap_or_else(|| panic!("missing shared ProblemDetails response {status}"));
        let media = response
            .content
            .get("application/problem+json")
            .unwrap_or_else(|| panic!("{status} must use application/problem+json"));
        assert_eq!(
            media.schema["$ref"], "#/components/schemas/ProblemDetails",
            "{status} must reference the canonical schema"
        );
    }
}

// ── Collision-proof schema component identity (issue #1972, Part 2 / Item 2) ──

mod spec_create {
    use autumn_web::openapi::OpenApiSchema;
    #[derive(serde::Serialize, serde::Deserialize, OpenApiSchema)]
    pub struct Payload {
        pub create_only: String,
    }
}

mod spec_update {
    use autumn_web::openapi::OpenApiSchema;
    #[derive(serde::Serialize, serde::Deserialize, OpenApiSchema)]
    pub struct Payload {
        pub update_only: i64,
    }
}

#[post("/api/spec-create")]
async fn spec_create_route(Json(_p): Json<spec_create::Payload>) -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({}))
}

#[post("/api/spec-update")]
async fn spec_update_route(Json(_p): Json<spec_update::Payload>) -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({}))
}

#[test]
fn openapi_spec_disambiguates_same_named_component_schemas() {
    let create = __autumn_route_info_spec_create_route().api_doc;
    let update = __autumn_route_info_spec_update_route().api_doc;
    let config = OpenApiConfig::new("Demo", "1.0.0");
    let spec = autumn_web::openapi::generate_spec(&config, &[&create, &update]);

    // The two request-body `$ref`s must be distinct component keys.
    let create_ref = spec.paths["/api/spec-create"]
        .post
        .as_ref()
        .unwrap()
        .request_body
        .as_ref()
        .unwrap()
        .content["application/json"]
        .schema["$ref"]
        .as_str()
        .unwrap()
        .to_owned();
    let update_ref = spec.paths["/api/spec-update"]
        .post
        .as_ref()
        .unwrap()
        .request_body
        .as_ref()
        .unwrap()
        .content["application/json"]
        .schema["$ref"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(
        create_ref, update_ref,
        "distinct types must not share a $ref"
    );

    // Both refs must resolve to their own field-accurate component schema.
    let components = spec.components.expect("components");
    let create_key = create_ref.trim_start_matches("#/components/schemas/");
    let update_key = update_ref.trim_start_matches("#/components/schemas/");
    let create_schema = components
        .schemas
        .get(create_key)
        .expect("create component present");
    let update_schema = components
        .schemas
        .get(update_key)
        .expect("update component present");
    assert!(
        create_schema["properties"].get("create_only").is_some(),
        "{create_schema}"
    );
    assert!(
        create_schema["properties"].get("update_only").is_none(),
        "no shadow: {create_schema}"
    );
    assert!(
        update_schema["properties"].get("update_only").is_some(),
        "{update_schema}"
    );
    assert!(
        update_schema["properties"].get("create_only").is_none(),
        "no shadow: {update_schema}"
    );
}

// ── Nested derived-schema refs resolve through the collision index (issue #1972,
//    Part 2 / P1 follow-up) ──
//
// A `$ref` emitted *inside* a derived schema body (a nested named-struct field)
// must carry the field type's `type_name` identity and be rewritten to the same
// collision-resolved display key the top-level route refs use — so a wrapper
// whose field is a `create::Args` resolves correctly even when a sibling route
// references a same-last-segment `update::Args`.

mod wrap_create {
    use autumn_web::openapi::OpenApiSchema;
    #[derive(serde::Serialize, serde::Deserialize, OpenApiSchema)]
    pub struct Args {
        pub create_only: String,
    }
}

mod wrap_update {
    use autumn_web::openapi::OpenApiSchema;
    #[derive(serde::Serialize, serde::Deserialize, OpenApiSchema)]
    pub struct Args {
        pub update_only: i64,
    }
}

// A wrapper whose `payload` field is the CREATE Args — the nested-only type that
// no route references directly (Mode 1 back-fill + collision closure).
#[derive(serde::Serialize, serde::Deserialize, autumn_web::openapi::OpenApiSchema)]
struct WrapEnvelope {
    pub payload: wrap_create::Args,
    pub note: String,
}

#[post("/api/wrap-envelope")]
async fn wrap_envelope_route(Json(_e): Json<WrapEnvelope>) -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({}))
}

#[post("/api/wrap-update")]
async fn wrap_update_route(Json(_u): Json<wrap_update::Args>) -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({}))
}

fn request_body_ref(spec: &autumn_web::openapi::OpenApiSpec, path: &str) -> String {
    spec.paths[path]
        .post
        .as_ref()
        .unwrap()
        .request_body
        .as_ref()
        .unwrap()
        .content["application/json"]
        .schema["$ref"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[test]
fn nested_derived_ref_resolves_through_collision_index() {
    let envelope = __autumn_route_info_wrap_envelope_route().api_doc;
    let update = __autumn_route_info_wrap_update_route().api_doc;
    let config = OpenApiConfig::new("Demo", "1.0.0");
    let spec = autumn_web::openapi::generate_spec(&config, &[&envelope, &update]);

    let envelope_ref = request_body_ref(&spec, "/api/wrap-envelope");
    let update_ref = request_body_ref(&spec, "/api/wrap-update");
    let components = spec.components.expect("components");

    // The two `Args` collide (create is nested in the envelope, update is a route
    // ref), so both must qualify away from the bare `Args` key.
    assert_ne!(
        update_ref, "#/components/schemas/Args",
        "colliding update Args must qualify: {update_ref}"
    );

    // The envelope's nested `payload` ref must resolve to the CREATE Args — not
    // dangle, not carry a raw `type_name` identity, and not point at update.
    let envelope_key = envelope_ref.trim_start_matches("#/components/schemas/");
    let envelope_schema = components
        .schemas
        .get(envelope_key)
        .expect("envelope component present");
    let payload_ref = envelope_schema["properties"]["payload"]["$ref"]
        .as_str()
        .expect("envelope payload is a $ref");
    let payload_key = payload_ref.trim_start_matches("#/components/schemas/");
    assert!(
        !payload_key.contains("::"),
        "nested ref must be a display key, not a raw identity: {payload_ref}"
    );
    let payload_schema = components
        .schemas
        .get(payload_key)
        .expect("nested CREATE Args component present (not dangling)");
    assert!(
        payload_schema["properties"].get("create_only").is_some(),
        "nested ref must resolve to CREATE Args: {payload_schema}"
    );
    assert!(
        payload_schema["properties"].get("update_only").is_none(),
        "nested ref must NOT resolve to UPDATE Args: {payload_schema}"
    );

    // The update route's own component is the OTHER, distinct Args.
    let update_key = update_ref.trim_start_matches("#/components/schemas/");
    assert_ne!(
        update_key, payload_key,
        "create and update Args must be distinct components: {update_key} == {payload_key}"
    );
    let update_schema = components
        .schemas
        .get(update_key)
        .expect("update Args component present");
    assert!(
        update_schema["properties"].get("update_only").is_some(),
        "update component must expose its own field: {update_schema}"
    );
}

// ── No-collision nested ref: no churn + nested-only back-fill (Mode 1) ──

mod ncnest {
    use autumn_web::openapi::OpenApiSchema;
    #[derive(serde::Serialize, serde::Deserialize, OpenApiSchema)]
    pub struct NestedFoo {
        pub value: String,
    }
    #[derive(serde::Serialize, serde::Deserialize, OpenApiSchema)]
    pub struct OuterBar {
        pub foo: NestedFoo,
        pub label: String,
    }
}

#[post("/api/nc-outer")]
async fn nc_outer_route(Json(_o): Json<ncnest::OuterBar>) -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({}))
}

#[test]
fn non_colliding_nested_ref_stays_short_with_no_churn() {
    let outer = __autumn_route_info_nc_outer_route().api_doc;
    let config = OpenApiConfig::new("Demo", "1.0.0");
    let spec = autumn_web::openapi::generate_spec(&config, &[&outer]);

    let outer_ref = request_body_ref(&spec, "/api/nc-outer");
    assert_eq!(
        outer_ref, "#/components/schemas/OuterBar",
        "top-level ref stays short"
    );
    let components = spec.components.expect("components");
    let outer_schema = components
        .schemas
        .get("OuterBar")
        .expect("outer component present");
    let nested_ref = outer_schema["properties"]["foo"]["$ref"]
        .as_str()
        .expect("nested foo is a $ref");
    // No churn: a non-colliding nested ref stays the short last-segment key.
    assert_eq!(
        nested_ref, "#/components/schemas/NestedFoo",
        "non-colliding nested ref must stay the short key (no churn): {nested_ref}"
    );
    // Mode 1: the nested-only type (no direct route ref) is back-filled with its
    // real field-accurate schema.
    let nested_schema = components
        .schemas
        .get("NestedFoo")
        .expect("nested-only component back-filled");
    assert!(
        nested_schema["properties"].get("value").is_some(),
        "nested-only component carries its real fields: {nested_schema}"
    );
}
