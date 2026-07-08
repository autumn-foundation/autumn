use autumn_web::security::CsrfConfig;
use autumn_web::security::CsrfLayer;
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
    routing::post,
};
use tower::ServiceExt;
use uuid::Uuid;

#[tokio::test]
async fn eris_csrf_form_bypass_fixed() {
    let config = CsrfConfig {
        enabled: true,
        ..Default::default()
    };

    let app = Router::new()
        .route("/submit", post(|| async { "created" }))
        .layer(CsrfLayer::from_config(&config));

    let token = Uuid::new_v4().to_string();

    let body_content = format!("_csrf={token}&other_data=foo");

    // This request mimics a standard HTML form submission where the CSRF token
    // is included in the body, but no custom headers are set by JS.
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/submit")
                .header("Cookie", format!("autumn-csrf={token}"))
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(Body::from(body_content))
                .unwrap(),
        )
        .await
        .unwrap();

    // The middleware should correctly parse the body, find `_csrf` matching the cookie,
    // and let the request through (Status 200 OK) instead of blocking it (Status 403 Forbidden).
    assert_eq!(response.status(), StatusCode::OK);
}
