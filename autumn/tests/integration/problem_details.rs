//! Problem Details contract tests for framework-generated JSON errors.

use autumn_web::config::AutumnConfig;
use autumn_web::prelude::*;
use autumn_web::security::{CsrfConfig, SecurityConfig};
use autumn_web::test::{TestApp, TestResponse};
use serde::Deserialize;
use validator::Validate;

#[derive(Deserialize)]
struct Payload {
    value: i32,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct Search {
    page: i32,
}

#[derive(Deserialize, Validate)]
struct NewAccount {
    #[validate(email)]
    email: String,
}

#[get("/bad-request")]
async fn bad_request() -> AutumnResult<&'static str> {
    Err(AutumnError::bad_request_msg("invalid input"))
}

#[get("/unauthorized")]
async fn unauthorized() -> AutumnResult<&'static str> {
    Err(AutumnError::unauthorized_msg("login required"))
}

#[get("/forbidden")]
async fn forbidden() -> AutumnResult<&'static str> {
    Err(AutumnError::forbidden_msg("not allowed"))
}

#[get("/conflict")]
async fn conflict() -> AutumnResult<&'static str> {
    Err(AutumnError::conflict_msg("version mismatch"))
}

#[get("/service-unavailable")]
async fn service_unavailable() -> AutumnResult<&'static str> {
    Err(AutumnError::service_unavailable_msg(
        "Database not configured",
    ))
}

#[get("/boom")]
async fn boom() -> AutumnResult<&'static str> {
    Err(AutumnError::internal_server_error_msg(
        "database password leaked",
    ))
}

#[post("/json")]
async fn json_body(Json(payload): Json<Payload>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "value": payload.value }))
}

#[get("/path/{id}")]
async fn path_param(Path(_id): Path<i64>) -> &'static str {
    "ok"
}

#[get("/query")]
async fn query_param(Query(_search): Query<Search>) -> &'static str {
    "ok"
}

#[post("/validated")]
async fn validated_body(Valid(Json(_account)): Valid<Json<NewAccount>>) -> &'static str {
    "ok"
}

#[post("/csrf")]
async fn csrf_target() -> &'static str {
    "ok"
}

fn problem_json(response: &TestResponse, status: u16, code: &str) -> serde_json::Value {
    response.assert_status(status);
    response.assert_header_contains("content-type", "application/problem+json");

    let json: serde_json::Value = response.json();
    assert_eq!(json["status"], status);
    assert_eq!(json["code"], code);
    assert!(json["type"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(json["title"].as_str().is_some_and(|s| !s.is_empty()));
    assert!(json["detail"].as_str().is_some());
    assert!(
        json.as_object().unwrap().contains_key("instance"),
        "instance must be a stable key"
    );
    assert!(
        json.as_object().unwrap().contains_key("request_id"),
        "request_id must be a stable key"
    );
    assert!(json["errors"].as_array().is_some());
    json
}

fn client() -> autumn_web::test::TestClient {
    TestApp::new()
        .routes(routes![
            bad_request,
            unauthorized,
            forbidden,
            conflict,
            service_unavailable,
            boom,
            json_body,
            path_param,
            query_param,
            validated_body,
            csrf_target
        ])
        .build()
}

#[tokio::test]
async fn autumn_error_matrix_uses_problem_details() {
    let client = client();

    let cases = [
        ("/bad-request", 400, "autumn.bad_request"),
        ("/unauthorized", 401, "autumn.unauthorized"),
        ("/forbidden", 403, "autumn.forbidden"),
        ("/missing-route", 404, "autumn.not_found"),
        ("/conflict", 409, "autumn.conflict"),
        ("/service-unavailable", 503, "autumn.service_unavailable"),
    ];

    for (path, status, code) in cases {
        let response = client
            .get(path)
            .header("accept", "application/json")
            .send()
            .await;
        let json = problem_json(&response, status, code);
        assert_eq!(json["request_id"], response.header("x-request-id").unwrap());
        assert_eq!(json["instance"], path);
    }
}

#[tokio::test]
async fn invalid_json_body_uses_problem_details() {
    let response = client()
        .post("/json")
        .header("accept", "application/json")
        .header("content-type", "application/json")
        .body("not json")
        .send()
        .await;

    let json = problem_json(&response, 400, "autumn.bad_request");
    assert!(json["detail"].as_str().unwrap().contains("JSON"));
}

#[tokio::test]
async fn path_and_query_parse_errors_use_problem_details() {
    let client = client();

    let path_response = client
        .get("/path/not-a-number")
        .header("accept", "application/json")
        .send()
        .await;
    problem_json(&path_response, 400, "autumn.bad_request");

    let query_response = client
        .get("/query?page=not-a-number")
        .header("accept", "application/json")
        .send()
        .await;
    problem_json(&query_response, 400, "autumn.bad_request");
}

#[tokio::test]
async fn validation_errors_keep_field_level_problem_details() {
    let response = client()
        .post("/validated")
        .header("accept", "application/json")
        .json(&serde_json::json!({ "email": "not-an-email" }))
        .send()
        .await;

    let json = problem_json(&response, 422, "autumn.validation_failed");
    let errors = json["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0]["field"], "email");
    assert!(
        errors[0]["messages"].as_array().unwrap()[0]
            .as_str()
            .unwrap()
            .contains("email")
    );
}

#[tokio::test]
#[allow(clippy::field_reassign_with_default)]
async fn production_500_problem_details_do_not_leak_internal_detail() {
    let mut config = AutumnConfig::default();
    config.profile = Some("prod".to_owned());
    config.security.trusted_hosts.hosts = vec!["example.com".to_owned()];

    let client = TestApp::new().config(config).routes(routes![boom]).build();

    let response = client
        .get("/boom")
        .header("accept", "application/json")
        .header("host", "example.com")
        .send()
        .await;

    let json = problem_json(&response, 500, "autumn.internal_server_error");
    assert_eq!(json["detail"], "Internal server error");
    assert!(
        !response.text().contains("database password leaked"),
        "prod 500 JSON must not expose raw internal causes"
    );
    assert_eq!(json["request_id"], response.header("x-request-id").unwrap());
}

#[tokio::test]
async fn dev_500_problem_details_include_diagnostic_detail() {
    let client = TestApp::new().profile("dev").routes(routes![boom]).build();

    let response = client
        .get("/boom")
        .header("accept", "application/json")
        .send()
        .await;

    let json = problem_json(&response, 500, "autumn.internal_server_error");
    assert_eq!(json["detail"], "database password leaked");
}

#[tokio::test]
async fn html_requests_still_use_html_error_pages() {
    let response = client()
        .get("/boom")
        .header("accept", "text/html")
        .send()
        .await;

    response.assert_status(500);
    response.assert_header_contains("content-type", "text/html");
    assert!(response.text().contains("<!DOCTYPE html>"));
}

#[tokio::test]
async fn problem_json_accept_prevents_html_error_page_replacement() {
    let response = client()
        .get("/boom")
        .header("accept", "application/problem+json, text/html;q=0.1")
        .send()
        .await;

    let json = problem_json(&response, 500, "autumn.internal_server_error");
    assert_eq!(json["instance"], "/boom");
    assert!(!response.text().contains("<!DOCTYPE html>"));
}

#[tokio::test]
async fn csrf_failures_use_problem_details_for_json_clients() {
    let config = AutumnConfig {
        profile: Some("test".to_owned()),
        security: SecurityConfig {
            csrf: CsrfConfig {
                enabled: true,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };

    let client = TestApp::new()
        .config(config)
        .routes(routes![csrf_target])
        .build();

    let response = client
        .post("/csrf")
        .header("accept", "application/json")
        .send()
        .await;

    let json = problem_json(&response, 403, "autumn.csrf");
    assert_eq!(json["detail"], "CSRF token missing or invalid");
}

#[tokio::test]
async fn csrf_failures_honor_json_accept_quality_over_html() {
    let config = AutumnConfig {
        profile: Some("test".to_owned()),
        security: SecurityConfig {
            csrf: CsrfConfig {
                enabled: true,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };

    let client = TestApp::new()
        .config(config)
        .routes(routes![csrf_target])
        .build();

    for accept in [
        "application/json, text/html;q=0",
        "application/json;q=1, text/html;q=0.1",
    ] {
        let response = client.post("/csrf").header("accept", accept).send().await;

        let json = problem_json(&response, 403, "autumn.csrf");
        assert_eq!(json["instance"], "/csrf");
        assert_eq!(json["request_id"], response.header("x-request-id").unwrap());
    }
}

#[test]
fn problem_details_schema_fixture_defines_contract_keys() {
    let schema: serde_json::Value = serde_json::from_str(include_str!(
        "../../../docs/schemas/problem-details.schema.json"
    ))
    .expect("problem-details schema must be valid JSON");
    let required = schema["required"].as_array().expect("required array");
    for key in [
        "type",
        "title",
        "status",
        "detail",
        "instance",
        "code",
        "request_id",
        "errors",
    ] {
        assert!(
            required.iter().any(|v| v == key),
            "schema must require {key}"
        );
    }
}
