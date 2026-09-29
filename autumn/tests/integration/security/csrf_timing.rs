use autumn_web::security::CsrfConfig;
use autumn_web::security::CsrfLayer;
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
    routing::post,
};
use tower::ServiceExt;

#[tokio::test]
async fn test_csrf_timing_attack() {
    let config = CsrfConfig {
        enabled: true,
        ..Default::default()
    };

    let app = Router::new()
        .route("/submit", post(|| async { "created" }))
        .layer(CsrfLayer::from_config(&config));

    // The real token to match
    let token = "12345678-1234-1234-1234-123456789012".to_string();

    // We'll write the test to ensure that the code compiles with the fix,
    // and that the basic token verification works.

    let valid_req = Request::builder()
        .method("POST")
        .uri("/submit")
        .header("Cookie", format!("autumn-csrf={token}"))
        .header("X-CSRF-Token", &token)
        .body(Body::empty())
        .unwrap();

    let response = app.clone().oneshot(valid_req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let invalid_req = Request::builder()
        .method("POST")
        .uri("/submit")
        .header("Cookie", format!("autumn-csrf={token}"))
        .header("X-CSRF-Token", "12345678-1234-1234-1234-123456789013")
        .body(Body::empty())
        .unwrap();

    let response2 = app.clone().oneshot(invalid_req).await.unwrap();
    assert_eq!(response2.status(), StatusCode::FORBIDDEN);
}
