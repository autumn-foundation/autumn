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
async fn test_csrf_constant_time_length() {
    let config = CsrfConfig {
        enabled: true,
        ..Default::default()
    };

    let app = Router::new()
        .route("/submit", post(|| async { "created" }))
        .layer(CsrfLayer::from_config(&config));

    let token = "12345678-1234-1234-1234-123456789012".to_string(); // 36 chars

    // Request with token of length 1
    let req_short = Request::builder()
        .method("POST")
        .uri("/submit")
        .header("Cookie", format!("autumn-csrf={token}"))
        .header("X-CSRF-Token", "1")
        .body(Body::empty())
        .unwrap();

    // Request with token of length 36 but wrong chars
    let req_same_len = Request::builder()
        .method("POST")
        .uri("/submit")
        .header("Cookie", format!("autumn-csrf={token}"))
        .header("X-CSRF-Token", "22345678-1234-1234-1234-123456789012")
        .body(Body::empty())
        .unwrap();

    let res1 = app.clone().oneshot(req_short).await.unwrap();
    let res2 = app.oneshot(req_same_len).await.unwrap();

    // Both a wrong-length token and a same-length wrong token must be rejected.
    assert_eq!(res1.status(), StatusCode::FORBIDDEN);
    assert_eq!(res2.status(), StatusCode::FORBIDDEN);
}
