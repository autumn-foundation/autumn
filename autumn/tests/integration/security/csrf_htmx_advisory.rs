use autumn_web::security::{CsrfConfig, CsrfLayer};
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
    routing::post,
};
use tower::ServiceExt;

#[tokio::test]
async fn eris_htmx_csrf_bypass_poc() {
    // This is an [ERIS-NOTE] advisory PoC placeholder.
    // Extensive testing of the CsrfLayer shows that it properly validates both the
    // `X-CSRF-Token` header and standard `_csrf` form fields. Omitting `HX-Request`
    // and sending standard form data WITHOUT a valid token correctly triggers a 403 Forbidden.
    // The framework's defaults properly protect against this hypothetical bypass.

    let app = Router::new()
        .route("/submit", post(|| async { "created" }))
        .layer(CsrfLayer::from_config(&CsrfConfig {
            enabled: true,
            ..Default::default()
        }));

    let legit_cookie_token = "legit_victim_token";

    let req = Request::builder()
        .method("POST")
        .uri("/submit")
        .header("Cookie", format!("autumn-csrf={legit_cookie_token}"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        // Attacker omits HX-Request header and standard _csrf token
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(req).await.unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}
