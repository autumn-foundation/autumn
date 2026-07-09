//! Integration tests for the test-harness auth helpers (issue #1359).
//!
//! Exercises the cookie jar, `acting_as` / `login_as`, and `log_out`
//! helpers on [`TestClient`], covering each acceptance criterion:
//!
//! 1. The cookie jar persists `Set-Cookie` and replays it, so a real
//!    `POST /login` → `GET /secured` flow works with no manual headers.
//! 2. `acting_as(id)` authenticates a session without hitting `/login`.
//! 3. A non-default `auth.session_key` is honored by `acting_as`.
//! 4. `log_out()` clears the session — the secured route rejects again.
//! 5. `acting_as` sets identity only; authorization (roles) still runs.
//! 6. Two clients keep independent cookie jars.

use autumn_web::config::AutumnConfig;
use autumn_web::prelude::*;
use autumn_web::test::TestApp;

// ── Handlers ──────────────────────────────────────────────────

/// Logs a user in the "real" way: writes the auth session key so the
/// `SessionLayer` emits a `Set-Cookie` the client jar can capture.
#[autumn_web::post("/login")]
async fn login(session: Session) -> &'static str {
    session.insert("user_id", "42").await;
    "logged in"
}

/// Secured route with no role requirement: 401 unless authenticated.
#[autumn_web::get("/secured")]
#[autumn_web::secured]
async fn secured() -> &'static str {
    "secret"
}

/// Secured route requiring the `admin` role: 403 for an authenticated
/// user who lacks it.
#[autumn_web::get("/admin")]
#[autumn_web::secured("admin")]
async fn admin_only() -> &'static str {
    "admin area"
}

// ── AC1: cookie jar round-trips a real login ──────────────────

#[tokio::test]
async fn cookie_jar_round_trips_a_real_login() {
    let client = TestApp::new().routes(routes![login, secured]).build();

    // Unauthenticated: the secured route rejects.
    client.get("/secured").send().await.assert_status(401);

    // Real login: sets the session cookie via Set-Cookie.
    client.post("/login").send().await.assert_ok();

    // Same client, no manual cookie header — the jar replays it.
    client.get("/secured").send().await.assert_ok();
}

// ── AC2: acting_as authenticates without the login endpoint ───

#[tokio::test]
async fn acting_as_authenticates_without_login() {
    let client = TestApp::new().routes(routes![secured]).build();

    client.get("/secured").send().await.assert_status(401);

    client.acting_as(42).await;

    client.get("/secured").send().await.assert_ok();
}

#[tokio::test]
async fn login_as_is_an_alias_for_acting_as() {
    let client = TestApp::new().routes(routes![secured]).build();

    client.login_as("alice").await;

    client.get("/secured").send().await.assert_ok();
}

// ── AC3: configured non-default session key is honored ────────

#[tokio::test]
async fn acting_as_honors_configured_session_key() {
    let mut config = AutumnConfig::default();
    config.auth.session_key = "uid".to_owned();

    let client = TestApp::new()
        .config(config)
        .routes(routes![secured])
        .build();

    client.acting_as(7).await;

    // The secured route resolves auth through the configured key, so the
    // session `acting_as` wrote under `uid` is the identity it extracts.
    client.get("/secured").send().await.assert_ok();
}

// ── AC4: log_out clears the session ───────────────────────────

#[tokio::test]
async fn log_out_reverts_to_unauthenticated() {
    let client = TestApp::new().routes(routes![secured]).build();

    client.acting_as(42).await;
    client.get("/secured").send().await.assert_ok();

    client.log_out();

    client.get("/secured").send().await.assert_status(401);
}

// ── AC5: acting_as sets identity only; authorization still runs ─

#[tokio::test]
async fn acting_as_does_not_bypass_authorization() {
    let client = TestApp::new().routes(routes![admin_only]).build();

    // Acted-as identity has no `role`, so the admin route denies with 403
    // even though authentication succeeds.
    client.acting_as(42).await;

    client.get("/admin").send().await.assert_status(403);
}

// ── acting_as signs the seeded cookie when a signing secret is set ─

#[tokio::test]
async fn acting_as_signs_cookie_when_signing_secret_configured() {
    // A configured signing secret makes the router sign session cookies, so
    // `acting_as` must emit a `{session_id}.{signature}` cookie the real
    // `SessionLayer` accepts — exercising the signed-cookie branch that the
    // default (unsigned) tests never reach.
    let mut config = AutumnConfig::default();
    config.security.signing_secret.secret =
        Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_owned());

    let client = TestApp::new()
        .config(config)
        .routes(routes![secured])
        .build();

    client.get("/secured").send().await.assert_status(401);

    client.acting_as(42).await;

    // The signed cookie round-trips: a raw/unsigned id would fail signature
    // verification and still 401, so a 200 proves the signature is valid.
    client.get("/secured").send().await.assert_ok();
}

// ── AC6: cookie jars are isolated between clients ─────────────

#[tokio::test]
async fn cookie_jars_are_isolated_between_clients() {
    let authed = TestApp::new().routes(routes![login, secured]).build();
    let anon = TestApp::new().routes(routes![login, secured]).build();

    authed.post("/login").send().await.assert_ok();

    // The authed client carries its cookie…
    authed.get("/secured").send().await.assert_ok();
    // …but the second client never saw that Set-Cookie.
    anon.get("/secured").send().await.assert_status(401);
}

// ── Cookie-jar `Expires` evaluation uses the virtual clock ────

/// Emits a `Set-Cookie` for `probe` that expires at a fixed instant
/// (2022-01-01). Whether the jar keeps or drops it depends solely on the
/// clock the jar consults when it folds the header back in.
#[autumn_web::get("/emit-expiring-cookie")]
async fn emit_expiring_cookie() -> impl IntoResponse {
    (
        [(
            axum::http::header::SET_COOKIE,
            "probe=live; Path=/; Expires=Sat, 01 Jan 2022 00:00:00 GMT",
        )],
        "set",
    )
}

/// Echoes the raw `Cookie` request header, so a test can observe whether the
/// jar replayed `probe` on the next request.
#[autumn_web::get("/echo-cookie")]
async fn echo_cookie(headers: axum::http::HeaderMap) -> String {
    headers
        .get(axum::http::header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned()
}

#[tokio::test]
async fn cookie_jar_expires_check_uses_virtual_clock_future() {
    use autumn_web::time::FixedClock;
    use chrono::{TimeZone, Utc};

    // Pin the clock *before* the cookie's `Expires` (2022): the jar must keep
    // the cookie. Under real time (2026+) it would be dropped, so a retained
    // cookie proves the jar consulted the virtual clock, not `Utc::now()`.
    let client = TestApp::new()
        .with_clock(FixedClock::at(
            Utc.with_ymd_and_hms(2021, 6, 1, 0, 0, 0).unwrap(),
        ))
        .routes(routes![emit_expiring_cookie, echo_cookie])
        .build();

    client.get("/emit-expiring-cookie").send().await.assert_ok();
    client
        .get("/echo-cookie")
        .send()
        .await
        .assert_body_contains("probe=live");
}

#[tokio::test]
async fn cookie_jar_expires_check_uses_virtual_clock_past() {
    use autumn_web::time::FixedClock;
    use chrono::{TimeZone, Utc};

    // Pin the clock *after* the cookie's `Expires` (2022): the jar must drop it.
    let client = TestApp::new()
        .with_clock(FixedClock::at(
            Utc.with_ymd_and_hms(2023, 6, 1, 0, 0, 0).unwrap(),
        ))
        .routes(routes![emit_expiring_cookie, echo_cookie])
        .build();

    client.get("/emit-expiring-cookie").send().await.assert_ok();
    // The jar dropped the expired cookie, so no `Cookie` header is replayed.
    let echoed = client.get("/echo-cookie").send().await;
    assert!(
        !echoed.text().contains("probe"),
        "expected the virtually-expired cookie to be dropped, but it was replayed: {:?}",
        echoed.text(),
    );
}

// ── Live-cookie `Max-Age` expiry is honored at replay ─────────

/// Emits a `Set-Cookie` for `probe` that lives for one hour (`Max-Age=3600`).
/// The absolute expiry is computed from the jar's clock at the moment the
/// header is folded in, so advancing the clock past it must stop the replay.
#[autumn_web::get("/emit-live-cookie")]
async fn emit_live_cookie() -> impl IntoResponse {
    (
        [(
            axum::http::header::SET_COOKIE,
            "probe=live; Path=/; Max-Age=3600",
        )],
        "set",
    )
}

#[tokio::test]
async fn live_cookie_max_age_expires_against_virtual_clock() {
    use autumn_web::time::TickingClock;
    use chrono::{TimeZone, Utc};
    use std::time::Duration;

    let client = TestApp::new()
        .with_clock(TickingClock::starting_at(
            Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap(),
        ))
        .routes(routes![emit_live_cookie, echo_cookie])
        .build();

    // Set the cookie: expires at start + 3600s.
    client.get("/emit-live-cookie").send().await.assert_ok();

    // Before expiry (only 100s later): the cookie is still live and replays.
    client.advance_clock(Duration::from_secs(100));
    client
        .get("/echo-cookie")
        .send()
        .await
        .assert_body_contains("probe=live");

    // Advance past the one-hour lifetime: the cookie must no longer replay.
    client.advance_clock(Duration::from_secs(3600));
    let echoed = client.get("/echo-cookie").send().await;
    assert!(
        !echoed.text().contains("probe"),
        "expected the live cookie to stop replaying once the clock passed its Max-Age, but it was replayed: {:?}",
        echoed.text(),
    );
}
