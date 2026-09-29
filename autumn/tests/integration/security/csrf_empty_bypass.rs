use autumn_web::security::CsrfConfig;
use autumn_web::security::CsrfLayer;
use axum::http::Request;
use axum::{Router, body::Body, routing::post};
use tower::ServiceExt;

#[tokio::test]
async fn test_csrf_bypass_empty_tokens() {
    let app = Router::new()
        .route("/submit", post(|| async { "created" }))
        .layer(CsrfLayer::from_config(&CsrfConfig {
            enabled: true,
            ..Default::default()
        }));

    let req = Request::builder()
        .method("POST")
        .uri("/submit")
        .header("Cookie", "autumn-csrf=")
        .header("X-CSRF-Token", "")
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(req).await.unwrap();

    assert_eq!(response.status(), axum::http::StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_csrf_bypass_empty_form_tokens() {
    let app = Router::new()
        .route("/submit", post(|| async { "created" }))
        .layer(CsrfLayer::from_config(&CsrfConfig {
            enabled: true,
            ..Default::default()
        }));

    let req = Request::builder()
        .method("POST")
        .uri("/submit")
        .header("Cookie", "autumn-csrf=")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(Body::from("_csrf="))
        .unwrap();

    let response = app.oneshot(req).await.unwrap();

    assert_eq!(response.status(), axum::http::StatusCode::FORBIDDEN);
}
