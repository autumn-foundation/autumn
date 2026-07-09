//! Integration tests for the pluggable error-reporting pipeline (issue #798).
//!
//! Verifies that:
//!   * handler panics are caught at the HTTP layer and become a clean 500
//!     Problem Details response (never aborting the request task);
//!   * panics and 5xx responses produce exactly one [`ErrorEvent`] per request
//!     delivered to every registered reporter;
//!   * the event carries request context (status, message, route, method,
//!     request id) and — for panics — the panic payload + backtrace;
//!   * a panicking reporter never affects the client response;
//!   * `[reporting] enabled = false` suppresses delivery but still catches
//!     panics.

use std::time::Duration;

use autumn_web::config::AutumnConfig;
use autumn_web::reporting::{ErrorEvent, ErrorReporter, ReportFuture};
use autumn_web::test::TestApp;
use autumn_web::{get, routes};
use tokio::sync::mpsc;

// ── Test reporters ────────────────────────────────────────────────────────

/// Reporter that forwards every event over a channel so tests can await
/// delivery deterministically (reporting runs on a spawned task).
#[derive(Clone)]
struct ChannelReporter {
    tx: mpsc::UnboundedSender<ErrorEvent>,
}

impl ErrorReporter for ChannelReporter {
    fn report<'a>(&'a self, event: &'a ErrorEvent) -> ReportFuture<'a> {
        let tx = self.tx.clone();
        let event = event.clone();
        Box::pin(async move {
            let _ = tx.send(event);
        })
    }
}

/// Reporter that always panics — used to prove reporting failures are
/// swallowed and never reach the client.
struct PanickingReporter;

impl ErrorReporter for PanickingReporter {
    fn report<'a>(&'a self, _event: &'a ErrorEvent) -> ReportFuture<'a> {
        Box::pin(async move {
            panic!("reporter blew up");
        })
    }
}

// ── Handlers ────────────────────────────────────────────────────────────────

#[get("/boom")]
async fn boom() -> &'static str {
    panic!("kaboom in handler");
}

#[get("/explode/{id}")]
async fn explode() -> &'static str {
    panic!("kaboom with path param");
}

#[get("/fail")]
async fn fail() -> Result<&'static str, autumn_web::AutumnError> {
    Err(autumn_web::AutumnError::internal_server_error_msg(
        "database on fire",
    ))
}

#[get("/ok")]
async fn ok() -> &'static str {
    "ok"
}

#[get("/raw500")]
async fn raw500() -> axum::http::StatusCode {
    // A raw 5xx that does NOT go through `AutumnError`, so the response carries
    // no `AutumnErrorInfo` — exercises the reporting fallback path.
    axum::http::StatusCode::INTERNAL_SERVER_ERROR
}

async fn recv_one(rx: &mut mpsc::UnboundedReceiver<ErrorEvent>) -> ErrorEvent {
    tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("reporter should deliver an event within the timeout")
        .expect("reporter channel should not be closed")
}

// ── AC #1: panic is caught and becomes a clean 500 Problem Details ───────────

#[tokio::test]
async fn handler_panic_becomes_500_problem_details() {
    let client = TestApp::new().routes(routes![boom]).build();

    let resp = client.get("/boom").send().await;
    resp.assert_status(500);

    let ct = resp
        .header("content-type")
        .expect("error response must have a content-type");
    assert!(
        ct.contains("application/problem+json"),
        "panic should produce an RFC 7807 Problem Details response, got {ct}"
    );

    let body: serde_json::Value = resp.json();
    assert_eq!(body["status"], 500);
    // The internal panic message must never leak to the client.
    assert_eq!(body["detail"], "Internal server error");
}

#[tokio::test]
async fn server_survives_panic_and_serves_next_request() {
    let client = TestApp::new().routes(routes![boom, ok]).build();

    client.get("/boom").send().await.assert_status(500);
    // A panic must not poison the worker — the next request still succeeds.
    client.get("/ok").send().await.assert_status(200);
}

// ── AC #2/#3: panic produces exactly one event with full context ─────────────

#[tokio::test]
async fn panic_reported_once_with_context() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let client = TestApp::new()
        .routes(routes![explode])
        .with_error_reporter(ChannelReporter { tx })
        .build();

    client.get("/explode/42").send().await.assert_status(500);

    let event = recv_one(&mut rx).await;
    assert_eq!(event.status.as_u16(), 500);
    assert_eq!(event.method.as_deref(), Some("GET"));
    assert_eq!(event.route.as_deref(), Some("/explode/{id}"));
    assert!(
        event.request_id.is_some(),
        "event should carry the request id"
    );
    let panic = event
        .panic
        .as_ref()
        .expect("panic events must carry panic info");
    assert!(
        panic.payload.contains("kaboom with path param"),
        "panic payload should be captured, got {:?}",
        panic.payload
    );

    // Exactly one event for the request.
    assert!(
        tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .is_err(),
        "exactly one event should be delivered per panic"
    );
}

/// AC #3: panic events capture a backtrace when `RUST_BACKTRACE` is set, and
/// omit it otherwise. `Backtrace::capture()` memoizes the enabled flag from the
/// environment, so we mirror whatever the test binary was launched with rather
/// than toggling it mid-process.
#[tokio::test]
async fn panic_backtrace_tracks_rust_backtrace_env() {
    let backtrace_enabled =
        std::env::var("RUST_BACKTRACE").is_ok_and(|v| v != "0" && !v.is_empty());

    let (tx, mut rx) = mpsc::unbounded_channel();
    let client = TestApp::new()
        .routes(routes![boom])
        .with_error_reporter(ChannelReporter { tx })
        .build();

    client.get("/boom").send().await.assert_status(500);

    let event = recv_one(&mut rx).await;
    let panic = event.panic.expect("panic event must carry panic info");
    if backtrace_enabled {
        assert!(
            panic.backtrace.is_some(),
            "backtrace should be captured when RUST_BACKTRACE is set"
        );
    } else {
        assert!(
            panic.backtrace.is_none(),
            "backtrace should be absent when RUST_BACKTRACE is unset"
        );
    }
}

// ── AC #1 (5xx): server errors are reported too ──────────────────────────────

#[tokio::test]
async fn server_error_reported_once() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let client = TestApp::new()
        .routes(routes![fail])
        .with_error_reporter(ChannelReporter { tx })
        .build();

    client.get("/fail").send().await.assert_status(500);

    let event = recv_one(&mut rx).await;
    assert_eq!(event.status.as_u16(), 500);
    assert_eq!(event.method.as_deref(), Some("GET"));
    assert_eq!(event.route.as_deref(), Some("/fail"));
    assert!(event.panic.is_none(), "a plain 5xx is not a panic");
}

#[tokio::test]
async fn raw_5xx_without_autumn_error_is_reported() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let client = TestApp::new()
        .routes(routes![raw500])
        .with_error_reporter(ChannelReporter { tx })
        .build();

    client.get("/raw500").send().await.assert_status(500);

    let event = recv_one(&mut rx).await;
    assert_eq!(event.status.as_u16(), 500);
    assert!(
        event.panic.is_none(),
        "a raw status-code 5xx is not a panic"
    );
    // No AutumnErrorInfo on the response, so the message falls back to the
    // status' canonical reason rather than an error string.
    assert_eq!(event.message, "Internal Server Error");
}

#[tokio::test]
async fn success_responses_are_not_reported() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let client = TestApp::new()
        .routes(routes![ok])
        .with_error_reporter(ChannelReporter { tx })
        .build();

    client.get("/ok").send().await.assert_status(200);

    assert!(
        tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .is_err(),
        "2xx responses must not produce an error event"
    );
}

// ── AC #4: multiple reporters can be chained ─────────────────────────────────

#[tokio::test]
async fn multiple_reporters_all_receive_the_event() {
    let (tx1, mut rx1) = mpsc::unbounded_channel();
    let (tx2, mut rx2) = mpsc::unbounded_channel();
    let client = TestApp::new()
        .routes(routes![fail])
        .with_error_reporter(ChannelReporter { tx: tx1 })
        .with_error_reporter(ChannelReporter { tx: tx2 })
        .build();

    client.get("/fail").send().await.assert_status(500);

    let e1 = recv_one(&mut rx1).await;
    let e2 = recv_one(&mut rx2).await;
    assert_eq!(e1.status.as_u16(), 500);
    assert_eq!(e2.status.as_u16(), 500);
}

// ── AC #6: reporting failures never affect the client response ───────────────

#[tokio::test]
async fn panicking_reporter_does_not_break_response() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let client = TestApp::new()
        .routes(routes![fail])
        // First reporter panics; the second must still receive the event and
        // the client must still get its clean 500.
        .with_error_reporter(PanickingReporter)
        .with_error_reporter(ChannelReporter { tx })
        .build();

    client.get("/fail").send().await.assert_status(500);

    let event = recv_one(&mut rx).await;
    assert_eq!(event.status.as_u16(), 500);
}

// ── AC #7: [reporting] enabled = false suppresses delivery ───────────────────

#[tokio::test]
#[allow(clippy::field_reassign_with_default)]
async fn disabled_reporting_suppresses_delivery_but_still_catches_panics() {
    let mut config = AutumnConfig::default();
    config.profile = Some("test".into());
    config.security.csrf.enabled = false;
    config.reporting.enabled = false;

    let (tx, mut rx) = mpsc::unbounded_channel();
    let client = TestApp::new()
        .config(config)
        .routes(routes![boom])
        .with_error_reporter(ChannelReporter { tx })
        .build();

    // Panic is still converted to a clean 500 even with reporting disabled.
    client.get("/boom").send().await.assert_status(500);

    assert!(
        tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .is_err(),
        "no events should be delivered when reporting is disabled"
    );
}
