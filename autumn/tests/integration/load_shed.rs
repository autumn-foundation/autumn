//! Integration tests for admission control / load shedding (#1006).
//!
//! Acceptance criteria covered end-to-end (through the full framework
//! middleware stack, not the bare-router unit tests in
//! `autumn::middleware::load_shed`):
//! - a shed request is access-logged with `status = 503` (composes with #999)
//! - `/actuator/prometheus` exposes `autumn_requests_shed_total` and it
//!   increments on a shed request
//! - health/liveness/readiness probes are never shed, even while the ceiling
//!   is saturated
//! - the default configuration (`server.max_concurrent_requests` unset)
//!   never sheds — today's unlimited behavior is preserved

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use autumn_web::config::AutumnConfig;
use autumn_web::test::TestApp;
use autumn_web::{get, routes};
use tokio::sync::Notify;
use tracing_subscriber::layer::SubscriberExt as _;

/// `tracing` target carried by every access-log event (#999).
const ACCESS_TARGET: &str = "autumn::access";

// ── Access-log capture (mirrors tests/integration/access_log.rs) ────────

#[derive(Debug, Clone, Default)]
struct CapturedEvent {
    fields: BTreeMap<String, String>,
}

impl CapturedEvent {
    fn field(&self, name: &str) -> Option<&str> {
        self.fields.get(name).map(String::as_str)
    }
}

#[derive(Clone, Default)]
struct AccessLogCapture {
    events: Arc<Mutex<Vec<CapturedEvent>>>,
}

impl AccessLogCapture {
    fn captured(&self) -> Vec<CapturedEvent> {
        self.events.lock().unwrap().clone()
    }
}

struct FieldVisitor<'a>(&'a mut BTreeMap<String, String>);

impl tracing::field::Visit for FieldVisitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().to_owned(), format!("{value:?}"));
    }
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().to_owned(), value.to_owned());
    }
    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.0.insert(field.name().to_owned(), value.to_string());
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for AccessLogCapture {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if event.metadata().target() != ACCESS_TARGET {
            return;
        }
        let mut fields = BTreeMap::new();
        event.record(&mut FieldVisitor(&mut fields));
        self.events.lock().unwrap().push(CapturedEvent { fields });
    }
}

/// Installs a thread-local capturing subscriber and returns it plus its guard.
///
/// Every test in this file must call this (discarding the `AccessLogCapture`
/// if unused) rather than running with the thread's ambient default. `tracing`
/// callsite `Interest` is a single value cached **per callsite across the
/// whole process**, combined from every currently-active dispatcher
/// (thread-local overrides included). This file spawns concurrent in-flight
/// requests via `tokio::spawn` and `cargo test` runs `#[tokio::test]`
/// functions across multiple OS threads in parallel by default: a thread with
/// no override at all falls back to a no-op ambient dispatcher, and if that
/// thread races another test's real subscriber while the access-log
/// callsite's `Interest` is first computed, the combined result can end up
/// cached as "not interested" — silently dropping the access-log event on
/// *every* thread, including ones with a real capturing subscriber installed.
/// Giving every test its own real (non-no-op) subscriber avoids that.
fn install_capture() -> (AccessLogCapture, tracing::subscriber::DefaultGuard) {
    let capture = AccessLogCapture::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    let guard = tracing::subscriber::set_default(subscriber);
    // Belt-and-braces: force the cached `Interest` to be re-evaluated now
    // that this thread's dispatcher exists, in case it was already poisoned
    // by an earlier no-op-dispatcher thread.
    tracing::callsite::rebuild_interest_cache();
    (capture, guard)
}

fn config_with_ceiling(ceiling: usize) -> AutumnConfig {
    let mut config = AutumnConfig {
        profile: Some("test".into()),
        ..Default::default()
    };
    config.security.csrf.enabled = false;
    config.server.max_concurrent_requests = Some(ceiling);
    config
}

// ── Blocking handlers: each test gets its own gate + counter so parallel
// tests in this binary never interfere with one another. ────────────────

static GATE_SHED_LOG: LazyLock<Notify> = LazyLock::new(Notify::new);
static ENTERED_SHED_LOG: AtomicUsize = AtomicUsize::new(0);

#[get("/block")]
async fn block_shed_log() -> &'static str {
    ENTERED_SHED_LOG.fetch_add(1, Ordering::SeqCst);
    GATE_SHED_LOG.notified().await;
    "released"
}

static GATE_PROBE: LazyLock<Notify> = LazyLock::new(Notify::new);
static ENTERED_PROBE: AtomicUsize = AtomicUsize::new(0);

#[get("/block")]
async fn block_probe() -> &'static str {
    ENTERED_PROBE.fetch_add(1, Ordering::SeqCst);
    GATE_PROBE.notified().await;
    "released"
}

static GATE_METRICS: LazyLock<Notify> = LazyLock::new(Notify::new);
static ENTERED_METRICS: AtomicUsize = AtomicUsize::new(0);

#[get("/block")]
async fn block_metrics() -> &'static str {
    ENTERED_METRICS.fetch_add(1, Ordering::SeqCst);
    GATE_METRICS.notified().await;
    "released"
}

#[get("/ping")]
async fn ping() -> &'static str {
    "pong"
}

/// Poll until `counter` reaches `expected`, so the test can deterministically
/// wait for N concurrently-fired requests to be admitted (past the load-shed
/// gate, into the handler body) before firing the deciding request.
async fn wait_for_entered(counter: &AtomicUsize, expected: usize) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while counter.load(Ordering::SeqCst) < expected {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("handlers did not reach the expected in-flight count in time");
}

// ── Tests ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn shed_request_is_access_logged_with_503_status() {
    let (capture, _guard) = install_capture();
    let client = Arc::new(
        TestApp::new()
            .config(config_with_ceiling(1))
            .routes(routes![block_shed_log])
            .build(),
    );

    // Occupy the single slot.
    let held = {
        let client = Arc::clone(&client);
        tokio::spawn(async move { client.get("/block").send().await })
    };
    wait_for_entered(&ENTERED_SHED_LOG, 1).await;

    // The second concurrent request must be shed with a 503.
    let shed = client.get("/block").send().await;
    shed.assert_status(503);
    assert!(
        shed.header("retry-after").is_some(),
        "shed response must carry Retry-After"
    );

    // Release the held request so the spawned task completes.
    GATE_SHED_LOG.notify_waiters();
    held.await.unwrap().assert_ok();

    let events = capture.captured();
    let shed_event = events
        .iter()
        .find(|e| e.field("status") == Some("503"))
        .expect("access log should contain a 503 event for the shed request");
    assert_eq!(shed_event.field("route"), Some("/block"));
}

#[tokio::test]
async fn probes_bypass_ceiling_under_saturation() {
    // See install_capture's doc: every test needs a real subscriber.
    let _guard = install_capture().1;
    let client = Arc::new(
        TestApp::new()
            .config(config_with_ceiling(1))
            .routes(routes![block_probe])
            .build(),
    );

    // Saturate the single slot.
    let held = {
        let client = Arc::clone(&client);
        tokio::spawn(async move { client.get("/block").send().await })
    };
    wait_for_entered(&ENTERED_PROBE, 1).await;

    // Probe/health/actuator routes must still succeed while the ceiling is
    // saturated — a merely-busy replica must not be killed by its
    // orchestrator (AC4).
    client.get("/live").send().await.assert_ok();
    client.get("/ready").send().await.assert_ok();
    client.get("/startup").send().await.assert_ok();
    client.get("/health").send().await.assert_ok();
    client.get("/actuator/health").send().await.assert_ok();

    // But a second ordinary request is still shed.
    client.get("/block").send().await.assert_status(503);

    GATE_PROBE.notify_waiters();
    held.await.unwrap().assert_ok();
}

#[tokio::test]
async fn prometheus_exposes_requests_shed_total() {
    // See install_capture's doc: every test needs a real subscriber.
    let _guard = install_capture().1;
    let client = Arc::new(
        TestApp::new()
            .config(config_with_ceiling(1))
            .routes(routes![block_metrics])
            .build(),
    );

    let held = {
        let client = Arc::clone(&client);
        tokio::spawn(async move { client.get("/block").send().await })
    };
    wait_for_entered(&ENTERED_METRICS, 1).await;

    client.get("/block").send().await.assert_status(503);

    let metrics = client.get("/actuator/prometheus").send().await;
    metrics.assert_ok();
    let text = metrics.text();
    assert!(text.contains("# HELP autumn_requests_shed_total"));
    assert!(text.contains("# TYPE autumn_requests_shed_total counter"));
    assert!(
        text.contains("autumn_requests_shed_total{version=\"stable\"} 1"),
        "expected exactly one shed request recorded, got:\n{text}"
    );

    GATE_METRICS.notify_waiters();
    held.await.unwrap().assert_ok();
}

// ── The ceiling can be sourced from the committed capacity contract (#1733) ──

static GATE_CONTRACT: LazyLock<Notify> = LazyLock::new(Notify::new);
static ENTERED_CONTRACT: AtomicUsize = AtomicUsize::new(0);

#[get("/block")]
async fn block_contract() -> &'static str {
    ENTERED_CONTRACT.fetch_add(1, Ordering::SeqCst);
    GATE_CONTRACT.notified().await;
    "released"
}

static GATE_STALE: LazyLock<Notify> = LazyLock::new(Notify::new);
static ENTERED_STALE: AtomicUsize = AtomicUsize::new(0);

#[get("/block")]
async fn block_stale_contract() -> &'static str {
    ENTERED_STALE.fetch_add(1, Ordering::SeqCst);
    GATE_STALE.notified().await;
    "released"
}

/// Write a contract licensing `admission_limit` on `host`, and return its path
/// (with the temp dir that owns it, which the caller must keep alive).
fn write_contract(
    admission_limit: usize,
    host: autumn_web::capacity::HostProfile,
) -> (tempfile::TempDir, std::path::PathBuf) {
    let routes = vec![autumn_web::capacity::RouteShape {
        method: "GET".to_owned(),
        path: "/block".to_owned(),
        handler: "block".to_owned(),
        shape: autumn_web::capacity::ResourceShape::ComputeBound,
        pools: Vec::new(),
    }];
    let contract = autumn_web::capacity::CapacityContract {
        version: autumn_web::capacity::CONTRACT_VERSION,
        provenance: autumn_web::capacity::Provenance {
            autumn_version: "0.7.0".to_owned(),
            calibrated_at: "2026-09-01T00:00:00Z".to_owned(),
            git_commit: None,
            git_dirty: false,
            route_graph_digest: autumn_web::capacity::route_graph_digest(&routes),
        },
        host,
        envelope: autumn_web::capacity::Envelope {
            sustained_rps: 1000.0,
            p99_latency_ms: 5.0,
            saturation_concurrency: admission_limit,
            admission_limit,
        },
        routes,
    };

    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join(autumn_web::capacity::CONTRACT_FILE_NAME);
    std::fs::write(&path, contract.to_toml().expect("serialize contract")).expect("write contract");
    (dir, path)
}

fn config_with_contract(path: &std::path::Path) -> AutumnConfig {
    let mut config = AutumnConfig {
        profile: Some("test".into()),
        ..Default::default()
    };
    config.security.csrf.enabled = false;
    // Deliberately NO `max_concurrent_requests`: the contract is the only
    // source of the ceiling in this test.
    config.server.capacity_contract = Some(path.display().to_string());
    config
}

/// AC-4: with no hand-set `max_concurrent_requests`, the binary admits against
/// the envelope its committed contract proved.
#[tokio::test]
async fn ceiling_is_sourced_from_the_committed_capacity_contract() {
    let _guard = install_capture().1;
    let (_dir, path) = write_contract(1, autumn_web::capacity::HostProfile::detect());

    let client = Arc::new(
        TestApp::new()
            .config(config_with_contract(&path))
            .routes(routes![block_contract])
            .build(),
    );

    let held = {
        let client = Arc::clone(&client);
        tokio::spawn(async move { client.get("/block").send().await })
    };
    wait_for_entered(&ENTERED_CONTRACT, 1).await;

    // The contract licensed exactly one in-flight request; the second is shed.
    // Bounded: were the ceiling ever *not* sourced from the contract, this
    // request would be admitted and block on the gate forever, turning a
    // regression into a hung CI job instead of a failing assertion.
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        client.get("/block").send().await.assert_status(503);
    })
    .await
    .expect("the second request must be shed, not admitted and left blocking");

    GATE_CONTRACT.notify_waiters();
    held.await.unwrap().assert_ok();
}

/// A contract measured on a *different* host class must never throttle this
/// process: enforcing a laptop's envelope on a bigger box is a self-inflicted
/// outage, so the runtime falls back to unlimited rather than to a ceiling.
#[tokio::test]
async fn a_contract_from_another_host_class_does_not_throttle_this_process() {
    let _guard = install_capture().1;
    let foreign = autumn_web::capacity::HostProfile {
        logical_cpus: autumn_web::capacity::HostProfile::detect()
            .logical_cpus
            .saturating_add(64),
        ..autumn_web::capacity::HostProfile::detect()
    };
    let (_dir, path) = write_contract(1, foreign);

    let client = Arc::new(
        TestApp::new()
            .config(config_with_contract(&path))
            .routes(routes![block_stale_contract])
            .build(),
    );

    let held = {
        let client = Arc::clone(&client);
        tokio::spawn(async move { client.get("/block").send().await })
    };
    wait_for_entered(&ENTERED_STALE, 1).await;

    // No ceiling was licensed, so the second concurrent request is admitted
    // (it blocks on the same gate) rather than shed.
    let second = {
        let client = Arc::clone(&client);
        tokio::spawn(async move { client.get("/block").send().await })
    };
    wait_for_entered(&ENTERED_STALE, 2).await;

    GATE_STALE.notify_waiters();
    held.await.unwrap().assert_ok();
    second.await.unwrap().assert_ok();
}

#[tokio::test]
async fn default_config_never_sheds() {
    // See install_capture's doc: every test needs a real subscriber.
    let _guard = install_capture().1;
    // No `server.max_concurrent_requests` configured — today's unlimited
    // behavior must be preserved (AC2).
    let client = TestApp::new().routes(routes![ping]).build();

    for _ in 0..25 {
        client.get("/ping").send().await.assert_ok();
    }
}

// ── /mcp envelope must also be shed (the ceiling must not have a bypass) ──

#[cfg(feature = "mcp")]
mod mcp_admission {
    use super::*;

    static GATE_MCP: LazyLock<Notify> = LazyLock::new(Notify::new);
    static ENTERED_MCP: AtomicUsize = AtomicUsize::new(0);

    #[get("/block")]
    async fn block_mcp() -> &'static str {
        ENTERED_MCP.fetch_add(1, Ordering::SeqCst);
        GATE_MCP.notified().await;
        "released"
    }

    /// The late-mounted `/mcp` envelope router is merged after
    /// `apply_middleware`, so every admission-style gate applied there
    /// (maintenance mode, rate limiting, body limit, timeout, security
    /// headers) is explicitly re-applied to it. Load shedding must be too —
    /// otherwise MCP traffic (`initialize`/`tools/list`/`tools/call`) bypasses
    /// `server.max_concurrent_requests` entirely, defeating the ceiling for
    /// that ingress surface.
    #[tokio::test]
    async fn mcp_envelope_is_shed_when_ceiling_is_saturated() {
        let client = Arc::new(
            TestApp::new()
                .config(config_with_ceiling(1))
                .routes(routes![block_mcp])
                .mount_mcp("/mcp")
                .build(),
        );

        let held = {
            let client = Arc::clone(&client);
            tokio::spawn(async move { client.get("/block").send().await })
        };
        wait_for_entered(&ENTERED_MCP, 1).await;

        // The single slot is occupied by `/block`; a `tools/list` call to the
        // MCP envelope must also be shed, not silently bypass the ceiling.
        let resp = client
            .post("/mcp")
            .json(&serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}))
            .send()
            .await;
        resp.assert_status(503);

        GATE_MCP.notify_waiters();
        held.await.unwrap().assert_ok();
    }

    #[autumn_web::api_doc(mcp, summary = "Ping (MCP double-count regression, #1577)")]
    #[get("/mcp-ping")]
    async fn mcp_ping() -> autumn_web::Json<serde_json::Value> {
        autumn_web::Json(serde_json::json!({"ok": true}))
    }

    /// A `tools/call` replays the request through the same shared
    /// `LoadShedLayer` instance the `/mcp` envelope itself is wrapped with
    /// (see `build_router_pre_state`'s `load_shed_layer`/`envelope_load_shed`
    /// wiring). Without exempting that replay, a single solo `tools/call`
    /// consumes two slots for one logical request — at `ceiling = 1` it would
    /// shed itself even with no other traffic in flight at all.
    #[tokio::test]
    async fn tools_call_does_not_double_count_against_its_own_envelope_slot() {
        let client = TestApp::new()
            .config(config_with_ceiling(1))
            .routes(routes![mcp_ping])
            .mount_mcp("/mcp")
            .build();

        let resp = client
            .post("/mcp")
            .json(&serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {"name": "mcp_ping", "arguments": {}}
            }))
            .send()
            .await;
        resp.assert_ok();
        let body: serde_json::Value = resp.json();
        assert_ne!(
            body["result"]["isError"], true,
            "a solo tools/call at ceiling=1 must not shed itself via a double-counted \
             replay; got: {body}"
        );
    }
}
