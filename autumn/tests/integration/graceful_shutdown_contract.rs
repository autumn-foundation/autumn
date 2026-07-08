//! Rolling-deploy shutdown contract tests for issue #679.
//!
//! Written RED-first: tests reference `ServerConfig::prestop_grace_secs`
//! and `run_shutdown_hooks_with_timeout` before either exists, so the
//! first `cargo test` run fails to compile (RED). The green phase adds
//! the missing symbols; the refactor phase cleans up.
//!
//! AC coverage:
//! - AC 2: `prestop_grace_secs` config field (≥ 5 default)
//! - AC 3: `autumn_shutdown_aborted_requests_total` counter
//! - AC 7: per-hook shutdown timeout + overrun logging (internal tests in app.rs)
//! - AC 9: integration test — SIGTERM during long HTTP request

use autumn_web::config::AutumnConfig;

// ── AC 2: prestop_grace_secs ─────────────────────────────────────────────────

#[test]
fn server_config_prestop_grace_secs_defaults_to_five() {
    let config = AutumnConfig::default();
    assert!(
        config.server.prestop_grace_secs >= 5,
        "prestop_grace_secs default must be ≥ 5 so load balancers can deregister before \
         the listener closes"
    );
}

#[test]
fn server_config_prestop_grace_secs_is_configurable_via_toml() {
    let config: AutumnConfig =
        toml::from_str("[server]\nprestop_grace_secs = 12\n").expect("config should deserialize");
    assert_eq!(config.server.prestop_grace_secs, 12);
}

#[test]
fn server_config_prestop_grace_secs_env_override() {
    use autumn_web::config::MockEnv;
    let env = MockEnv::new().with("AUTUMN_SERVER__PRESTOP_GRACE_SECS", "8");
    let mut config = AutumnConfig::default();
    config.apply_env_overrides_with_env(&env);
    assert_eq!(config.server.prestop_grace_secs, 8);
}

// ── AC 3: autumn_shutdown_aborted_requests_total ─────────────────────────────

#[test]
fn metrics_snapshot_includes_shutdown_aborted_requests() {
    use autumn_web::middleware::MetricsCollector;
    let collector = MetricsCollector::new();
    let snap = collector.snapshot();
    assert_eq!(
        snap.http.shutdown_aborted_requests_total, 0,
        "aborted-requests counter must start at zero"
    );
}

#[test]
fn metrics_collector_can_record_aborted_requests() {
    use autumn_web::middleware::MetricsCollector;
    let collector = MetricsCollector::new();
    collector.record_shutdown_aborted(3);
    let snap = collector.snapshot();
    assert_eq!(snap.http.shutdown_aborted_requests_total, 3);
}

// ── AC 9: integration — SIGTERM during long HTTP request ─────────────────────

// Handler defined at module level so it isn't treated as an item-after-statement.
#[autumn_web::get("/slow")]
async fn slow_handler() -> &'static str {
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    "done"
}

/// Spins up a real TCP server, begins a slow HTTP request, fires the
/// shutdown `CancellationToken` (simulating SIGTERM), then asserts:
///  (a) the slow request completes (not dropped mid-flight), and
///  (b) `/ready` was draining (503) before the listener closed.
///
/// Uses raw tokio TCP + hand-written HTTP/1.1 to avoid adding `reqwest`
/// to dev-deps.
#[tokio::test(flavor = "multi_thread")]
async fn sigterm_during_long_http_request_drains_cleanly() {
    use autumn_web::prelude::*;
    use autumn_web::test::TestApp;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio_util::sync::CancellationToken;

    let request_completed = Arc::new(AtomicBool::new(false));
    let ready_was_503 = Arc::new(AtomicBool::new(false));

    let rc = Arc::clone(&request_completed);
    let r503 = Arc::clone(&ready_was_503);

    let shutdown = CancellationToken::new();
    let shutdown_clone = shutdown.clone();

    // Build via TestApp so probe_state is wired to the same AppState the
    // router uses — this connects begin_draining() to the /ready handler.
    let tc = TestApp::new().routes(routes![slow_handler]).build();
    let probe_clone = tc.probes().clone();
    let router = tc.into_router();

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr: SocketAddr = listener.local_addr().expect("local_addr");

    tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(shutdown.cancelled_owned())
            .await
            .ok();
    });

    tokio::time::sleep(Duration::from_millis(20)).await;

    // Start the slow request via raw TCP before the shutdown fires.
    let client_task = tokio::spawn(async move {
        let mut stream = TcpStream::connect(addr).await.expect("connect");
        stream
            .write_all(b"GET /slow HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .expect("send request");
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.expect("read response");
        let response = String::from_utf8_lossy(&buf);
        assert!(
            response.starts_with("HTTP/1.1 200"),
            "/slow must respond 200; got: {response}"
        );
        rc.store(true, Ordering::SeqCst);
    });

    // After 50 ms the slow request is in-flight (handler sleeps 200 ms).
    // Flip /ready → 503 before cancelling the listener; verify via HTTP.
    tokio::time::sleep(Duration::from_millis(50)).await;
    probe_clone.begin_draining();

    // Query /ready on the live server — the handler reads the same ProbeState
    // we just flipped, so the HTTP response must be 503.
    let mut ready_stream = TcpStream::connect(addr).await.expect("connect /ready");
    ready_stream
        .write_all(b"GET /ready HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .expect("send /ready");
    let mut ready_buf = Vec::new();
    ready_stream
        .read_to_end(&mut ready_buf)
        .await
        .expect("read /ready");
    let ready_resp = String::from_utf8_lossy(&ready_buf);
    r503.store(ready_resp.starts_with("HTTP/1.1 503"), Ordering::SeqCst);

    shutdown_clone.cancel();

    tokio::time::timeout(Duration::from_secs(3), client_task)
        .await
        .expect("slow request should complete within drain window")
        .expect("client task must not panic");

    assert!(
        request_completed.load(Ordering::SeqCst),
        "in-flight HTTP request must complete before server exits"
    );
    assert!(
        ready_was_503.load(Ordering::SeqCst),
        "/ready must return 503 after begin_draining(), before the listener closed; \
         got: {ready_resp}"
    );
}

// ── AC 9b: job workers observe drain via ProbeState ──────────────────────────

/// Asserts that `begin_draining()` sets the state that job-runtime loops
/// observe via `is_shutting_down()` — verifying the probe contract that
/// lets job workers stop dequeuing at drain start.
#[test]
fn probe_draining_is_observable_for_job_runtime_coordination() {
    use autumn_web::probe::ProbeState;

    let probe = ProbeState::ready_for_test();
    assert!(!probe.is_shutting_down());
    assert!(!probe.draining());

    probe.begin_draining();
    assert!(probe.is_shutting_down());
    assert!(probe.draining());
}
