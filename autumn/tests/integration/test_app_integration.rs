//! Integration tests demonstrating Autumn's first-party test infrastructure.
//!
//! These tests showcase the `TestApp`, `TestClient`, and `TestResponse` APIs
//! that bring Autumn's testing story to parity with Spring Boot / Django.

use autumn_web::prelude::*;
use autumn_web::test::TestApp;
use axum::http::StatusCode;

// ── Sample handlers (simulate a real application) ──────────────

#[get("/")]
async fn index() -> &'static str {
    "Welcome to Autumn"
}

#[get("/api/health")]
async fn health_check() -> Json<serde_json::Value> {
    Json(serde_json::json!({"status": "UP"}))
}

#[post("/echo")]
async fn echo(Json(body): Json<serde_json::Value>) -> Json<serde_json::Value> {
    Json(body)
}

#[post("/users")]
async fn create_user(Json(body): Json<serde_json::Value>) -> (StatusCode, Json<serde_json::Value>) {
    let name = body["name"].as_str().unwrap_or("anonymous");
    (
        StatusCode::CREATED,
        Json(serde_json::json!({"id": 1, "name": name})),
    )
}

#[get("/users/{id}")]
async fn get_user(axum::extract::Path(id): axum::extract::Path<i64>) -> Json<serde_json::Value> {
    Json(serde_json::json!({"id": id, "name": "Alice"}))
}

#[put("/users/{id}")]
async fn update_user(
    axum::extract::Path(id): axum::extract::Path<i64>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({"id": id, "name": body["name"]}))
}

#[delete("/users/{id}")]
async fn delete_user(axum::extract::Path(_id): axum::extract::Path<i64>) -> StatusCode {
    StatusCode::NO_CONTENT
}

#[get("/primitive/int")]
#[allow(clippy::unused_async)]
async fn primitive_int() -> i32 {
    42
}

#[get("/primitive/bool")]
#[allow(clippy::unused_async)]
async fn primitive_bool() -> bool {
    true
}

#[post("/validate")]
async fn validate_input(
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, AutumnError> {
    let name = body["name"]
        .as_str()
        .ok_or_else(|| AutumnError::bad_request_msg("name is required"))?;
    if name.is_empty() {
        return Err(AutumnError::bad_request_msg("name must not be empty"));
    }
    Ok(Json(serde_json::json!({"valid": true, "name": name})))
}

// ── Helper to build a client with all routes ───────────────────

fn app() -> autumn_web::test::TestClient {
    TestApp::new()
        .routes(routes![
            index,
            health_check,
            echo,
            create_user,
            get_user,
            update_user,
            delete_user,
            validate_input,
            primitive_int,
            primitive_bool
        ])
        .build()
}

// ── Tests ──────────────────────────────────────────────────────

#[tokio::test]
async fn get_index_returns_200() {
    let client = app();
    client
        .get("/")
        .send()
        .await
        .assert_ok()
        .assert_body_contains("Welcome");
}

#[tokio::test]
async fn health_endpoint_returns_json() {
    let client = app();
    client
        .get("/api/health")
        .send()
        .await
        .assert_ok()
        .assert_json::<serde_json::Value, _>(|v| {
            assert_eq!(v["status"], "UP");
        });
}

#[tokio::test]
async fn post_echo_returns_same_body() {
    let client = app();
    let payload = serde_json::json!({"message": "hello", "count": 42});

    client
        .post("/echo")
        .json(&payload)
        .send()
        .await
        .assert_ok()
        .assert_body_contains("hello")
        .assert_body_contains("42");
}

#[tokio::test]
async fn create_user_returns_201() {
    let client = app();

    client
        .post("/users")
        .json(&serde_json::json!({"name": "Bob"}))
        .send()
        .await
        .assert_status(201)
        .assert_json::<serde_json::Value, _>(|user| {
            assert_eq!(user["name"], "Bob");
            assert_eq!(user["id"], 1);
        });
}

#[tokio::test]
async fn get_user_by_id_test() {
    let client = app();

    client
        .get("/users/42")
        .send()
        .await
        .assert_ok()
        .assert_json::<serde_json::Value, _>(|user| {
            assert_eq!(user["id"], 42);
            assert_eq!(user["name"], "Alice");
        });
}

#[tokio::test]
async fn update_user_by_id() {
    let client = app();

    client
        .put("/users/1")
        .json(&serde_json::json!({"name": "Updated"}))
        .send()
        .await
        .assert_ok()
        .assert_json::<serde_json::Value, _>(|user| {
            assert_eq!(user["id"], 1);
            assert_eq!(user["name"], "Updated");
        });
}

#[tokio::test]
async fn delete_user_returns_204() {
    let client = app();

    client
        .delete("/users/1")
        .send()
        .await
        .assert_status(204)
        .assert_body_empty();
}

#[tokio::test]
async fn not_found_returns_404() {
    let client = app();
    client.get("/nonexistent").send().await.assert_status(404);
}

#[tokio::test]
async fn validation_error_returns_400() {
    let client = app();

    client
        .post("/validate")
        .json(&serde_json::json!({"wrong_field": "value"}))
        .send()
        .await
        .assert_status(400);
}

#[tokio::test]
async fn validation_success() {
    let client = app();

    client
        .post("/validate")
        .json(&serde_json::json!({"name": "Alice"}))
        .send()
        .await
        .assert_ok()
        .assert_json::<serde_json::Value, _>(|v| {
            assert_eq!(v["valid"], true);
            assert_eq!(v["name"], "Alice");
        });
}

#[tokio::test]
async fn primitive_integer_return_is_plain_text() {
    let client = app();

    client
        .get("/primitive/int")
        .send()
        .await
        .assert_ok()
        .assert_body_eq("42");
}

#[tokio::test]
async fn primitive_bool_return_is_plain_text() {
    let client = app();

    client
        .get("/primitive/bool")
        .send()
        .await
        .assert_ok()
        .assert_body_eq("true");
}

// ── Full CRUD lifecycle test ───────────────────────────────────

#[tokio::test]
async fn full_crud_lifecycle() {
    let client = app();

    // Create
    let resp = client
        .post("/users")
        .json(&serde_json::json!({"name": "Charlie"}))
        .send()
        .await;
    resp.assert_status(201);
    let user: serde_json::Value = resp.json();
    let id = user["id"].as_i64().unwrap();

    // Read
    client
        .get(&format!("/users/{id}"))
        .send()
        .await
        .assert_ok()
        .assert_json::<serde_json::Value, _>(|u| {
            assert_eq!(u["id"], id);
        });

    // Update
    client
        .put(&format!("/users/{id}"))
        .json(&serde_json::json!({"name": "Charlie Updated"}))
        .send()
        .await
        .assert_ok()
        .assert_json::<serde_json::Value, _>(|u| {
            assert_eq!(u["name"], "Charlie Updated");
        });

    // Delete
    client
        .delete(&format!("/users/{id}"))
        .send()
        .await
        .assert_status(204);
}

// ── Custom config test ─────────────────────────────────────────

#[tokio::test]
async fn custom_profile_configuration() {
    let client = TestApp::new()
        .profile("staging")
        .routes(routes![index])
        .build();

    client.get("/").send().await.assert_ok();
}

// ── Header assertions ──────────────────────────────────────────

#[tokio::test]
async fn response_has_request_id_header() {
    let client = app();

    let resp = client.get("/").send().await;
    resp.assert_ok();
    // Autumn adds x-request-id via RequestIdLayer
    assert!(
        resp.header("x-request-id").is_some(),
        "expected x-request-id header from RequestIdLayer"
    );
}

#[tokio::test]
async fn json_response_has_content_type() {
    let client = app();

    client
        .post("/echo")
        .json(&serde_json::json!({"test": true}))
        .send()
        .await
        .assert_ok()
        .assert_header_contains("content-type", "application/json");
}

// ── Form submission test ───────────────────────────────────────

#[post("/form-echo")]
async fn form_echo(
    Form(data): Form<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    Json(serde_json::json!(data))
}

#[tokio::test]
async fn form_submission() {
    let client = TestApp::new().routes(routes![form_echo]).build();

    client
        .post("/form-echo")
        .form("name=Alice&age=30")
        .send()
        .await
        .assert_ok()
        .assert_body_contains("Alice")
        .assert_body_contains("30");
}

#[derive(serde::Serialize, serde::Deserialize, validator::Validate)]
struct SentinelInput {
    #[validate(length(min = 1, max = 5, message = "Custom validation failed"))]
    field1: String,
    #[validate(length(min = 10, message = "Another validation failed"))]
    field2: String,
}

#[autumn_web::post("/test-validation")]
async fn test_route_sentinel(
    autumn_web::Valid(autumn_web::prelude::Json(_payload)): autumn_web::Valid<
        autumn_web::prelude::Json<SentinelInput>,
    >,
) -> &'static str {
    "ok"
}

#[tokio::test]
async fn validation_error_structured_details() {
    let client = autumn_web::test::TestApp::new()
        .routes(autumn_web::routes![test_route_sentinel])
        .build();

    let payload = serde_json::json!({
        "field1": "too long string",
        "field2": "short"
    });

    let response = client.post("/test-validation").json(&payload).send().await;

    response.assert_status(422);

    let json: serde_json::Value = response.json::<serde_json::Value>();

    assert_eq!(json["status"], 422);
    assert_eq!(json["code"], "autumn.validation_failed");

    let errors = json["errors"]
        .as_array()
        .expect("errors should be an array of field-level validation failures");
    assert!(
        !errors.is_empty(),
        "errors should preserve field-level validation failures"
    );

    let field1 = errors
        .iter()
        .find(|error| error["field"] == "field1")
        .expect("field1 should have an error entry");
    let field1_errors = field1["messages"]
        .as_array()
        .expect("field1 should have an array of messages");
    assert!(!field1_errors.is_empty(), "field1 should have errors");
    assert_eq!(
        field1_errors[0].as_str().unwrap(),
        "Custom validation failed"
    );

    let field2 = errors
        .iter()
        .find(|error| error["field"] == "field2")
        .expect("field2 should have an error entry");
    let field2_errors = field2["messages"]
        .as_array()
        .expect("field2 should have an array of messages");
    assert!(!field2_errors.is_empty(), "field2 should have errors");
    assert_eq!(
        field2_errors[0].as_str().unwrap(),
        "Another validation failed"
    );
}
