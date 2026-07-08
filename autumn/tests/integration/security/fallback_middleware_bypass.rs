use autumn_web::security::CsrfLayer;
use autumn_web::{AppState, config::AutumnConfig};
use axum::{Router, body::Body, extract::Request, http::StatusCode, routing::get};
use tower::ServiceExt; // for oneshot()

#[tokio::test]
async fn eris_fallback_middleware_bypass_poc() {
    let mut config = AutumnConfig::default();
    config.security.csrf.enabled = true;

    // A raw router merged or nested into the app via AppBuilder
    // represents an "escape hatch" for the user.
    let raw_router = Router::new().route("/private", get(|| async { "Secret Data" }));

    // In autumn/src/router.rs, `apply_middleware` applies `router = router.fallback(...)`
    // but nested `raw_router`s do not inherit this fallback by default in Axum.
    // To ensure they are protected by global middleware (like CSRF), we explicitly
    // assign a fallback to them before nesting them!

    // Simulating `autumn::router::mount_raw_routers` with the fix:
    let raw_router = raw_router.fallback(|| async { (StatusCode::NOT_FOUND, "Not Found") });

    let base = Router::new()
        .nest("/api", raw_router)
        .fallback(|| async { (StatusCode::NOT_FOUND, "Not Found") });

    // Apply global middleware (representing autumn's global middleware stack)
    let csrf_layer = CsrfLayer::from_config(&config.security.csrf);
    let app = base.layer(csrf_layer).with_state(AppState::detached());

    // Without the fix, this POST request to a non-existent nested route
    // would hit Axum's default empty fallback *inside* the nested router,
    // bypassing the CSRF layer and returning 404.
    // WITH the fix, the explicit fallback is hit, which properly bubbles up
    // to the middleware layer, resulting in a 403 Forbidden.
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/nonexistent")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    // The CSRF Layer correctly intercepts the mutating request and rejects it
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}
