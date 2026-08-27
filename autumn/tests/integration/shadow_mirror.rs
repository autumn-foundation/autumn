//! End-to-end shadow traffic mirroring and response diffing (issue #1653).
//!
//! These tests stand up a real **candidate build** — a separate axum server on
//! a loopback port — carrying one seeded response regression, mirror live
//! traffic to it through the framework's ingress stack, and assert the
//! acceptance criteria the issue names:
//!
//! * the regressed endpoint is flagged,
//! * unchanged endpoints report **zero** divergences,
//! * the client's bytes are identical whether mirroring is on or off,
//! * no mutating request is ever replayed,
//! * `/actuator/shadow` reports what happened.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use autumn_web::actuator::ProvideActuatorState;
use autumn_web::config::AutumnConfig;
use autumn_web::shadow::{SHADOW_HEADER, ShadowConfig};
use autumn_web::test::{TestApp, TestClient};
use autumn_web::{get, post, routes};
use axum::Json;
use serde_json::{Value, json};

// ── The live build ──────────────────────────────────────────────────────────

#[get("/api/orders")]
async fn live_orders() -> Json<Value> {
    Json(json!({ "id": 7, "total": 42, "items": ["a", "b"] }))
}

/// How many times [`counted_orders`] ran. Its own route, touched by exactly one
/// test, so the tests in this binary — which run in parallel — cannot disturb
/// each other's count.
static COUNTED_ORDER_CALLS: AtomicUsize = AtomicUsize::new(0);

#[get("/api/counted-orders")]
async fn counted_orders() -> Json<Value> {
    COUNTED_ORDER_CALLS.fetch_add(1, Ordering::SeqCst);
    Json(json!({ "id": 7, "total": 42, "items": ["a", "b"] }))
}

#[get("/api/status")]
async fn live_status() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

#[post("/api/orders")]
async fn create_order() -> Json<Value> {
    Json(json!({ "created": true }))
}

// ── The candidate build ─────────────────────────────────────────────────────

/// What the candidate saw, so tests can assert on the mirrored traffic itself.
#[derive(Debug, Default)]
struct CandidateLog {
    requests: std::sync::Mutex<Vec<(String, String, bool)>>,
}

impl CandidateLog {
    fn record(&self, method: &str, path: &str, loop_guard: bool) {
        if let Ok(mut requests) = self.requests.lock() {
            requests.push((method.to_owned(), path.to_owned(), loop_guard));
        }
    }

    fn seen(&self) -> Vec<(String, String, bool)> {
        self.requests
            .lock()
            .map(|requests| requests.clone())
            .unwrap_or_default()
    }
}

/// Start a candidate build on a loopback port.
///
/// `/api/orders` carries the **seeded regression**: the `total` field is
/// dropped. `/api/status` is byte-identical to the live build, so it is the
/// false-positive control.
async fn spawn_candidate() -> (SocketAddr, Arc<CandidateLog>) {
    let log = Arc::new(CandidateLog::default());

    let router = axum::Router::new()
        .route(
            "/api/orders",
            axum::routing::get(|| async { Json(json!({ "id": 7, "items": ["a", "b"] })) }),
        )
        .route(
            "/api/status",
            axum::routing::get(|| async { Json(json!({ "status": "ok" })) }),
        )
        .route(
            "/api/huge",
            axum::routing::get(|| async { "x".repeat(512 * 1024) }),
        )
        .route(
            "/api/orders-post",
            axum::routing::post(|| async { Json(json!({ "created": true })) }),
        )
        .layer(axum::middleware::from_fn({
            let log = Arc::clone(&log);
            move |req: axum::extract::Request, next: axum::middleware::Next| {
                let log = Arc::clone(&log);
                async move {
                    log.record(
                        req.method().as_str(),
                        req.uri().path(),
                        req.headers().contains_key(SHADOW_HEADER),
                    );
                    next.run(req).await
                }
            }
        }));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind candidate");
    let addr = listener.local_addr().expect("candidate addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (addr, log)
}

// ── Harness ─────────────────────────────────────────────────────────────────

fn mirroring_config(target: SocketAddr) -> AutumnConfig {
    let mut config = AutumnConfig::default();
    config.profile = Some("test".into());
    config.security.csrf.enabled = false;
    config.actuator.sensitive = true;
    config.shadow = ShadowConfig {
        enabled: true,
        target: Some(format!("http://{target}")),
        sample_rate: 1.0,
        // Well above anything these tests issue, so a mirror is never dropped
        // at the ceiling and the counts below are exact. (The ceiling itself is
        // covered by the layer's own unit tests.)
        max_in_flight: 64,
        ..ShadowConfig::default()
    };
    config
}

/// Poll `condition` every 10 ms for up to ~5 s. Mirroring runs on a detached
/// task by design, so tests observe its effects rather than awaiting it.
async fn settle(mut condition: impl FnMut() -> bool) {
    for _ in 0..500 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn stats(app: &TestClient) -> autumn_web::shadow::ShadowStats {
    app.state()
        .shadow()
        .map(|handle| handle.registry.stats())
        .unwrap_or_default()
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_seeded_regression_is_flagged_and_clean_routes_stay_clean() {
    let (addr, candidate) = spawn_candidate().await;
    let app = TestApp::new()
        .config(mirroring_config(addr))
        .routes(routes![live_orders, live_status])
        .build();

    // Ten mirrored requests: five to the regressed endpoint, five to the
    // control.
    for _ in 0..5 {
        let response = app.get("/api/orders").send().await;
        response.assert_status(200);
        assert_eq!(
            response.json::<Value>(),
            json!({ "id": 7, "total": 42, "items": ["a", "b"] }),
            "the client must receive the LIVE build's response, untouched"
        );

        app.get("/api/status").send().await.assert_status(200);
    }

    settle(|| stats(&app).compared >= 10).await;
    let stats = stats(&app);
    assert_eq!(stats.mirrored, 10);
    assert_eq!(stats.compared, 10, "every mirror must reach the differ");
    assert_eq!(stats.diverged, 5, "the regressed endpoint diverges");
    assert_eq!(stats.matched, 5, "the unchanged endpoint must not");
    assert_eq!(stats.shadow_errors, 0);
    assert_eq!(stats.shadow_timeouts, 0);
    assert_eq!(stats.dropped_at_capacity, 0);
    assert_eq!(stats.skipped_oversize, 0);

    // One record, not five: the same divergence collapses by fingerprint.
    let handle = app.state().shadow().expect("mirror handle");
    let records = handle.registry.recent();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].target, "/api/orders");
    assert_eq!(records[0].occurrences, 5);
    assert_eq!(records[0].divergence.kind.as_str(), "body");
    // The dropped field is visible in the redacted samples.
    let primary = records[0]
        .divergence
        .primary_sample
        .as_ref()
        .expect("json sample");
    assert_eq!(primary["total"], json!(42));
    let shadow = records[0]
        .divergence
        .shadow_sample
        .as_ref()
        .expect("json sample");
    assert!(shadow.get("total").is_none());

    // Every mirrored request carried the loop guard, and none of them was a
    // mutating method.
    let seen = candidate.seen();
    assert_eq!(seen.len(), 10);
    assert!(
        seen.iter()
            .all(|(method, _, guard)| method == "GET" && *guard)
    );
}

#[tokio::test]
async fn mutating_requests_are_never_replayed_against_the_candidate() {
    let (addr, candidate) = spawn_candidate().await;
    let app = TestApp::new()
        .config(mirroring_config(addr))
        .routes(routes![live_orders, create_order])
        .build();

    for _ in 0..3 {
        app.post("/api/orders").send().await.assert_status(200);
    }
    // One GET afterwards proves mirroring is on at all, so the POST result is
    // not a false negative from a dead mirror.
    app.get("/api/orders").send().await.assert_status(200);

    settle(|| stats(&app).compared >= 1).await;
    let seen = candidate.seen();
    assert!(
        seen.iter().all(|(method, _, _)| method == "GET"),
        "a mutating method reached the candidate: {seen:?}"
    );
    assert_eq!(stats(&app).mirrored, 1);
}

#[tokio::test]
async fn the_client_sees_identical_bytes_with_and_without_mirroring() {
    let (addr, _candidate) = spawn_candidate().await;

    let plain = TestApp::new().routes(routes![live_orders]).build();
    let plain_body = plain.get("/api/orders").send().await.text();

    let mirrored = TestApp::new()
        .config(mirroring_config(addr))
        .routes(routes![live_orders])
        .build();
    let mirrored_response = mirrored.get("/api/orders").send().await;
    let mirrored_body = mirrored_response.text();

    assert_eq!(plain_body, mirrored_body);

    settle(|| stats(&mirrored).compared >= 1).await;
    assert_eq!(stats(&mirrored).diverged, 1, "and it still diffed");
}

#[tokio::test]
async fn the_primary_handler_runs_exactly_once_per_client_request() {
    let (addr, _candidate) = spawn_candidate().await;
    let app = TestApp::new()
        .config(mirroring_config(addr))
        .routes(routes![counted_orders])
        .build();

    let before = COUNTED_ORDER_CALLS.load(Ordering::SeqCst);
    app.get("/api/counted-orders")
        .send()
        .await
        .assert_status(200);
    settle(|| stats(&app).mirrored >= 1).await;
    let after = COUNTED_ORDER_CALLS.load(Ordering::SeqCst);

    assert_eq!(
        after - before,
        1,
        "mirroring must not re-enter the live build"
    );
}

#[tokio::test]
async fn the_actuator_endpoint_reports_the_mirror_run() {
    let (addr, _candidate) = spawn_candidate().await;
    let app = TestApp::new()
        .config(mirroring_config(addr))
        .routes(routes![live_orders])
        .build();

    app.get("/api/orders").send().await.assert_status(200);
    settle(|| stats(&app).diverged >= 1).await;

    let response = app.get("/actuator/shadow").send().await;
    response.assert_status(200);
    let body = response.json::<Value>();
    assert_eq!(body["enabled"], json!(true));
    assert_eq!(body["target"], json!(format!("http://{addr}")));
    assert_eq!(body["stats"]["diverged"], json!(1));
    assert_eq!(body["divergences"][0]["target"], json!("/api/orders"));
    assert_eq!(body["divergences"][0]["kind"], json!("body"));
    assert!(
        body["divergences"][0]["fingerprint"]
            .as_str()
            .is_some_and(|f| !f.is_empty()),
        "a recorded divergence must be quotable by fingerprint"
    );
}

#[tokio::test]
async fn the_actuator_endpoint_reports_a_disabled_mirror_by_default() {
    let mut config = AutumnConfig::default();
    config.profile = Some("test".into());
    config.security.csrf.enabled = false;
    config.actuator.sensitive = true;

    let app = TestApp::new()
        .config(config)
        .routes(routes![live_orders])
        .build();
    app.get("/api/orders").send().await.assert_status(200);

    let response = app.get("/actuator/shadow").send().await;
    response.assert_status(200);
    let body = response.json::<Value>();
    assert_eq!(body["enabled"], json!(false));
    assert_eq!(body["stats"]["mirrored"], json!(0));
    assert!(app.state().shadow().is_none());
}

#[tokio::test]
async fn actuator_paths_that_would_amplify_the_candidate_are_never_mirrored() {
    let (addr, candidate) = spawn_candidate().await;
    let app = TestApp::new()
        .config(mirroring_config(addr))
        .routes(routes![live_orders])
        .build();

    for path in ["/actuator/health", "/actuator/shadow", "/health"] {
        let _ = app.get(path).send().await;
    }
    // A real request afterwards, so we can wait on a definite signal rather
    // than on the absence of one.
    app.get("/api/orders").send().await.assert_status(200);
    settle(|| stats(&app).compared >= 1).await;

    let seen = candidate.seen();
    assert_eq!(
        seen.len(),
        1,
        "only the application request may be mirrored: {seen:?}"
    );
    assert_eq!(seen[0].1, "/api/orders");
}

/// The live build's counterpart to the candidate's `/api/huge`.
#[get("/api/huge")]
async fn live_huge() -> String {
    "x".repeat(512 * 1024)
}

#[tokio::test]
async fn an_oversized_candidate_response_is_skipped_not_buffered() {
    let (addr, _candidate) = spawn_candidate().await;
    let mut config = mirroring_config(addr);
    config.shadow.max_body_bytes = 4096;
    let app = TestApp::new()
        .config(config)
        .routes(routes![live_huge])
        .build();

    let response = app.get("/api/huge").send().await;
    response.assert_status(200);
    assert_eq!(
        response.text().len(),
        512 * 1024,
        "the client still receives the whole body"
    );

    settle(|| stats(&app).skipped_oversize >= 1).await;
    let stats = stats(&app);
    assert_eq!(stats.mirrored, 1);
    assert_eq!(stats.skipped_oversize, 1);
    assert_eq!(stats.compared, 0);
    assert_eq!(stats.shadow_errors, 0);
}

#[tokio::test]
async fn an_unreachable_candidate_is_counted_and_never_affects_the_client() {
    // Bind and immediately drop, so the port is (almost certainly) closed.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    drop(listener);

    let mut config = mirroring_config(addr);
    config.shadow.timeout_ms = 500;
    let app = TestApp::new()
        .config(config)
        .routes(routes![live_orders])
        .build();

    let response = app.get("/api/orders").send().await;
    response.assert_status(200);
    assert_eq!(
        response.json::<Value>(),
        json!({ "id": 7, "total": 42, "items": ["a", "b"] })
    );

    settle(|| {
        let stats = stats(&app);
        stats.shadow_errors + stats.shadow_timeouts >= 1
    })
    .await;
    let stats = stats(&app);
    assert_eq!(stats.mirrored, 1);
    assert_eq!(stats.compared, 0);
    assert_eq!(stats.shadow_errors + stats.shadow_timeouts, 1);
}
