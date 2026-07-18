//! Integration tests for the `TimeZone` extractor and Maud view helpers.
//!
//! Demonstrates that `Clock` and `TimeZone` compose deterministically in
//! `TestApp`, letting tests drive three zones with zero sleeps.

use autumn_web::prelude::*;
use autumn_web::test::TestApp;
use autumn_web::time::FixedClock;
use autumn_web::time_zone::{TimeZone, local_datetime};
use chrono::{TimeZone as ChrTz, Utc};

// ── Handler under test ────────────────────────────────────────────────────────

/// Renders a fixed UTC timestamp as local time in the request's zone.
#[get("/local")]
async fn show_local(clock: Clock, tz: TimeZone) -> Markup {
    html! { p { (local_datetime(clock.now(), *tz)) } }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// Three zones → three distinct, correctly formatted bodies from one handler.
#[tokio::test]
async fn three_zones_produce_distinct_local_times() {
    let pinned = Utc.with_ymd_and_hms(2025, 6, 14, 12, 0, 0).unwrap();
    let client = TestApp::new()
        .routes(routes![show_local])
        .with_clock(FixedClock::at(pinned))
        .build();

    let utc_body = client.get("/local?tz=UTC").send().await.assert_ok().text();
    let ny_body = client
        .get("/local?tz=America/New_York")
        .send()
        .await
        .assert_ok()
        .text();
    let tok_body = client
        .get("/local?tz=Asia/Tokyo")
        .send()
        .await
        .assert_ok()
        .text();

    // UTC: 12:00 UTC
    assert!(utc_body.contains("12:00"), "UTC body: {utc_body}");
    // New York: UTC-4 in June (EDT) → 08:00
    assert!(ny_body.contains("08:00"), "New York body: {ny_body}");
    // Tokyo: UTC+9 → 21:00
    assert!(tok_body.contains("21:00"), "Tokyo body: {tok_body}");

    // All three bodies differ
    assert_ne!(utc_body, ny_body);
    assert_ne!(utc_body, tok_body);
    assert_ne!(ny_body, tok_body);
}

/// `Clock` and `TimeZone` compose: both are deterministic and independent.
#[tokio::test]
async fn clock_and_time_zone_compose_deterministically() {
    let pinned = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
    let client = TestApp::new()
        .routes(routes![show_local])
        .with_clock(FixedClock::at(pinned))
        .build();

    // Tokyo is UTC+9, so midnight UTC = 09:00 Tokyo on Jan 1
    let body = client
        .get("/local?tz=Asia/Tokyo")
        .send()
        .await
        .assert_ok()
        .text();
    assert!(body.contains("09:00"), "body: {body}");
    // Machine-readable attr must still be UTC
    assert!(body.contains("2025-01-01T00:00:00"), "body: {body}");
}

/// Without any zone override, the extractor falls back to UTC (default config).
#[tokio::test]
async fn falls_back_to_utc_when_no_zone_provided() {
    let pinned = Utc.with_ymd_and_hms(2025, 6, 14, 15, 30, 0).unwrap();
    let client = TestApp::new()
        .routes(routes![show_local])
        .with_clock(FixedClock::at(pinned))
        .build();

    let body = client.get("/local").send().await.assert_ok().text();
    assert!(body.contains("15:30"), "UTC fallback body: {body}");
}
