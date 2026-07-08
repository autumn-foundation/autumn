//! CSRF exemption matching must not be bypassable via path traversal.
//!
//! Exempt-path prefixes are matched against the *normalized* request path,
//! so a request like `POST /api/../submit` (or its percent-encoded
//! `%2e%2e` variant) cannot satisfy an `/api/` exemption while targeting a
//! protected route through any downstream component that resolves
//! dot-segments (reverse proxies, nested services, path-normalizing
//! middleware).

use autumn_web::security::CsrfConfig;
use autumn_web::security::CsrfLayer;
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
    routing::post,
};
use tower::ServiceExt;

fn app() -> Router {
    let config = CsrfConfig {
        enabled: true,
        exempt_paths: vec!["/api/".to_string()],
        ..Default::default()
    };

    Router::new()
        .route("/submit", post(|| async { "created" }))
        .route("/api/items", post(|| async { "created" }))
        .layer(CsrfLayer::from_config(&config))
}

async fn post_status(uri: &str) -> StatusCode {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    app().oneshot(req).await.unwrap().status()
}

#[tokio::test]
async fn dot_segment_path_does_not_match_exemption() {
    // `/api/../submit` normalizes to `/submit`, which is NOT exempt, so the
    // tokenless POST must be rejected by CSRF (403) instead of being waved
    // through the middleware as `/api/`-exempt.
    assert_eq!(post_status("/api/../submit").await, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn encoded_dot_segment_path_does_not_match_exemption() {
    // Percent-encoded dot-segments must be treated the same as literal ones.
    assert_eq!(
        post_status("/api/%2e%2e/submit").await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(post_status("/api/.%2E/submit").await, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn nested_dot_segments_do_not_match_exemption() {
    // Even with extra segments and duplicate slashes mixed in, the
    // normalized path (`/submit`) decides the exemption.
    assert_eq!(
        post_status("/api/v1/../..//submit").await,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn literal_exempt_path_is_still_exempt() {
    // A genuinely exempt path keeps working without a CSRF token.
    assert_eq!(post_status("/api/items").await, StatusCode::OK);
}

#[tokio::test]
async fn protected_path_still_requires_token() {
    // Sanity check: the protected route rejects tokenless POSTs outright.
    assert_eq!(post_status("/submit").await, StatusCode::FORBIDDEN);
}
