//! Integration tests for `OpenAPI` auto-generation (S-056).
//!
//! Covers:
//! * Route macros emit the expected `ApiDoc` metadata.
//! * `#[api_doc(...)]` overrides flow through to the generated spec.
//! * `AppBuilder::openapi(...)` mounts `/openapi.json` and `/swagger-ui`.

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

// `Option` and `Vec` are matched on the LAST PATH SEGMENT here too, so an
// application's own generic of that name reaches the `Nullable` / `Array` arms
// of the route macro's schema-entry builder. Neither is nullable nor an array.
//
// Each impostor gets its own module so it shadows exactly one prelude name:
// declaring both in one module would silently re-point every bare `Option` /
// `Vec` in it.
mod impostor_opt {
    /// Last segment `Option`, but an ordinary struct: an object, never `null`.
    #[derive(serde::Serialize, serde::Deserialize)]
    pub struct Option<T> {
        pub held: T,
    }
}

mod impostor_vec {
    /// Last segment `Vec`, but an ordinary struct: an object, not an array.
    #[derive(serde::Serialize, serde::Deserialize)]
    pub struct Vec<T> {
        pub head: T,
    }
}

#[get("/impostor-option")]
async fn get_impostor_option() -> axum::Json<impostor_opt::Option<Widget>> {
    axum::Json(impostor_opt::Option {
        held: Widget { id: 0 },
    })
}

#[get("/impostor-vec")]
async fn get_impostor_vec() -> axum::Json<impostor_vec::Vec<Widget>> {
    axum::Json(impostor_vec::Vec {
        head: Widget { id: 0 },
    })
}

// A handler that genuinely deals in arbitrary JSON. Nothing registers a schema
// for `serde_json::Value`, so a named `$ref` to it back-fills into the opaque
// `{"type":"object"}` placeholder — which misdescribes every array, scalar and
// null the handler legitimately returns, and fails `--strict` for a handler that
// is behaving correctly.
#[get("/arbitrary")]
async fn get_arbitrary_json() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!([1, 2, 3]))
}

#[get("/maybe-arbitrary")]
async fn get_optional_arbitrary_json() -> axum::Json<Option<serde_json::Value>> {
    axum::Json(None)
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

// ── Response schema survives a body guard above the route attribute (#1677) ──
//
// `#[secured]`, `#[step_up]`, and `#[throttle]` all rewrite the handler's
// return type to `Response` when they expand. Written above the route
// attribute, each expands first — before the route macro ever sees the
// handler — so the route macro must recover the original `Json<T>` return
// type from the guard's generated body instead of losing the response
// schema.

#[secured]
#[get("/secured-top-first-response")]
async fn secured_top_first_response() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({}))
}

#[step_up]
#[get("/step-up-top-first-response")]
async fn step_up_top_first_response() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({}))
}

#[throttle(limit = 5, per = "1m", key = "ip")]
#[post("/throttle-top-first-response")]
async fn throttle_top_first_response() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({}))
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

/// An application `Option<T>` is an ordinary named type. Rendering the route's
/// response as `oneOf [<inner>, null]` advertised both a null the handler never
/// emits and the WRONG payload — the inner type, which this wrapper does not
/// wrap. The wrapper entry carries its own `type_name`, so the generator can
/// tell it from `std`'s `Option` and emit an honest `$ref` instead.
#[test]
fn an_impostor_option_response_is_a_ref_not_a_nullable() {
    let route = __autumn_route_info_get_impostor_option();
    let config = OpenApiConfig::new("Demo", "1.0.0");
    let spec = autumn_web::openapi::generate_spec(&config, &[&route.api_doc]);
    let schema = &spec.paths["/impostor-option"]
        .get
        .as_ref()
        .unwrap()
        .responses["200"]
        .content["application/json"]
        .schema;

    assert!(
        schema.get("oneOf").is_none(),
        "an application `Option` must not be advertised as nullable: {schema}"
    );
    assert!(
        schema["$ref"]
            .as_str()
            .is_some_and(|r| r.contains("Option")),
        "it is an ordinary named component: {schema}"
    );
}

/// The same collision one level over: an application `Vec<T>` is an object, so
/// `type: array` + `items` described a shape the handler never serializes.
#[test]
fn an_impostor_vec_response_is_a_ref_not_an_array() {
    let route = __autumn_route_info_get_impostor_vec();
    let config = OpenApiConfig::new("Demo", "1.0.0");
    let spec = autumn_web::openapi::generate_spec(&config, &[&route.api_doc]);
    let schema = &spec.paths["/impostor-vec"]
        .get
        .as_ref()
        .unwrap()
        .responses["200"]
        .content["application/json"]
        .schema;

    assert_ne!(
        schema["type"], "array",
        "an application `Vec` must not be advertised as an array: {schema}"
    );
    assert!(
        schema.get("items").is_none(),
        "and it must carry no `items`: {schema}"
    );
    assert!(
        schema["$ref"].as_str().is_some_and(|r| r.contains("Vec")),
        "it is an ordinary named component: {schema}"
    );
}

/// `Json<serde_json::Value>` is unconstrained, not an object, and earns no
/// component at all — registering one is exactly how it became a placeholder.
#[test]
fn route_level_serde_json_value_is_unconstrained() {
    let route = __autumn_route_info_get_arbitrary_json();
    let config = OpenApiConfig::new("Demo", "1.0.0");
    let spec = autumn_web::openapi::generate_spec(&config, &[&route.api_doc]);
    let schema =
        &spec.paths["/arbitrary"].get.as_ref().unwrap().responses["200"].content["application/json"]
            .schema;

    assert!(
        schema.get("$ref").is_none(),
        "arbitrary JSON must be described inline, not referred to: {schema}"
    );
    assert!(
        schema.get("type").is_none(),
        "and it must not be constrained to any one type: {schema}"
    );
    assert!(
        spec.components
            .as_ref()
            .is_none_or(|c| !c.schemas.contains_key("Value")),
        "no component may be registered for it, or the back-fill turns it into \
         the opaque placeholder that `--strict` reports"
    );
    assert!(
        autumn_web::openapi::opaque_component_schemas(&spec).is_empty(),
        "so a spec whose only untyped thing is genuine arbitrary JSON is clean"
    );
}

/// The optional form must NOT gain a null branch: `oneOf` demands that exactly
/// one branch match, and the unconstrained schema already admits null — so
/// `oneOf [{unconstrained}, {"type":"null"}]` would reject the very null it is
/// meant to permit.
#[test]
fn route_level_optional_serde_json_value_is_not_wrapped() {
    let route = __autumn_route_info_get_optional_arbitrary_json();
    let config = OpenApiConfig::new("Demo", "1.0.0");
    let spec = autumn_web::openapi::generate_spec(&config, &[&route.api_doc]);
    let schema = &spec.paths["/maybe-arbitrary"]
        .get
        .as_ref()
        .unwrap()
        .responses["200"]
        .content["application/json"]
        .schema;

    assert!(
        schema.get("oneOf").is_none(),
        "wrapping unconstrained JSON in a nullable oneOf rejects its own null: {schema}"
    );
    assert!(schema.get("$ref").is_none(), "{schema}");
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

#[test]
fn secured_above_route_attribute_preserves_response_schema() {
    let route = __autumn_route_info_secured_top_first_response();
    let resp = route.api_doc.response.as_ref().expect(
        "a Json<...> return type must still be inferred when #[secured] expands before \
         the route macro",
    );
    assert_eq!(resp.name, "Value");
}

#[test]
fn step_up_above_route_attribute_preserves_response_schema() {
    let route = __autumn_route_info_step_up_top_first_response();
    let resp = route.api_doc.response.as_ref().expect(
        "a Json<...> return type must still be inferred when #[step_up] expands before \
         the route macro",
    );
    assert_eq!(resp.name, "Value");
}

#[test]
fn throttle_above_route_attribute_preserves_response_schema() {
    let route = __autumn_route_info_throttle_top_first_response();
    let resp = route.api_doc.response.as_ref().expect(
        "a Json<...> return type must still be inferred when #[throttle] expands before \
         the route macro",
    );
    assert_eq!(resp.name, "Value");
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

// ── Per-field query parameters for a nested `Query<T>` (issue #2251) ──
//
// `Query<T>` used to document ONE struct-level parameter with
// `style: form, explode: true` — exact for a scalar or scalar-array field, but
// undefined for a nested one (neither RFC 6570 nor OAS 3.x say what `form`
// means for a composite value). A `Query<T>` whose fields are introspectable
// (an `OpenApiSchema` back-fill match) now documents one parameter PER FIELD,
// so each field gets the `style` that actually round-trips it through
// `crate::query_string`'s bracketed decoder.

mod query_shapes {
    use autumn_web::openapi::OpenApiSchema;

    #[derive(serde::Deserialize, OpenApiSchema)]
    #[allow(dead_code)]
    pub struct Filter {
        pub status: String,
    }

    #[derive(serde::Deserialize, OpenApiSchema)]
    #[allow(dead_code)]
    pub struct Item {
        pub sku: String,
    }

    #[derive(serde::Deserialize, OpenApiSchema)]
    #[allow(dead_code)]
    pub struct SearchQuery {
        pub q: Option<String>,
        pub tags: Option<Vec<String>>,
        pub filter: Option<Filter>,
        pub items: Option<Vec<Item>>,
    }
}

#[get("/api/nested-search")]
async fn nested_search_route(_q: Query<query_shapes::SearchQuery>) -> &'static str {
    "ok"
}

mod query_required_shapes {
    use autumn_web::openapi::OpenApiSchema;

    #[derive(serde::Deserialize, OpenApiSchema)]
    #[allow(dead_code)]
    pub struct RequiredFilter {
        pub status: String,
    }

    #[derive(serde::Deserialize, OpenApiSchema)]
    #[allow(dead_code)]
    pub struct RequiredSearchQuery {
        pub filter: RequiredFilter,
    }
}

#[get("/api/required-search")]
async fn required_search_route(
    _q: Query<query_required_shapes::RequiredSearchQuery>,
) -> &'static str {
    "ok"
}

mod unregistered_ref_shapes {
    use autumn_web::openapi::OpenApiSchema;

    // Deliberately does NOT derive `OpenApiSchema` — a plain enum that
    // serializes as a string (`?dir=asc`), same as any type a caller never
    // opted into field-accurate schemas for.
    #[derive(serde::Deserialize)]
    #[allow(dead_code)]
    pub enum Sort {
        Asc,
        Desc,
    }

    #[derive(serde::Deserialize, OpenApiSchema)]
    #[allow(dead_code)]
    pub struct UnregisteredRefQuery {
        pub dir: Sort,
    }
}

#[get("/api/unregistered-ref-search")]
async fn unregistered_ref_search_route(
    _q: Query<unregistered_ref_shapes::UnregisteredRefQuery>,
) -> &'static str {
    "ok"
}

fn nested_search_spec() -> autumn_web::openapi::OpenApiSpec {
    let route = __autumn_route_info_nested_search_route();
    let config = OpenApiConfig::new("Demo", "1.0.0");
    autumn_web::openapi::generate_spec(&config, &[&route.api_doc])
}

fn nested_search_param<'a>(
    spec: &'a autumn_web::openapi::OpenApiSpec,
    name: &str,
) -> &'a autumn_web::openapi::Parameter {
    spec.paths["/api/nested-search"]
        .get
        .as_ref()
        .expect("GET /api/nested-search")
        .parameters
        .iter()
        .find(|p| p.location == "query" && p.name == name)
        .unwrap_or_else(|| panic!("expected a query parameter named {name}"))
}

/// Unwrap a nullable `{"oneOf": [<real>, {"type": "null"}]}` wrapper.
fn unwrap_nullable(schema: &serde_json::Value) -> &serde_json::Value {
    schema.get("oneOf").map_or(schema, |branches| &branches[0])
}

#[test]
fn one_query_parameter_per_field() {
    let spec = nested_search_spec();
    let params: Vec<&str> = spec.paths["/api/nested-search"]
        .get
        .as_ref()
        .unwrap()
        .parameters
        .iter()
        .filter(|p| p.location == "query")
        .map(|p| p.name.as_str())
        .collect();
    assert_eq!(
        params,
        ["filter", "items", "q", "tags"],
        "one parameter per struct field, not one for the whole struct"
    );
}

#[test]
fn scalar_query_field_keeps_form_explode() {
    let spec = nested_search_spec();
    let q = nested_search_param(&spec, "q");
    assert_eq!(q.style.as_deref(), Some("form"));
    assert_eq!(q.explode, Some(true));
    assert!(!q.required, "an Option<T> field is not required");
}

#[test]
fn scalar_array_query_field_keeps_form_explode() {
    let spec = nested_search_spec();
    let tags = nested_search_param(&spec, "tags");
    assert_eq!(
        tags.style.as_deref(),
        Some("form"),
        "a scalar-array field still round-trips via ?tags=a&tags=b"
    );
    assert_eq!(tags.explode, Some(true));
}

#[test]
fn nested_object_query_field_uses_deep_object() {
    let spec = nested_search_spec();
    let filter = nested_search_param(&spec, "filter");
    assert_eq!(
        filter.style.as_deref(),
        Some("deepObject"),
        "an object field decodes from ?filter[status]=open, which deepObject describes"
    );
    assert_eq!(filter.explode, Some(true));
    let inner = unwrap_nullable(&filter.schema);
    let reference = inner["$ref"]
        .as_str()
        .expect("a nested object field schema is a $ref");
    assert!(
        !reference.contains("::"),
        "the $ref must be the collision-resolved display key, not a raw type_name: {reference}"
    );
    let key = reference.trim_start_matches("#/components/schemas/");
    let components = spec
        .components
        .as_ref()
        .expect("components must be present");
    let resolved = components
        .schemas
        .get(key)
        .unwrap_or_else(|| panic!("$ref {reference} must resolve to a real component"));
    assert!(
        resolved["properties"].get("status").is_some(),
        "the resolved component must carry Filter's real fields: {resolved}"
    );
}

#[test]
fn required_nested_object_query_field_is_required() {
    // `filter` on `RequiredSearchQuery` is NOT `Option`-wrapped, so it must be
    // `required: true` on its own parameter — the old whole-struct fallback
    // could never say this (issue #2251).
    let route = __autumn_route_info_required_search_route();
    let config = OpenApiConfig::new("Demo", "1.0.0");
    let spec = autumn_web::openapi::generate_spec(&config, &[&route.api_doc]);
    let op = spec.paths["/api/required-search"].get.as_ref().unwrap();
    let filter = op
        .parameters
        .iter()
        .find(|p| p.location == "query" && p.name == "filter")
        .expect("a query parameter named filter");
    assert!(
        filter.required,
        "a non-Option nested field must be required: true"
    );
    assert_eq!(filter.style.as_deref(), Some("deepObject"));
}

#[test]
fn unregistered_ref_field_keeps_form_explode_not_deep_object() {
    // `Sort` derives no `OpenApiSchema`, so its own shape can't be read. A
    // plain enum serializes as a string (?dir=asc), so defaulting an
    // unresolvable $ref to "flat" must win over guessing "object" — the old
    // whole-struct fallback got this right by luck (everything was form), and
    // the per-field split must not regress it (issue #2251).
    let route = __autumn_route_info_unregistered_ref_search_route();
    let config = OpenApiConfig::new("Demo", "1.0.0");
    let spec = autumn_web::openapi::generate_spec(&config, &[&route.api_doc]);
    let op = spec.paths["/api/unregistered-ref-search"]
        .get
        .as_ref()
        .unwrap();
    let dir = op
        .parameters
        .iter()
        .find(|p| p.location == "query" && p.name == "dir")
        .expect("a query parameter named dir");
    assert_eq!(
        dir.style.as_deref(),
        Some("form"),
        "an unregistered $ref must default to flat, not deepObject: {dir:?}"
    );
    assert_eq!(dir.explode, Some(true));
}

#[test]
fn array_of_objects_query_field_documents_the_gap_instead_of_a_style() {
    let spec = nested_search_spec();
    let items = nested_search_param(&spec, "items");
    assert!(
        items.style.is_none(),
        "no OpenAPI style expresses an array of objects (issue #2251)"
    );
    assert!(items.explode.is_none());
    let description = items
        .description
        .as_deref()
        .expect("the gap must be documented on the parameter, not left silent");
    assert!(
        description.contains("[0]"),
        "must name the bracketed encoding a client needs: {description}"
    );
}

#[test]
fn undescribable_query_struct_keeps_the_old_single_parameter() {
    // `SearchParams` (defined above) derives no `OpenApiSchema`, so its fields
    // cannot be introspected — the old whole-struct fallback must still apply
    // (no spec churn for the common undecorated case).
    let route = __autumn_route_info_search();
    let config = OpenApiConfig::new("Demo", "1.0.0");
    let spec = autumn_web::openapi::generate_spec(&config, &[&route.api_doc]);
    let op = spec.paths["/search"].get.as_ref().unwrap();
    let query_params: Vec<_> = op
        .parameters
        .iter()
        .filter(|p| p.location == "query")
        .collect();
    assert_eq!(
        query_params.len(),
        1,
        "an undescribable query struct still documents one struct-level parameter"
    );
    assert_eq!(query_params[0].name, "SearchParams");
}
