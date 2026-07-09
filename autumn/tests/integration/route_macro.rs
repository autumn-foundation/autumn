//!
//! Integration tests for the single route macro.
//!
use autumn_web::extract::Path;
use autumn_web::{delete, get, post, put};

// ── GET handlers ─────────────────────────────────────────────────────

#[get("/hello")]
async fn hello() -> &'static str {
    "Hello, Autumn!"
}

#[get("/")]
async fn index() -> &'static str {
    "root"
}

#[get("/with/nested/path")]
async fn nested() -> &'static str {
    "nested"
}

// ── POST / PUT / DELETE handlers (S-003) ─────────────────────────────

#[post("/create")]
async fn create_item() -> &'static str {
    "created"
}

#[put("/update")]
async fn update_item() -> &'static str {
    "updated"
}

#[delete("/remove")]
async fn remove_item() -> &'static str {
    "removed"
}

// ── Handlers with various return types (S-004: debug_handler) ────────

#[get("/string")]
async fn returns_string() -> String {
    "hello".to_owned()
}

#[post("/json")]
async fn returns_json() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({"ok": true}))
}

#[get("/status")]
async fn returns_status() -> http::StatusCode {
    http::StatusCode::NO_CONTENT
}

#[get("/primitive-int")]
#[allow(clippy::unused_async)]
async fn returns_primitive_int() -> i32 {
    42
}

#[get("/primitive-bool")]
#[allow(clippy::unused_async)]
async fn returns_primitive_bool() -> bool {
    true
}

// ── GET tests ────────────────────────────────────────────────────────

#[test]
fn hello_route_info_has_correct_method() {
    let route = __autumn_route_info_hello();
    assert_eq!(route.method, http::Method::GET);
}

#[test]
fn hello_route_info_has_correct_path() {
    let route = __autumn_route_info_hello();
    assert_eq!(route.path, "/hello");
}

#[test]
fn hello_route_info_has_correct_name() {
    let route = __autumn_route_info_hello();
    assert_eq!(route.name, "hello");
}

#[test]
fn index_route_info_has_correct_fields() {
    let route = __autumn_route_info_index();
    assert_eq!(route.method, http::Method::GET);
    assert_eq!(route.path, "/");
    assert_eq!(route.name, "index");
}

#[test]
fn nested_route_info_has_correct_path() {
    let route = __autumn_route_info_nested();
    assert_eq!(route.path, "/with/nested/path");
}

// ── POST / PUT / DELETE tests (S-003) ────────────────────────────────

#[test]
fn post_route_info_has_correct_method() {
    let route = __autumn_route_info_create_item();
    assert_eq!(route.method, http::Method::POST);
}

#[test]
fn post_route_info_has_correct_path() {
    let route = __autumn_route_info_create_item();
    assert_eq!(route.path, "/create");
}

#[test]
fn post_route_info_has_correct_name() {
    let route = __autumn_route_info_create_item();
    assert_eq!(route.name, "create_item");
}

#[test]
fn put_route_info_has_correct_method() {
    let route = __autumn_route_info_update_item();
    assert_eq!(route.method, http::Method::PUT);
}

#[test]
fn put_route_info_has_correct_path() {
    let route = __autumn_route_info_update_item();
    assert_eq!(route.path, "/update");
}

#[test]
fn put_route_info_has_correct_name() {
    let route = __autumn_route_info_update_item();
    assert_eq!(route.name, "update_item");
}

#[test]
fn delete_route_info_has_correct_method() {
    let route = __autumn_route_info_remove_item();
    assert_eq!(route.method, http::Method::DELETE);
}

#[test]
fn delete_route_info_has_correct_path() {
    let route = __autumn_route_info_remove_item();
    assert_eq!(route.path, "/remove");
}

#[test]
fn delete_route_info_has_correct_name() {
    let route = __autumn_route_info_remove_item();
    assert_eq!(route.name, "remove_item");
}

// ── Path parameter extraction (S-006) ───────────────────────────────

#[get("/users/{id}")]
async fn get_user(_id: Path<i32>) -> &'static str {
    "user"
}

#[get("/posts/{year}/{slug}")]
async fn get_post(_params: Path<(i32, String)>) -> &'static str {
    "post"
}

// ── debug_handler return-type coverage (S-004) ──────────────────────
//
// These tests prove that handlers with various return types compile
// successfully under debug_handler (debug builds) and without it
// (release builds). If debug_handler rejects a return type, the test
// file itself will fail to compile.

#[test]
fn string_return_type_compiles_with_debug_handler() {
    let route = __autumn_route_info_returns_string();
    assert_eq!(route.method, http::Method::GET);
    assert_eq!(route.path, "/string");
    assert_eq!(route.name, "returns_string");
}

#[test]
fn json_return_type_compiles_with_debug_handler() {
    let route = __autumn_route_info_returns_json();
    assert_eq!(route.method, http::Method::POST);
    assert_eq!(route.path, "/json");
    assert_eq!(route.name, "returns_json");
}

#[test]
fn status_code_return_type_compiles_with_debug_handler() {
    let route = __autumn_route_info_returns_status();
    assert_eq!(route.method, http::Method::GET);
    assert_eq!(route.path, "/status");
    assert_eq!(route.name, "returns_status");
}

#[test]
fn primitive_int_return_type_compiles_with_route_macro() {
    let route = __autumn_route_info_returns_primitive_int();
    assert_eq!(route.method, http::Method::GET);
    assert_eq!(route.path, "/primitive-int");
    assert_eq!(route.name, "returns_primitive_int");
}

#[test]
fn primitive_bool_return_type_compiles_with_route_macro() {
    let route = __autumn_route_info_returns_primitive_bool();
    assert_eq!(route.method, http::Method::GET);
    assert_eq!(route.path, "/primitive-bool");
    assert_eq!(route.name, "returns_primitive_bool");
}

#[tokio::test]
async fn primitive_int_handler_keeps_declared_return_type_for_direct_calls() {
    let value: i32 = returns_primitive_int().await;
    assert_eq!(value, 42);
}

#[tokio::test]
async fn primitive_bool_handler_keeps_declared_return_type_for_direct_calls() {
    let value: bool = returns_primitive_bool().await;
    assert!(value);
}

// ── Path parameter tests (S-006) ────────────────────────────────────

#[test]
fn path_param_route_preserves_pattern() {
    let route = __autumn_route_info_get_user();
    assert_eq!(route.path, "/users/{id}");
    assert_eq!(route.method, http::Method::GET);
}

#[test]
fn path_param_route_has_correct_name() {
    let route = __autumn_route_info_get_user();
    assert_eq!(route.name, "get_user");
}

#[test]
fn multi_path_params_route_preserves_pattern() {
    let route = __autumn_route_info_get_post();
    assert_eq!(route.path, "/posts/{year}/{slug}");
    assert_eq!(route.method, http::Method::GET);
}
