//! Security regression: dev-only surfaces must agree on what "dev" means.
//!
//! Autumn has several profile-conditional surfaces that are meant to be safe
//! in production and dangerous only in `dev` (the request inspector at
//! `/_autumn/inspect`, the dev error-overlay badge injected into HTML error
//! pages). Each is supposed to derive its "am I in dev?" decision from the
//! same fact: `config.profile == Some("dev" | "development")`.
//!
//! `docs/guide/custom-subsystems.md` documents installing a custom
//! [`ConfigLoader`] (e.g. a `JsonFileConfigLoader` that
//! `serde_json::from_slice`s an `AutumnConfig` straight out of a JSON file).
//! That is a fully-supported, documented way to configure an app — and
//! nothing in the docs says the JSON must include a `"profile"` key. When it
//! doesn't, `AutumnConfig::profile` deserializes to `None` (the field is
//! `Option<String>` with no `#[serde(default = ...)]` override).
//!
//! The request inspector gate (`router.rs`'s `is_dev_profile`) treats a
//! `None` profile as "not dev" — fails closed. The dev error-overlay's `is_dev`
//! (`apply_middleware` in `router.rs`) instead falls back to
//! `cfg!(debug_assertions)` when the profile is `None` — so a debug build
//! (the default `cargo build`, no `--release`) running with a profile-less
//! config serves the *full* dev error overlay (stack frames, scrubbed
//! cookies/headers/body preview, SQL queries, route pattern) to anyone who
//! can trigger a 500, while the inspector correctly stays a 404. This test
//! pins the two gates to agree: a `None` profile must not activate the dev
//! badge either.
//!
//! Threat model: against an app that follows the documented
//! `with_config_loader` pattern and does not think to add a `profile` field
//! to its JSON config (nothing in the docs says to), an unauthenticated
//! attacker who can trigger any 5xx/4xx on an HTML-accepting route obtains
//! the dev error overlay — internal error messages, request headers,
//! cookies, and (when available) SQL query text and stack frames — on a
//! deployment the operator believes is production-safe because they never
//! set `profile = "dev"`.

use autumn_web::config::AutumnConfig;
use autumn_web::test::TestApp;
use autumn_web::{AutumnError, get, routes};

#[get("/boom")]
async fn boom() -> Result<&'static str, AutumnError> {
    Err(AutumnError::internal_server_error_msg(
        "sentinel-internal-failure-profile-audit",
    ))
}

/// Build the exact shape a documented custom `ConfigLoader` produces when its
/// backing JSON/TOML/etc. simply has no `profile` key: `AutumnConfig::profile
/// == None`. Nothing else about the config is unusual — CSRF is disabled the
/// same way `TestApp::new()` disables it, so the only variable under test is
/// the missing profile.
fn config_with_no_profile() -> AutumnConfig {
    let mut config = AutumnConfig::default();
    config.security.csrf.enabled = false;
    assert!(
        config.profile.is_none(),
        "test setup: AutumnConfig::default() must leave profile unset"
    );
    config
}

/// Sibling #1: the request inspector must stay a 404 when the profile is
/// unset — this is the control showing the OTHER dev-only surface already
/// fails closed on `None`.
#[tokio::test]
async fn inspector_stays_closed_when_profile_is_unset() {
    let client = TestApp::new()
        .config(config_with_no_profile())
        .routes(routes![boom])
        .build();

    let resp = client.get("/_autumn/inspect").send().await;
    resp.assert_status(404);
}

/// Sibling #2 (the bug): the dev error-overlay badge must ALSO stay off when
/// the profile is unset, matching the inspector's fail-closed behaviour.
///
/// This fails on trunk: `apply_middleware`'s `is_dev` falls back to
/// `cfg!(debug_assertions)` (true under `cargo test`) when `config.profile`
/// is `None`, so the badge — including the sentinel internal error message
/// this handler sets — is injected into the response.
#[tokio::test]
async fn dev_error_badge_stays_closed_when_profile_is_unset() {
    let client = TestApp::new()
        .config(config_with_no_profile())
        .routes(routes![boom])
        .build();

    let resp = client
        .get("/boom")
        .header("accept", "text/html")
        .send()
        .await;
    resp.assert_status(500);

    let body = resp.text();
    assert!(
        !body.contains("autumn-dev-error-badge"),
        "dev error overlay must not activate for an app that never set \
         profile = \"dev\" (profile is unset here, exactly as a documented \
         custom ConfigLoader with no `profile` field in its source would \
         produce), got body: {body}"
    );
    assert!(
        !body.contains("sentinel-internal-failure-profile-audit"),
        "the raw internal error message must not leak through the dev \
         overlay when the profile is unset, got body: {body}"
    );
}

/// Positive control: with an explicit `profile = "prod"`, the badge is
/// correctly suppressed (this already passes on trunk — included so the
/// contrast with the `None`-profile case above is explicit in one file).
#[tokio::test]
async fn dev_error_badge_stays_closed_in_explicit_prod_profile() {
    let mut config = AutumnConfig {
        profile: Some("prod".to_owned()),
        ..AutumnConfig::default()
    };
    config.security.csrf.enabled = false;
    // The `prod` profile's `TrustedHostLayer` no longer auto-trusts
    // `localhost` (see `TrustedHostPolicy::from_config` in router.rs), so the
    // test client's default Host header would otherwise be rejected with a
    // 400 before the handler — and this test — ever runs. Host validation is
    // not the surface under test here, so open it wide.
    config.security.trusted_hosts.hosts = vec!["*".to_owned()];

    let client = TestApp::new().config(config).routes(routes![boom]).build();

    let resp = client
        .get("/boom")
        .header("accept", "text/html")
        // `TrustedHostPolicy::allow_missing_host` is also profile-gated (only
        // non-`prod`/`production` profiles allow a request with no `Host` at
        // all through) and the test client sends no `Host` header by
        // default, so under `prod` that combination alone would 400 before
        // reaching the handler even with `trusted_hosts = ["*"]` above
        // (which only widens which *present* hosts are accepted). Supplying
        // a `Host` header sidesteps that unrelated gate so this test
        // exercises only the dev-overlay surface under test.
        .header("host", "example.com")
        .send()
        .await;
    resp.assert_status(500);

    let body = resp.text();
    assert!(
        !body.contains("autumn-dev-error-badge"),
        "dev error overlay must not activate under an explicit prod profile, \
         got body: {body}"
    );
}
