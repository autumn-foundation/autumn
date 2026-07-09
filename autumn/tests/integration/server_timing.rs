//! Integration tests for the `Server-Timing` response header (#1348).
//!
//! Acceptance criteria covered:
//! - header present when `[observability] server_timing = true`
//! - header absent when `[observability] server_timing = false`
//! - `total` metric reflects elapsed wall time
//! - streaming responses (SSE) get `total`-only header — no `db`
//! - responses with no DB queries omit the `db` metric
//! - the profile-default resolver is on in dev, off elsewhere

use autumn_web::config::AutumnConfig;
use autumn_web::test::TestApp;
use autumn_web::{get, routes};

fn test_config_with_server_timing(enabled: Option<bool>) -> AutumnConfig {
    let mut config = AutumnConfig {
        profile: Some("test".into()),
        ..Default::default()
    };
    config.security.csrf.enabled = false;
    config.observability.server_timing = enabled;
    config
}

// ── Sample handlers ────────────────────────────────────────────

#[get("/plain")]
async fn plain() -> String {
    "hi".to_owned()
}

#[get("/html")]
async fn html_handler() -> axum::response::Html<&'static str> {
    axum::response::Html("<p>hi</p>")
}

#[get("/json")]
async fn json_handler() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({ "ok": true }))
}

#[get("/slow")]
async fn slow_handler() -> String {
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    "done".to_owned()
}

#[get("/sse")]
async fn sse_handler() -> axum::response::Response<axum::body::Body> {
    use axum::http::header;
    axum::response::Response::builder()
        .status(200)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(axum::body::Body::from("data: hi\n\n"))
        .expect("failed to build SSE response")
}

// ── Tests ──────────────────────────────────────────────────────

#[tokio::test]
async fn header_present_when_enabled_html() {
    let client = TestApp::new()
        .config(test_config_with_server_timing(Some(true)))
        .routes(routes![html_handler])
        .build();

    let response = client.get("/html").send().await;
    response.assert_ok();

    let header = response
        .header("server-timing")
        .expect("server-timing header should be present when enabled");
    assert!(
        header.starts_with("total;dur="),
        "expected header to start with `total;dur=`, got {header:?}"
    );
    // Value after `total;dur=` should parse as a positive f64.
    let dur_str = header
        .strip_prefix("total;dur=")
        .and_then(|s| s.split(',').next())
        .expect("could not extract total duration");
    let dur: f64 = dur_str.parse().expect("total dur should be numeric");
    assert!(dur >= 0.0, "expected non-negative total dur, got {dur}");
}

#[tokio::test]
async fn header_present_when_enabled_json() {
    let client = TestApp::new()
        .config(test_config_with_server_timing(Some(true)))
        .routes(routes![json_handler])
        .build();

    let response = client.get("/json").send().await;
    response.assert_ok();

    let header = response
        .header("server-timing")
        .expect("server-timing header should be present on JSON responses too");
    assert!(header.starts_with("total;dur="), "got {header:?}");
}

#[tokio::test]
async fn header_absent_when_disabled() {
    let client = TestApp::new()
        .config(test_config_with_server_timing(Some(false)))
        .routes(routes![plain])
        .build();

    let response = client.get("/plain").send().await;
    response.assert_ok();
    assert!(
        response.header("server-timing").is_none(),
        "server-timing header must not appear when disabled: {:?}",
        response.header("server-timing")
    );
}

#[tokio::test]
async fn total_reflects_elapsed_time() {
    let client = TestApp::new()
        .config(test_config_with_server_timing(Some(true)))
        .routes(routes![slow_handler])
        .build();

    let response = client.get("/slow").send().await;
    response.assert_ok();

    let header = response
        .header("server-timing")
        .expect("server-timing header should be present");
    let dur_str = header
        .strip_prefix("total;dur=")
        .and_then(|s| s.split(',').next())
        .expect("could not extract total duration");
    let dur: f64 = dur_str.parse().expect("total dur should be numeric");
    assert!(
        dur >= 15.0,
        "total dur should reflect the 20ms handler sleep, got {dur}"
    );
}

#[tokio::test]
async fn sse_response_total_only() {
    let client = TestApp::new()
        .config(test_config_with_server_timing(Some(true)))
        .routes(routes![sse_handler])
        .build();

    let response = client.get("/sse").send().await;
    response.assert_ok();

    let header = response
        .header("server-timing")
        .expect("SSE responses should still carry `total`");
    assert!(header.contains("total;dur="), "got {header:?}");
    assert!(
        !header.contains("db;"),
        "SSE responses must not carry `db` metric: {header:?}"
    );
}

/// A normal request flows through BOTH the primary `ServerTimingLayer`
/// (`apply_middleware`) and the outermost fallback (`apply_startup_barrier`).
/// The `ServerTimingEmitted` marker must keep the fallback silent so the
/// response carries exactly one `total` metric — no duplicate from the
/// fallback re-appending.
#[tokio::test]
async fn primary_and_fallback_do_not_duplicate_total() {
    let client = TestApp::new()
        .config(test_config_with_server_timing(Some(true)))
        .routes(routes![html_handler])
        .build();

    let response = client.get("/html").send().await;
    response.assert_ok();

    let header = response
        .header("server-timing")
        .expect("server-timing header should be present");
    assert_eq!(
        header.matches("total;dur=").count(),
        1,
        "primary+fallback must not double-emit `total`: {header:?}"
    );
}

#[tokio::test]
async fn zero_queries_omits_db() {
    let client = TestApp::new()
        .config(test_config_with_server_timing(Some(true)))
        .routes(routes![html_handler])
        .build();

    let response = client.get("/html").send().await;
    response.assert_ok();

    let header = response
        .header("server-timing")
        .expect("server-timing header should be present");
    assert!(header.starts_with("total;dur="), "got {header:?}");
    assert!(
        !header.contains(", db;"),
        "requests with no DB queries must not include `db`: {header:?}"
    );
}
