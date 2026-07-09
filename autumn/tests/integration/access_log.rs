//! Integration tests for the default structured per-request access log (#999).
//!
//! Acceptance criteria covered:
//! - exactly one structured access-log event per served request, emitted at
//!   the response boundary with default configuration and no telemetry feature
//! - the event carries method, the matched low-cardinality route template,
//!   status, `duration_ms`, and the `request_id` that matches `x-request-id`
//! - `/health`, `/actuator/*`, and `/static/*` are excluded by default and the
//!   exclusion set is configurable
//! - access logging is on by default and can be disabled via `log.access_log`
//! - query strings never leak into the event
//! - under `LogFormat::Json` the event renders as a single JSON object line

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use autumn_web::config::AutumnConfig;
use autumn_web::test::TestApp;
use autumn_web::{get, routes};
use tracing_subscriber::layer::SubscriberExt as _;

/// `tracing` target carried by every access-log event.
const ACCESS_TARGET: &str = "autumn::access";

// ── Capture infrastructure ─────────────────────────────────────

#[derive(Debug, Clone, Default)]
struct CapturedEvent {
    fields: BTreeMap<String, String>,
}

impl CapturedEvent {
    fn field(&self, name: &str) -> Option<&str> {
        self.fields.get(name).map(String::as_str)
    }
}

/// Test tracing layer that records every access-log event's fields.
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
    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.0.insert(field.name().to_owned(), value.to_string());
    }
    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
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

/// Install a capture layer as the thread-default subscriber. Tests run on a
/// current-thread tokio runtime, so everything the request emits is observed.
fn install_capture() -> (AccessLogCapture, tracing::subscriber::DefaultGuard) {
    let capture = AccessLogCapture::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    let guard = tracing::subscriber::set_default(subscriber);
    (capture, guard)
}

fn test_config() -> AutumnConfig {
    // Mirror TestApp::new() defaults so `.config()` overrides stay additive.
    let mut config = AutumnConfig {
        profile: Some("test".into()),
        ..Default::default()
    };
    config.security.csrf.enabled = false;
    config
}

// ── Sample handlers ────────────────────────────────────────────

#[get("/users/{id}")]
async fn show_user(axum::extract::Path(id): axum::extract::Path<i64>) -> String {
    format!("user {id}")
}

#[get("/boom")]
async fn boom() -> Result<String, autumn_web::error::AutumnError> {
    Err(autumn_web::error::AutumnError::not_found_msg("gone"))
}

// ── Tests ──────────────────────────────────────────────────────

#[tokio::test]
async fn emits_exactly_one_access_event_with_status_request_id_and_duration() {
    let (capture, _guard) = install_capture();
    let client = TestApp::new().routes(routes![show_user]).build();

    let response = client.get("/users/123").send().await;
    response.assert_ok();
    let request_id = response
        .header("x-request-id")
        .expect("x-request-id header should be set")
        .to_owned();

    let events = capture.captured();
    assert_eq!(
        events.len(),
        1,
        "expected exactly one access-log event, got {events:?}"
    );
    let event = &events[0];
    assert_eq!(event.field("method"), Some("GET"));
    assert_eq!(
        event.field("route"),
        Some("/users/{id}"),
        "route must be the matched low-cardinality template"
    );
    assert_eq!(event.field("status"), Some("200"));
    assert_eq!(
        event.field("request_id"),
        Some(request_id.as_str()),
        "access line must correlate with the x-request-id header"
    );
    let duration: f64 = event
        .field("duration_ms")
        .expect("duration_ms field should be present")
        .parse()
        .expect("duration_ms should be numeric");
    assert!(duration >= 0.0);
}

#[tokio::test]
async fn error_responses_are_logged_with_their_status() {
    let (capture, _guard) = install_capture();
    let client = TestApp::new().routes(routes![boom]).build();

    client.get("/boom").send().await;

    let events = capture.captured();
    assert_eq!(events.len(), 1, "got {events:?}");
    assert_eq!(events[0].field("status"), Some("404"));
    assert_eq!(events[0].field("route"), Some("/boom"));
}

#[tokio::test]
async fn route_field_never_contains_the_raw_path() {
    let (capture, _guard) = install_capture();
    let client = TestApp::new().routes(routes![show_user]).build();

    client.get("/users/4242").send().await.assert_ok();

    let events = capture.captured();
    assert_eq!(events.len(), 1);
    for (key, value) in &events[0].fields {
        assert!(
            !value.contains("4242"),
            "field {key} leaked the raw path parameter: {value}"
        );
    }
}

#[tokio::test]
async fn unmatched_requests_log_a_low_cardinality_placeholder() {
    let (capture, _guard) = install_capture();
    let client = TestApp::new().routes(routes![show_user]).build();

    client.get("/no/such/route").send().await;

    let events = capture.captured();
    assert_eq!(events.len(), 1, "got {events:?}");
    assert_eq!(events[0].field("status"), Some("404"));
    assert_eq!(events[0].field("route"), Some("_unmatched"));
}

#[tokio::test]
async fn health_actuator_and_static_are_excluded_by_default() {
    let (capture, _guard) = install_capture();
    let client = TestApp::new().routes(routes![show_user]).build();

    client.get("/health").send().await.assert_ok();
    client.get("/live").send().await.assert_ok();
    client.get("/ready").send().await.assert_ok();
    client.get("/startup").send().await.assert_ok();
    client.get("/actuator/health").send().await;
    client.get("/static/app.css").send().await;
    assert!(
        capture.captured().is_empty(),
        "probe/asset traffic should not be access-logged by default: {:?}",
        capture.captured()
    );

    // Ordinary application traffic is still logged.
    client.get("/users/1").send().await.assert_ok();
    assert_eq!(capture.captured().len(), 1);
}

#[tokio::test]
async fn exclusion_set_is_configurable() {
    let (capture, _guard) = install_capture();
    let mut config = test_config();
    config.log.access_log_exclude = vec!["/users".to_owned()];
    let client = TestApp::new()
        .config(config)
        .routes(routes![show_user])
        .build();

    // The configured exclusion replaces the default set entirely.
    client.get("/users/1").send().await.assert_ok();
    assert!(capture.captured().is_empty());

    client.get("/health").send().await.assert_ok();
    assert_eq!(
        capture.captured().len(),
        1,
        "/health should be logged once the default exclusions are replaced"
    );
}

#[tokio::test]
async fn access_log_can_be_disabled_via_config() {
    let (capture, _guard) = install_capture();
    let mut config = test_config();
    config.log.access_log = false;
    let client = TestApp::new()
        .config(config)
        .routes(routes![show_user])
        .build();

    client.get("/users/1").send().await.assert_ok();
    assert!(
        capture.captured().is_empty(),
        "log.access_log = false must silence the access log"
    );
}

#[tokio::test]
async fn query_strings_never_appear_in_the_access_event() {
    let (capture, _guard) = install_capture();
    let client = TestApp::new().routes(routes![show_user]).build();

    client
        .get("/users/1?token=supersecret&password=hunter2")
        .send()
        .await
        .assert_ok();

    let events = capture.captured();
    assert_eq!(events.len(), 1);
    for (key, value) in &events[0].fields {
        assert!(
            !value.contains("supersecret") && !value.contains("hunter2"),
            "field {key} leaked query-string data: {value}"
        );
    }
}

/// Success-metric probe (#999): added per-request overhead < 50 µs p99.
///
/// Ignored by default because wall-clock assertions are environment-sensitive;
/// run manually with `cargo test --test access_log --release -- --ignored`.
#[tokio::test]
#[ignore = "timing-sensitive; run manually in release mode"]
async fn per_request_overhead_is_under_50_microseconds_p99() {
    use autumn_web::middleware::AccessLogLayer;
    use tower::ServiceExt as _;

    async fn handler() -> &'static str {
        "ok"
    }

    async fn p99_nanos(router: &axum::Router) -> u128 {
        const N: usize = 5_000;
        let mut samples = Vec::with_capacity(N);
        // Warm-up.
        for _ in 0..500 {
            let req = axum::http::Request::builder()
                .uri("/ping")
                .body(axum::body::Body::empty())
                .unwrap();
            router.clone().oneshot(req).await.unwrap();
        }
        for _ in 0..N {
            let req = axum::http::Request::builder()
                .uri("/ping")
                .body(axum::body::Body::empty())
                .unwrap();
            let start = std::time::Instant::now();
            router.clone().oneshot(req).await.unwrap();
            samples.push(start.elapsed().as_nanos());
        }
        samples.sort_unstable();
        samples[N * 99 / 100]
    }

    // Discard formatted output so we measure emission, not test-buffer growth.
    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .json()
            .with_writer(std::io::sink),
    );
    let _guard = tracing::subscriber::set_default(subscriber);

    let bare = axum::Router::new().route("/ping", axum::routing::get(handler));
    let logged = bare
        .clone()
        .layer(AccessLogLayer::new(vec!["/health".to_owned()]));

    let baseline = p99_nanos(&bare).await;
    let with_access_log = p99_nanos(&logged).await;
    let overhead_ns = with_access_log.saturating_sub(baseline);
    println!(
        "p99 baseline: {baseline} ns, with access log: {with_access_log} ns, \
         overhead: {overhead_ns} ns"
    );
    assert!(
        overhead_ns < 50_000,
        "access-log p99 overhead {overhead_ns} ns exceeds the 50 µs budget"
    );
}

#[tokio::test]
async fn json_format_renders_the_event_as_a_single_json_object_line() {
    #[derive(Clone)]
    struct BufWriter(Arc<Mutex<Vec<u8>>>);
    struct BufGuard(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for BufGuard {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for BufWriter {
        type Writer = BufGuard;
        fn make_writer(&'a self) -> Self::Writer {
            BufGuard(Arc::clone(&self.0))
        }
    }

    let buf = Arc::new(Mutex::new(Vec::new()));
    // Same shape as the framework's LogFormat::Json subscriber.
    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .json()
            .with_writer(BufWriter(Arc::clone(&buf))),
    );
    let _guard = tracing::subscriber::set_default(subscriber);

    let client = TestApp::new().routes(routes![show_user]).build();
    client.get("/users/7").send().await.assert_ok();

    let out = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
    let access_lines: Vec<&str> = out
        .lines()
        .filter(|line| line.contains(ACCESS_TARGET))
        .collect();
    assert_eq!(
        access_lines.len(),
        1,
        "expected one access-log line, got: {out}"
    );
    let value: serde_json::Value =
        serde_json::from_str(access_lines[0]).expect("access line should be one JSON object");
    assert_eq!(value["target"], ACCESS_TARGET);
    let fields = &value["fields"];
    assert_eq!(fields["method"], "GET");
    assert_eq!(fields["route"], "/users/{id}");
    assert_eq!(fields["status"], 200);
    assert!(fields["duration_ms"].is_number());
    assert!(fields["request_id"].is_string());
}

#[tokio::test]
async fn filter_rewrites_log_handler_status_without_double_logging() {
    use autumn_web::middleware::{
        AccessLogLayer, AutumnErrorInfo, ExceptionFilter, ExceptionFilterLayer,
    };
    use axum::response::IntoResponse as _;
    use tower::ServiceExt as _;

    // A filter that replaces the handler's error response entirely, like the
    // documented `filter_can_replace_response` capability.
    struct Rewrite;
    impl ExceptionFilter for Rewrite {
        fn filter(
            &self,
            _error: &AutumnErrorInfo,
            _response: axum::response::Response,
        ) -> axum::response::Response {
            (axum::http::StatusCode::SERVICE_UNAVAILABLE, "down").into_response()
        }
    }

    let (capture, _guard) = install_capture();
    // Production ordering: the primary access log is INNER to the
    // exception-filter chain (in-span, correlated), with the fallback
    // outermost. The `AccessLogEmitted` marker must keep the fallback silent,
    // and the logged status is the handler's response — the documented,
    // accepted tradeoff of the in-context primary placement.
    let app = axum::Router::new()
        .route(
            "/boom",
            axum::routing::get(|| async {
                Err::<String, _>(autumn_web::error::AutumnError::not_found_msg("gone"))
            }),
        )
        .layer(AccessLogLayer::new(Vec::new()))
        .layer(ExceptionFilterLayer::new(vec![Arc::new(Rewrite)]))
        .layer(AccessLogLayer::fallback(Vec::new()));

    let response = app
        .oneshot(
            axum::http::Request::builder()
                .uri("/boom")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    );

    let events = capture.captured();
    assert_eq!(
        events.len(),
        1,
        "the marker must keep the fallback silent: {events:?}"
    );
    assert_eq!(
        events[0].field("status"),
        Some("404"),
        "primary placement logs the handler status (accepted tradeoff)"
    );
}

/// A response produced by a layer OUTER to the primary access log (here: a
/// short-circuiting middleware standing in for a session-store outage 503)
/// carries no `AccessLogEmitted` marker, so the outermost fallback logs it.
#[tokio::test]
async fn short_circuits_above_the_primary_are_logged_by_the_fallback() {
    use autumn_web::middleware::AccessLogLayer;
    use axum::response::IntoResponse as _;
    use tower::ServiceExt as _;

    let (capture, _guard) = install_capture();
    let app = axum::Router::new()
        .route("/ok", axum::routing::get(|| async { "ok" }))
        .layer(AccessLogLayer::new(Vec::new()))
        // Stands in for SessionLayer's store.load failure path: rejects
        // without calling the inner service (and the primary layer).
        .layer(axum::middleware::from_fn(
            |_req: axum::extract::Request, _next: axum::middleware::Next| async {
                (axum::http::StatusCode::SERVICE_UNAVAILABLE, "store down").into_response()
            },
        ))
        .layer(AccessLogLayer::fallback(Vec::new()));

    let response = app
        .oneshot(
            axum::http::Request::builder()
                .uri("/ok")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    );

    let events = capture.captured();
    assert_eq!(events.len(), 1, "got {events:?}");
    assert_eq!(events[0].field("status"), Some("503"));
    assert!(
        events[0].field("request_id").is_none(),
        "short-circuit never ran RequestIdLayer, so no request id"
    );
}

#[tokio::test]
async fn method_mismatch_405_is_access_logged() {
    let (capture, _guard) = install_capture();
    let client = TestApp::new().routes(routes![show_user]).build();

    // show_user is GET-only; POST to the same path should 405.
    let resp = client.post("/users/1").send().await;
    resp.assert_status(405);

    let events = capture.captured();
    assert_eq!(events.len(), 1, "got {events:?}");
    assert_eq!(events[0].field("status"), Some("405"));
    assert_eq!(events[0].field("method"), Some("POST"));
}
