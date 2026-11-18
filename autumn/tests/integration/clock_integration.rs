//! Integration tests for the injectable `Clock` extractor.
//!
//! These tests demonstrate deterministic time control via `TestApp::with_clock`
//! and `TestClient::advance_clock` — no `sleep`, no Tokio timer games.

use autumn_web::prelude::*;
use autumn_web::test::TestApp;
use autumn_web::time::{Clock, FixedClock, SignedDuration, TickingClock, Timestamp};
use std::time::Duration;

// ── Handlers under test ───────────────────────────────────────────────────────

/// Returns the framework clock's current UTC timestamp as text.
#[get("/now")]
async fn current_time(clock: Clock) -> String {
    autumn_web::time::compat::rfc3339(clock.now())
}

/// Pretends a token issued at a fixed date expires after 30 days.
/// Returns 200 while valid, 401 after expiry.
#[get("/token-check")]
async fn token_check(clock: Clock) -> axum::http::StatusCode {
    let issued_at = ts("2025-01-01T00:00:00Z");
    let expires_at = issued_at + SignedDuration::from_hours(30 * 24);
    if clock.now() < expires_at {
        axum::http::StatusCode::OK
    } else {
        axum::http::StatusCode::UNAUTHORIZED
    }
}

/// Parses an RFC 3339 instant, as the `/now` handler writes it.
fn ts(text: &str) -> Timestamp {
    text.parse().expect("valid RFC 3339")
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// AC-2: Clock extractor works in handlers without any customisation.
#[tokio::test]
async fn clock_extractor_works_with_default_real_clock() {
    let client = TestApp::new().routes(routes![current_time]).build();

    let resp = client.get("/now").send().await;
    resp.assert_ok();
    // Body is a valid RFC 3339 timestamp — just check it parses.
    let body = resp.text();
    body.parse::<Timestamp>()
        .expect("should be valid RFC 3339 from SystemClock");
}

/// AC-3: `TestApp::with_clock(FixedClock::at(...))` pins the observable time.
#[tokio::test]
async fn fixed_clock_pins_time() {
    let pinned = ts("2025-06-15T12:00:00Z");

    let client = TestApp::new()
        .routes(routes![current_time])
        .with_clock(FixedClock::at(pinned))
        .build();

    let body = client.get("/now").send().await.assert_ok().text();
    assert_eq!(ts(&body), pinned);
}

/// AC-3 + AC-4: `TickingClock` starts at a given time and `advance_clock`
/// steps it forward between requests.
#[tokio::test]
async fn ticking_clock_advances_between_requests() {
    let start = ts("2025-01-01T00:00:00Z");
    let clock = TickingClock::starting_at(start);

    let client = TestApp::new()
        .routes(routes![current_time])
        .with_clock(clock.clone())
        .build();

    // First request: at start time.
    let body = client.get("/now").send().await.assert_ok().text();
    let t1 = ts(&body);
    assert_eq!(t1, start);

    // Advance 1 hour — no sleep.
    client.advance_clock(Duration::from_secs(3600));

    let body = client.get("/now").send().await.assert_ok().text();
    let t2 = ts(&body);
    assert_eq!(t2, start + SignedDuration::from_hours(1));
}

/// AC-7 (doctest-equivalent): A token-expiry test that advances 30 days and
/// asserts a 401 — under 15 lines, no `sleep`, no Tokio time games.
#[tokio::test]
async fn token_expires_after_30_days_no_sleep() {
    let issued_at = ts("2025-01-01T00:00:00Z");
    let client = TestApp::new()
        .routes(routes![token_check])
        .with_clock(TickingClock::starting_at(issued_at))
        .build();

    // Token is valid right after issue.
    client.get("/token-check").send().await.assert_status(200);

    // Advance just under the 30-day window — still valid.
    client.advance_clock(Duration::from_hours(29 * 24));
    client.get("/token-check").send().await.assert_status(200);

    // Advance past expiry — should get 401.
    client.advance_clock(Duration::from_hours(48));
    client.get("/token-check").send().await.assert_status(401);
}

/// AC-6: Apps that never opt in keep working with the real clock.
#[tokio::test]
async fn app_without_custom_clock_uses_real_wall_clock() {
    // No `.with_clock(...)` call — real SystemClock is the default.
    let client = TestApp::new().routes(routes![current_time]).build();
    let resp = client.get("/now").send().await;
    resp.assert_ok();
    // Just verify the body is a parseable timestamp close to now.
    let body = resp.text();
    let got = ts(&body);
    let diff = Timestamp::now().duration_since(got).as_secs().abs();
    assert!(diff < 5, "system clock diff should be < 5s, got {diff}s");
}

/// AC-4: `advance_clock` is a no-op when the app uses a `FixedClock`
/// (`FixedClock` cannot be advanced — that's by design).
#[tokio::test]
async fn advance_clock_with_fixed_clock_is_noop() {
    let pinned = ts("2025-03-01T00:00:00Z");
    let client = TestApp::new()
        .routes(routes![current_time])
        .with_clock(FixedClock::at(pinned))
        .build();

    // advance_clock should not panic even for a FixedClock
    client.advance_clock(Duration::from_hours(24));

    // Time is still pinned — FixedClock ignores advance.
    let body = client.get("/now").send().await.assert_ok().text();
    assert_eq!(ts(&body), pinned);
}

/// AC-5 (scheduler): Scheduler tick keys are deterministic when the clock is fixed.
#[tokio::test]
async fn scheduler_tick_key_is_deterministic_with_clock() {
    use autumn_web::time::{FixedClock, clock_unix_secs};

    let clock = FixedClock::at(ts("2024-03-15T10:30:00Z"));

    let secs = clock_unix_secs(&clock);
    // Same clock → same tick key, regardless of real wall time.
    let tick_key = autumn_web::scheduler::fixed_delay_tick_key(
        "my_task",
        Duration::from_secs(60),
        Duration::from_secs(secs),
    );
    let tick_key2 = autumn_web::scheduler::fixed_delay_tick_key(
        "my_task",
        Duration::from_secs(60),
        Duration::from_secs(clock_unix_secs(&clock)),
    );
    assert_eq!(tick_key, tick_key2, "same clock time → same tick key");

    // Advancing by less than one interval (60s) should keep the same bucket.
    let tick_key_before_flip = autumn_web::scheduler::fixed_delay_tick_key(
        "my_task",
        Duration::from_secs(60),
        Duration::from_secs(secs + 59),
    );
    assert_eq!(
        tick_key, tick_key_before_flip,
        "within interval → same bucket"
    );

    // Advancing by exactly one interval flips to the next bucket.
    let tick_key_after_flip = autumn_web::scheduler::fixed_delay_tick_key(
        "my_task",
        Duration::from_secs(60),
        Duration::from_secs(secs + 60),
    );
    assert_ne!(
        tick_key, tick_key_after_flip,
        "one interval later → new bucket"
    );
}

/// AC-5 (signed URL): `verify_with_now` uses the injected clock's unix time.
///
/// Demonstrates deterministic signed-URL expiry checking without wall-clock
/// dependency via the clock-injectable `verify_with_now` helper.
#[cfg(feature = "storage")]
#[tokio::test]
async fn signed_url_expiry_is_deterministic_with_clock() {
    use autumn_web::storage::local::{SigningKey, sign, verify_with_now};
    use autumn_web::time::{TickingClock, clock_unix_secs};

    let key = SigningKey::new(b"test-signing-key-32-bytes-long!!".to_vec());
    let blob_key = "avatars/me.png";

    // Set a fixed start time and sign a URL that expires in 300 seconds.
    let start: autumn_web::time::Timestamp = "2025-01-01T00:00:00Z".parse().unwrap();
    let clock = TickingClock::starting_at(start);

    let now_unix = clock_unix_secs(&clock);
    let expires_at = now_unix + 300;
    let signature = sign(key.as_bytes(), blob_key, expires_at);

    // At t=0: URL is valid.
    assert!(
        verify_with_now(key.as_bytes(), blob_key, expires_at, &signature, now_unix).is_ok(),
        "URL should be valid at issue time"
    );

    // At t=299: still valid.
    clock.advance(Duration::from_secs(299));
    let now_unix = clock_unix_secs(&clock);
    assert!(
        verify_with_now(key.as_bytes(), blob_key, expires_at, &signature, now_unix).is_ok(),
        "URL should be valid 1s before expiry"
    );

    // At t=301: expired — no sleep needed.
    clock.advance(Duration::from_secs(2));
    let now_unix = clock_unix_secs(&clock);
    assert!(
        verify_with_now(key.as_bytes(), blob_key, expires_at, &signature, now_unix).is_err(),
        "URL should be expired after TTL"
    );
}
