//! Integration tests for the built-in inbound request timeout (issue #972).
//!
//! Exercises the full middleware stack via [`TestApp`] to verify:
//! - a slow handler yields a framework-standard `503` (Problem Details JSON for
//!   API clients, the HTML error page for browsers) — never a raw tower error;
//! - the per-route macro attributes `timeout_ms = ...` and `timeout = "off"`
//!   extend or disable the deadline end-to-end;
//! - streaming (SSE) responses are exempt: the deadline bounds time-to-response,
//!   not body-streaming duration;
//! - a fast route is never spuriously timed out when the deadline is enabled.

use std::convert::Infallible;
use std::time::Duration;

use autumn_web::config::AutumnConfig;
use autumn_web::sse::{Event, Sse};
use autumn_web::test::TestApp;
use autumn_web::{get, post, routes};
use futures::stream::Stream;

/// Build a config with a tight global request deadline (in ms).
fn with_global_timeout(ms: u64) -> AutumnConfig {
    let mut config = AutumnConfig::default();
    config.server.timeouts.request_timeout_ms = Some(ms);
    config
}

// ── Handlers ─────────────────────────────────────────────────────────────

#[get("/slow")]
async fn slow() -> &'static str {
    tokio::time::sleep(Duration::from_millis(300)).await;
    "done"
}

#[get("/fast")]
async fn fast() -> &'static str {
    "quick"
}

// Known-slow endpoint with an extended per-route deadline (AC4).
#[get("/export", timeout_ms = 5000)]
async fn export() -> &'static str {
    tokio::time::sleep(Duration::from_millis(200)).await;
    "report"
}

// Intentionally long-lived endpoint exempted from the deadline (AC4).
#[get("/longpoll", timeout = "off")]
async fn longpoll() -> &'static str {
    tokio::time::sleep(Duration::from_millis(200)).await;
    "eventually"
}

// Two methods sharing one path template: the GET is exempt, the POST inherits
// the global deadline — proving overrides are method-specific, not path-wide.
#[get("/items", timeout = "off")]
async fn items_list() -> &'static str {
    tokio::time::sleep(Duration::from_millis(200)).await;
    "list"
}

#[post("/items")]
async fn items_create() -> &'static str {
    tokio::time::sleep(Duration::from_millis(200)).await;
    "created"
}

// SSE route: returns immediately, then streams events past the deadline (AC3).
#[get("/sse")]
async fn sse() -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = futures::stream::unfold(0u8, |i| async move {
        if i >= 2 {
            return None;
        }
        if i > 0 {
            // Emit the second event well after the global deadline elapses.
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        Some((
            Ok::<_, Infallible>(Event::default().data(format!("tick-{i}"))),
            i + 1,
        ))
    });
    Sse::new(stream)
}

// ── AC2: 503 Problem Details for API clients ─────────────────────────────

#[tokio::test]
async fn slow_handler_returns_503_problem_json_for_api_client() {
    let client = TestApp::new()
        .routes(routes![slow])
        .config(with_global_timeout(50))
        .build();

    let resp = client
        .get("/slow")
        .header("accept", "application/json")
        .send()
        .await;

    resp.assert_status(503);
    resp.assert_header_contains("content-type", "application/problem+json");
    let body: serde_json::Value = resp.json();
    assert_eq!(body["status"], 503, "Problem Details status must be 503");
}

// ── AC2: HTML error page for browser navigation ──────────────────────────

#[tokio::test]
async fn slow_handler_renders_html_error_page_for_browser() {
    let client = TestApp::new()
        .routes(routes![slow])
        .config(with_global_timeout(50))
        .build();

    let resp = client
        .get("/slow")
        .header("accept", "text/html")
        .send()
        .await;

    resp.assert_status(503);
    resp.assert_header_contains("content-type", "text/html");
    // The correlation id must survive the HTML rebuild so operators can tie the
    // error page to their logs.
    assert!(
        resp.header("x-request-id").is_some(),
        "HTML timeout page must preserve the X-Request-Id header"
    );
    let body = resp.text();
    assert!(
        body.contains("<!DOCTYPE html") || body.contains("<html"),
        "browser clients must receive the HTML error page, got: {body}"
    );
}

// ── Codex #5: per-route overrides are method-specific, not path-wide ──────

#[tokio::test]
async fn timeout_override_is_method_specific() {
    let client = TestApp::new()
        .routes(routes![items_list, items_create])
        .config(with_global_timeout(50))
        .build();

    // GET /items is exempt (`timeout = "off"`) → its 200ms work completes.
    client.get("/items").send().await.assert_status(200);
    // POST /items shares the template but inherits the 50ms global → 503.
    // The GET's exemption must NOT bleed onto the POST handler.
    client.post("/items").send().await.assert_status(503);
}

// ── AC4: per-route override extends the deadline ─────────────────────────

#[tokio::test]
async fn per_route_timeout_ms_attribute_extends_deadline() {
    let client = TestApp::new()
        .routes(routes![slow, export])
        .config(with_global_timeout(50))
        .build();

    // The export route's 5s override lets its 200ms work finish.
    client.get("/export").send().await.assert_status(200);
    // A sibling inherit route is still bound by the 50ms global deadline.
    client.get("/slow").send().await.assert_status(503);
}

// ── AC4: per-route `timeout = "off"` disables the deadline ───────────────

#[tokio::test]
async fn per_route_timeout_off_attribute_disables_deadline() {
    let client = TestApp::new()
        .routes(routes![longpoll])
        .config(with_global_timeout(50))
        .build();

    client.get("/longpoll").send().await.assert_status(200);
}

// ── AC3: streaming responses are exempt ──────────────────────────────────

#[tokio::test]
async fn sse_stream_survives_global_timeout() {
    let client = TestApp::new()
        .routes(routes![sse])
        .config(with_global_timeout(50))
        .build();

    let resp = client.get("/sse").send().await;
    resp.assert_status(200);
    // Both events arrive even though the second is emitted long after the 50ms
    // deadline — the timeout never interrupts the response body.
    resp.assert_body_contains("tick-0");
    resp.assert_body_contains("tick-1");
}

// ── AC7 / composition: fast route is never spuriously timed out ──────────

#[tokio::test]
async fn fast_route_not_timed_out_when_deadline_enabled() {
    let client = TestApp::new()
        .routes(routes![fast])
        .config(with_global_timeout(50))
        .build();

    client
        .get("/fast")
        .send()
        .await
        .assert_status(200)
        .assert_body_contains("quick");
}

// ── AC7: the timeout composes with graceful shutdown drain ───────────────

// A handler that hangs well past the deadline, used to prove that a request
// which times out *while the server is draining* returns a single clean 503
// (freeing its worker) — no double cancellation, no hang.
#[get("/hang")]
async fn hang() -> &'static str {
    tokio::time::sleep(Duration::from_secs(30)).await;
    "never"
}

#[tokio::test(flavor = "multi_thread")]
async fn timeout_fires_cleanly_during_graceful_drain() {
    use std::net::SocketAddr;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio_util::sync::CancellationToken;

    let shutdown = CancellationToken::new();
    let shutdown_clone = shutdown.clone();

    // 100ms global deadline; the handler hangs for 30s.
    let tc = TestApp::new()
        .routes(routes![hang])
        .config(with_global_timeout(100))
        .build();
    let probes = tc.probes().clone();
    let router = tc.into_router();

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr: SocketAddr = listener.local_addr().expect("local_addr");

    let server = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(shutdown.cancelled_owned())
            .await
            .ok();
    });

    tokio::time::sleep(Duration::from_millis(20)).await;

    // Begin a request that will hang past the deadline.
    let client = tokio::spawn(async move {
        let mut stream = TcpStream::connect(addr).await.expect("connect");
        stream
            .write_all(b"GET /hang HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .expect("send");
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.expect("read");
        String::from_utf8_lossy(&buf).into_owned()
    });

    // Simulate SIGTERM mid-request: flip to draining and cancel the listener
    // while the hang handler is still in-flight.
    tokio::time::sleep(Duration::from_millis(40)).await;
    probes.begin_draining();
    shutdown_clone.cancel();

    // The in-flight request must still terminate with a single clean 503 from
    // the deadline (not hang for 30s, not a raw/duplicated error), proving the
    // timeout and the shutdown drain compose.
    let response = tokio::time::timeout(Duration::from_secs(5), client)
        .await
        .expect("hung request must be released by the deadline during drain")
        .expect("client task must not panic");
    assert!(
        response.starts_with("HTTP/1.1 503"),
        "a request that exceeds the deadline during drain must return a clean 503; got: {response}"
    );

    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("server must drain and exit cleanly")
        .ok();
}
