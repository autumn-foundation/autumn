//! Isolated integration test for issue #1044: `PUT /actuator/loggers/{name}`
//! must affect the **live** `tracing` subscriber, not just an in-memory map.
//!
//! This test installs the process-global tracing subscriber via the real
//! [`autumn_web::telemetry::init`] path (with the in-memory capture buffer
//! enabled so we can observe emitted events), wires the resulting reload handle
//! into a [`LogLevels`](autumn_web::actuator::LogLevels) exactly as
//! `AppBuilder` does, then asserts that level changes actually change what the
//! subscriber emits — captured **before/after**, not merely reflected in the
//! JSON response.
//!
//! It lives in its own test binary (not the consolidated `integration_tests`
//! target) because installing the global subscriber is a process-wide
//! side-effect: only one subscriber can be set per process, so this cannot
//! share a binary with other tests. See CLAUDE.md's isolated-test guidance.

use autumn_web::actuator::LogLevels;
use autumn_web::config::{LogConfig, LogFormat, TelemetryConfig};
use autumn_web::log::capture::LogCaptureConfig;

/// Count captured entries matching an exact target + message.
fn count(buffer: &autumn_web::log::capture::LogBuffer, target: &str, message: &str) -> usize {
    buffer
        .snapshot(None, None)
        .into_iter()
        .filter(|entry| entry.target == target && entry.message == message)
        .count()
}

#[test]
fn live_logger_level_changes_affect_the_subscriber() {
    // Configured level is `info`; capture buffer on so we can observe emission.
    let log = LogConfig {
        level: "info".to_owned(),
        format: LogFormat::Pretty,
        capture: LogCaptureConfig {
            enabled: true,
            capacity: 4096,
        },
        ..LogConfig::default()
    };

    let guard = autumn_web::telemetry::init(&log, &TelemetryConfig::default(), Some("test"))
        .expect("telemetry init should install the subscriber");

    // AC #7: a reload-capable subscriber must be installed on the default path.
    let reload = guard
        .filter_reload
        .clone()
        .expect("default telemetry init installs a reload handle");
    let buffer = guard
        .log_buffer
        .clone()
        .expect("capture buffer should be present when capture.enabled = true");

    // Wire the reload handle into LogLevels exactly as AppBuilder does.
    let levels = LogLevels::new(&log.level);
    levels.attach_reload_handle(reload);
    assert!(
        levels.reload_available(),
        "reload handle should be attached"
    );

    // ── Baseline: a debug event is filtered out at the configured `info`. ──
    tracing::debug!(target: "app::root_probe", "before");
    assert_eq!(
        count(&buffer, "app::root_probe", "before"),
        0,
        "debug must be filtered at the configured info level"
    );

    // ── AC #1: raise root to debug -> the debug event now emits. ──
    let _ = levels.set_logger_level("root", "debug");
    assert_eq!(levels.current_level(), "debug");
    tracing::debug!(target: "app::root_probe", "after-root-debug");
    assert!(
        count(&buffer, "app::root_probe", "after-root-debug") >= 1,
        "after raising root to debug the live subscriber must emit the debug event"
    );

    // ── AC #2: a per-target trace override raises only that target. ──
    // Global is currently `debug`; a sibling target at `debug` must NOT emit a
    // trace event, while the explicitly-raised target must.
    let _ = levels.set_logger_level("app::verbose", "trace");
    tracing::trace!(target: "app::verbose", "verbose-trace");
    tracing::trace!(target: "app::sibling", "sibling-trace");
    assert!(
        count(&buffer, "app::verbose", "verbose-trace") >= 1,
        "the per-target trace override must raise that target's verbosity"
    );
    assert_eq!(
        count(&buffer, "app::sibling", "sibling-trace"),
        0,
        "sibling targets must stay at the global level (no trace emission)"
    );

    // ── AC #3: reverting root to info silences the debug event again. ──
    let _ = levels.set_logger_level("root", "info");
    assert_eq!(levels.current_level(), "info");
    tracing::debug!(target: "app::root_probe", "post-revert-debug");
    assert_eq!(
        count(&buffer, "app::root_probe", "post-revert-debug"),
        0,
        "after reverting root to info the debug event must be silenced again"
    );

    // The per-target override survives the root revert (independent directive).
    tracing::trace!(target: "app::verbose", "verbose-trace-2");
    assert!(
        count(&buffer, "app::verbose", "verbose-trace-2") >= 1,
        "per-target trace override should persist across a root-level revert"
    );

    drop(guard);
}
