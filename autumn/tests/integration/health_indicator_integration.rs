//! Integration tests for the pluggable `HealthIndicator` trait (issue #824).
//!
//! These tests verify:
//! - The `HealthIndicator` trait can be implemented and registered
//! - `AppBuilder::health_indicator()` stores indicators
//! - Duplicate registrations are rejected
//! - The `HealthIndicatorRegistry` runs indicators with per-indicator timeouts
//! - A timed-out indicator is reported as `Unknown` with `timed_out: true`
//! - Status precedence: Down > `OutOfService` > Unknown > Up

use std::sync::Arc;
use std::time::Duration;

use autumn_web::actuator::{
    HealthCheckOutput, HealthIndicator, HealthIndicatorRegistry, HealthStatus, IndicatorGroup,
};
use autumn_web::{get, routes};

// ── helper indicators ────────────────────────────────────────────

struct AlwaysUp;
impl HealthIndicator for AlwaysUp {
    fn check(&self) -> futures::future::BoxFuture<'_, HealthCheckOutput> {
        Box::pin(async { HealthCheckOutput::up() })
    }
}

struct AlwaysDown;
impl HealthIndicator for AlwaysDown {
    fn check(&self) -> futures::future::BoxFuture<'_, HealthCheckOutput> {
        Box::pin(async { HealthCheckOutput::down() })
    }
}

struct HungIndicator;
impl HealthIndicator for HungIndicator {
    fn check(&self) -> futures::future::BoxFuture<'_, HealthCheckOutput> {
        Box::pin(async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            HealthCheckOutput::up()
        })
    }

    fn timeout_ms(&self) -> u64 {
        10
    }
}

struct HealthOnlyIndicator;
impl HealthIndicator for HealthOnlyIndicator {
    fn check(&self) -> futures::future::BoxFuture<'_, HealthCheckOutput> {
        Box::pin(async { HealthCheckOutput::up() })
    }

    fn group(&self) -> IndicatorGroup {
        IndicatorGroup::HealthOnly
    }
}

// ── trait default values ─────────────────────────────────────────

#[test]
fn health_indicator_default_timeout_is_2000ms() {
    assert_eq!(AlwaysUp.timeout_ms(), 2000);
}

#[test]
fn health_indicator_default_group_is_readiness() {
    assert!(matches!(AlwaysUp.group(), IndicatorGroup::Readiness));
}

#[test]
fn health_indicator_health_only_group_returns_health_only() {
    assert!(matches!(
        HealthOnlyIndicator.group(),
        IndicatorGroup::HealthOnly
    ));
}

// ── HealthCheckOutput helpers ────────────────────────────────────

#[test]
fn health_check_output_up_has_up_status_and_empty_details() {
    let out = HealthCheckOutput::up();
    assert_eq!(out.status, HealthStatus::Up);
    assert!(out.details.is_empty());
}

#[test]
fn health_check_output_down_has_down_status_and_empty_details() {
    let out = HealthCheckOutput::down();
    assert_eq!(out.status, HealthStatus::Down);
    assert!(out.details.is_empty());
}

// ── registry registration ────────────────────────────────────────

#[test]
fn registry_register_and_is_not_empty() {
    let registry = HealthIndicatorRegistry::new();
    assert!(registry.is_empty());
    registry
        .register("myapp", IndicatorGroup::Readiness, Arc::new(AlwaysUp))
        .unwrap();
    assert!(!registry.is_empty());
}

#[test]
fn registry_duplicate_name_is_rejected() {
    let registry = HealthIndicatorRegistry::new();
    registry
        .register("svc", IndicatorGroup::Readiness, Arc::new(AlwaysUp))
        .unwrap();
    let err = registry
        .register("svc", IndicatorGroup::Readiness, Arc::new(AlwaysUp))
        .unwrap_err();
    assert!(
        err.contains("svc"),
        "error should mention the duplicate name"
    );
}

// ── run_all ──────────────────────────────────────────────────────

#[tokio::test]
async fn registry_run_all_returns_all_results() {
    let registry = HealthIndicatorRegistry::new();
    registry
        .register("a", IndicatorGroup::Readiness, Arc::new(AlwaysUp))
        .unwrap();
    registry
        .register("b", IndicatorGroup::HealthOnly, Arc::new(AlwaysDown))
        .unwrap();

    let results = registry.run_all().await;
    assert_eq!(results.len(), 2);
}

#[tokio::test]
async fn registry_run_readiness_skips_health_only_indicators() {
    let registry = HealthIndicatorRegistry::new();
    registry
        .register(
            "readiness_check",
            IndicatorGroup::Readiness,
            Arc::new(AlwaysUp),
        )
        .unwrap();
    registry
        .register(
            "health_only_check",
            IndicatorGroup::HealthOnly,
            Arc::new(AlwaysDown),
        )
        .unwrap();

    let results = registry.run_readiness().await;
    // Only readiness-group indicators
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].name, "readiness_check");
}

#[tokio::test]
async fn hung_indicator_times_out_and_reports_unknown_with_timed_out_detail() {
    let registry = HealthIndicatorRegistry::new();
    registry
        .register("hung", IndicatorGroup::Readiness, Arc::new(HungIndicator))
        .unwrap();

    let results = registry.run_all().await;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].output.status, HealthStatus::Unknown);
    assert_eq!(
        results[0].output.details.get("timed_out"),
        Some(&serde_json::Value::Bool(true))
    );
}

// ── aggregate_status ─────────────────────────────────────────────

#[test]
fn aggregate_status_empty_is_up() {
    assert_eq!(
        HealthIndicatorRegistry::aggregate_status(&[]),
        HealthStatus::Up
    );
}

#[test]
fn aggregate_status_all_up_is_up() {
    let statuses = [HealthStatus::Up, HealthStatus::Up];
    assert_eq!(
        HealthIndicatorRegistry::aggregate_status(&statuses),
        HealthStatus::Up
    );
}

#[test]
fn aggregate_status_unknown_beats_up() {
    let statuses = [HealthStatus::Up, HealthStatus::Unknown];
    assert_eq!(
        HealthIndicatorRegistry::aggregate_status(&statuses),
        HealthStatus::Unknown
    );
}

#[test]
fn aggregate_status_out_of_service_beats_unknown() {
    let statuses = [HealthStatus::Unknown, HealthStatus::OutOfService];
    assert_eq!(
        HealthIndicatorRegistry::aggregate_status(&statuses),
        HealthStatus::OutOfService
    );
}

#[test]
fn aggregate_status_down_beats_out_of_service() {
    let statuses = [HealthStatus::OutOfService, HealthStatus::Down];
    assert_eq!(
        HealthIndicatorRegistry::aggregate_status(&statuses),
        HealthStatus::Down
    );
}

#[test]
fn aggregate_status_down_beats_everything() {
    let statuses = [
        HealthStatus::Up,
        HealthStatus::Unknown,
        HealthStatus::OutOfService,
        HealthStatus::Down,
    ];
    assert_eq!(
        HealthIndicatorRegistry::aggregate_status(&statuses),
        HealthStatus::Down
    );
}

// ── AppBuilder integration ───────────────────────────────────────

#[get("/ping")]
async fn ping_handler() -> &'static str {
    "pong"
}

#[test]
fn app_builder_accepts_health_indicator() {
    let _builder = autumn_web::app()
        .routes(routes![ping_handler])
        .health_indicator("mycheck", Arc::new(AlwaysUp));
}

#[test]
fn app_builder_health_indicator_duplicate_name_does_not_panic() {
    // Builder silently drops the duplicate (warns via tracing)
    let _builder = autumn_web::app()
        .routes(routes![ping_handler])
        .health_indicator("dup", Arc::new(AlwaysUp))
        .health_indicator("dup", Arc::new(AlwaysUp));
}

// ── HealthStatus serialization ───────────────────────────────────

#[test]
fn health_status_serializes_to_spring_boot_values() {
    assert_eq!(serde_json::to_string(&HealthStatus::Up).unwrap(), "\"UP\"");
    assert_eq!(
        serde_json::to_string(&HealthStatus::Down).unwrap(),
        "\"DOWN\""
    );
    assert_eq!(
        serde_json::to_string(&HealthStatus::OutOfService).unwrap(),
        "\"OUT_OF_SERVICE\""
    );
    assert_eq!(
        serde_json::to_string(&HealthStatus::Unknown).unwrap(),
        "\"UNKNOWN\""
    );
}

#[test]
fn health_status_is_healthy_for_up_and_unknown() {
    assert!(HealthStatus::Up.is_healthy());
    assert!(HealthStatus::Unknown.is_healthy());
    assert!(!HealthStatus::Down.is_healthy());
    assert!(!HealthStatus::OutOfService.is_healthy());
}
