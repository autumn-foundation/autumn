//! Integration tests for my-app.
//!
//! Run with:
//!
//!     cargo test
//!
//! Add DB-backed tests with `TestDb` (feature = "test-support") and mark
//! them `#[ignore = "requires Docker"]`. See `docs/guide/testing.md` in
//! the Autumn repository for the full integration-testing walkthrough.

use autumn_web::prelude::*;
use autumn_web::test::TestApp;

// ── Handlers under test ────────────────────────────────────────────────────

#[get("/")]
async fn index() -> &'static str {
    "Welcome to Autumn!"
}

#[get("/hello")]
async fn hello() -> &'static str {
    "Hello, Autumn!"
}

// ── Tests ──────────────────────────────────────────────────────────────────

/// Smoke test — no Docker required.
///
/// `TestApp::new()` boots the full Autumn middleware pipeline in-process
/// without binding a TCP listener. All middleware (security, routing,
/// tracing, `RequestIdLayer`, …) runs exactly as in production.
#[tokio::test]
async fn get_index_returns_200() {
    let client = TestApp::new()
        .routes(routes![index, hello])
        .build();

    client
        .get("/")
        .send()
        .await
        .assert_ok()
        .assert_body_contains("Welcome");
}

/// Autumn-specific assertion: every response carries `X-Request-Id`.
///
/// `RequestIdLayer` is part of Autumn's default middleware stack. Its
/// presence proves the full pipeline ran — not just the handler.
#[tokio::test]
async fn autumn_attaches_request_id_to_every_response() {
    let client = TestApp::new()
        .routes(routes![index])
        .build();

    let resp = client.get("/").send().await;

    assert!(
        resp.header("x-request-id").is_some(),
        "Autumn's RequestIdLayer must attach X-Request-Id to every response"
    );
}
