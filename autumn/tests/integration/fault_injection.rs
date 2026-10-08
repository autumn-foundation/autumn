//! Staging fault injection (issue #3071).
//!
//! The `[fault_injection]` section adds latency and errors to routes and
//! dependencies. It is refused in `prod` unless `allow_in_production = true`.
//! A burn-rate stop condition disables it. Each toggle writes an audit event.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use autumn_web::audit::{AuditError, AuditEvent, AuditLogger, AuditSink};
use autumn_web::config::AutumnConfig;
use autumn_web::fault_injection::{
    FaultInjection, FaultInjectionConfig, FaultKind, FaultRule, FaultTarget,
};
use autumn_web::prelude::*;
use autumn_web::test::{TestApp, TestClient};

#[get("/api/orders")]
async fn orders() -> &'static str {
    "orders"
}

#[get("/other")]
async fn other() -> &'static str {
    "other"
}

/// Sends one outbound call through the HTTP client.
#[cfg(feature = "http-client")]
#[get("/api/call")]
async fn call() -> String {
    match autumn_web::http_client::Client::new()
        .get("http://127.0.0.1:9/unreachable")
        .send()
        .await
    {
        Ok(_) => "sent".to_owned(),
        Err(error) => error.to_string(),
    }
}

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<AuditEvent>>>);

impl AuditSink for Captured {
    fn write(
        &self,
        event: AuditEvent,
    ) -> Pin<Box<dyn Future<Output = Result<(), AuditError>> + Send + '_>> {
        self.0.lock().unwrap().push(event);
        Box::pin(async { Ok(()) })
    }
}

impl Captured {
    fn actions(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .map(|event| event.action.clone())
            .collect()
    }
}

fn rule(target: FaultTarget, kind: FaultKind, rate: f64) -> FaultRule {
    let mut rule = FaultRule::new(target, kind, rate);
    rule.routes = vec!["/api/*".to_owned()];
    rule
}

fn config_with(faults: Vec<FaultRule>) -> AutumnConfig {
    let mut config = AutumnConfig::default();
    config.fault_injection = FaultInjectionConfig::default();
    config.fault_injection.enabled = true;
    config.fault_injection.faults = faults;
    config
}

fn build(config: AutumnConfig, profile: &str, audit: &Captured) -> TestClient {
    let sink = audit.clone();
    #[cfg(feature = "http-client")]
    let routes = routes![orders, other, call];
    #[cfg(not(feature = "http-client"))]
    let routes = routes![orders, other];
    TestApp::new()
        .config(config)
        .profile(profile)
        .routes(routes)
        .state_initializer(move |state| {
            state.insert_extension(AuditLogger::new().with_sink(Arc::new(sink)));
        })
        .build()
}

fn handle(client: &TestClient) -> Option<FaultInjection> {
    client
        .state()
        .extension::<FaultInjection>()
        .map(|handle| (*handle).clone())
}

/// Let spawned audit writes run.
async fn settle() {
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
}

#[test]
fn config_validation_refuses_prod_by_default() {
    let mut config = config_with(vec![rule(FaultTarget::Route, FaultKind::Error, 1.0)]);
    config.profile = Some("prod".to_owned());
    let error = config.validate().expect_err("prod must refuse fault injection");
    assert!(
        error.to_string().contains("allow_in_production"),
        "the error names the override: {error}"
    );

    config.fault_injection.allow_in_production = true;
    config.validate().expect("the override allows it");
}

#[test]
fn config_validation_accepts_staging() {
    let mut config = config_with(vec![rule(FaultTarget::Route, FaultKind::Error, 1.0)]);
    config.profile = Some("staging".to_owned());
    config.validate().expect("staging may inject faults");
}

#[tokio::test]
async fn prod_profile_does_not_install_faults_by_default() {
    let audit = Captured::default();
    let config = config_with(vec![rule(FaultTarget::Route, FaultKind::Error, 1.0)]);
    let client = build(config, "prod", &audit);

    assert!(handle(&client).is_none(), "no handle in prod");
    // The prod host policy can refuse the test host; only the fault matters.
    let response = client.get("/api/orders").send().await;
    assert_ne!(response.status.as_u16(), 503);
    assert_eq!(response.header("x-autumn-fault"), None);
}

#[tokio::test]
async fn prod_profile_installs_faults_with_the_override() {
    let audit = Captured::default();
    let mut config = config_with(vec![rule(FaultTarget::Route, FaultKind::Error, 1.0)]);
    config.fault_injection.allow_in_production = true;
    let client = build(config, "prod", &audit);

    assert!(handle(&client).is_some());
    client.get("/api/orders").send().await.assert_status(503);
}

#[tokio::test]
async fn error_fault_hits_only_matched_routes() {
    let audit = Captured::default();
    let config = config_with(vec![rule(FaultTarget::Route, FaultKind::Error, 1.0)]);
    let client = build(config, "staging", &audit);

    let response = client.get("/api/orders").send().await;
    response.assert_status(503);
    assert_eq!(response.header("x-autumn-fault"), Some("injected"));
    client.get("/other").send().await.assert_ok();
}

#[tokio::test]
async fn probe_paths_are_never_faulted() {
    let audit = Captured::default();
    let mut fault = rule(FaultTarget::Route, FaultKind::Error, 1.0);
    fault.routes = Vec::new(); // Every route.
    let client = build(config_with(vec![fault]), "staging", &audit);

    client.get("/other").send().await.assert_status(503);
    let live = client.get("/live").send().await;
    assert_ne!(live.status.as_u16(), 503, "the liveness probe is exempt");
}

#[tokio::test(start_paused = true)]
async fn latency_fault_delays_the_response() {
    let audit = Captured::default();
    let mut fault = rule(FaultTarget::Route, FaultKind::Latency, 1.0);
    fault.latency_ms = 250;
    let client = build(config_with(vec![fault]), "staging", &audit);

    let started = tokio::time::Instant::now();
    client.get("/api/orders").send().await.assert_ok();
    assert!(started.elapsed() >= std::time::Duration::from_millis(250));

    let started = tokio::time::Instant::now();
    client.get("/other").send().await.assert_ok();
    assert!(started.elapsed() < std::time::Duration::from_millis(250));
}

#[tokio::test]
async fn stop_condition_disables_injection_and_audits_it() {
    let audit = Captured::default();
    let mut config = config_with(vec![rule(FaultTarget::Route, FaultKind::Error, 1.0)]);
    config.fault_injection.stop.min_requests = 5;
    let client = build(config, "staging", &audit);
    let faults = handle(&client).expect("installed in staging");
    assert!(faults.is_armed());

    for _ in 0..5 {
        client.get("/api/orders").send().await.assert_status(503);
    }
    settle().await;

    assert!(!faults.is_armed(), "the burn rate stopped injection");
    client.get("/api/orders").send().await.assert_ok();
    assert_eq!(
        audit.actions(),
        ["fault_injection.armed", "fault_injection.disarmed"],
        "boot and stop are both audited"
    );
}

#[tokio::test]
async fn stop_condition_waits_for_min_requests() {
    let audit = Captured::default();
    let mut config = config_with(vec![rule(FaultTarget::Route, FaultKind::Error, 1.0)]);
    config.fault_injection.stop.min_requests = 50;
    let client = build(config, "staging", &audit);

    for _ in 0..10 {
        client.get("/api/orders").send().await.assert_status(503);
    }
    assert!(handle(&client).unwrap().is_armed());
}

#[tokio::test]
async fn manual_toggles_are_audited() {
    let audit = Captured::default();
    let config = config_with(vec![rule(FaultTarget::Route, FaultKind::Error, 1.0)]);
    let client = build(config, "staging", &audit);
    let faults = handle(&client).unwrap();

    faults.disarm("ops@example.com", "game day over").await;
    client.get("/api/orders").send().await.assert_ok();
    faults.arm("ops@example.com").await;
    client.get("/api/orders").send().await.assert_status(503);
    settle().await;

    assert_eq!(
        audit.actions(),
        [
            "fault_injection.armed",
            "fault_injection.disarmed",
            "fault_injection.armed"
        ]
    );
    assert!(faults.snapshot().injected >= 1);
}

#[cfg(feature = "http-client")]
#[tokio::test]
async fn http_dependency_fault_fails_the_outbound_call() {
    let audit = Captured::default();
    let config = config_with(vec![rule(FaultTarget::Http, FaultKind::Error, 1.0)]);
    let client = build(config, "staging", &audit);

    let response = client.get("/api/call").send().await;
    response.assert_ok();
    assert!(
        response.text().contains("fault injection"),
        "the call fails with an injected error: {}",
        response.text()
    );
}

#[tokio::test]
async fn disabled_section_installs_nothing() {
    let audit = Captured::default();
    let mut config = config_with(vec![rule(FaultTarget::Route, FaultKind::Error, 1.0)]);
    config.fault_injection.enabled = false;
    let client = build(config, "staging", &audit);

    assert!(handle(&client).is_none());
    client.get("/api/orders").send().await.assert_ok();
    settle().await;
    assert!(audit.actions().is_empty());
}
