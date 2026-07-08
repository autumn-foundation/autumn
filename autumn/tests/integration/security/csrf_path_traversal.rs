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
async fn eris_csrf_path_traversal_bypass() {
    let config = CsrfConfig {
        enabled: true,
        exempt_paths: vec!["/api/".to_string()],
        ..Default::default()
    };

    let app = Router::new()
        .route("/submit", post(|| async { "created" }))
        .route("/api/items", post(|| async { "created" }))
        .layer(CsrfLayer::from_config(&config));

    let malicious_req = Request::builder()
        .method("POST")
        .uri("/api/../submit")
        .body(Body::empty())
        .unwrap();

    let response = app.clone().oneshot(malicious_req).await.unwrap();

    // Axum routes strictly based on exact URI match and does not resolve '..'.
    // Therefore, the request to /api/../submit safely falls through to a 404
    // instead of executing the /submit handler, averting the CSRF bypass entirely.
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}
