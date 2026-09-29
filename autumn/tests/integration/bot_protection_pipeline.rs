//! Integration tests for bot protection / CAPTCHA middleware (Issue #828).
//!
//! Verifies that configuring `[bot_protection]` in `AutumnConfig` wires the
//! [`BotProtectionLayer`] into the router pipeline and produces
//! `400 Bad Request` with Problem Details when a CAPTCHA token is missing or
//! invalid, while dev-bypass and disabled modes pass through freely.

use autumn_web::config::AutumnConfig;
use autumn_web::security::captcha::BotProtectionLayer;
use autumn_web::test::TestApp;
use autumn_web::{get, post, routes};

#[post("/submit")]
async fn submit() -> &'static str {
    "submitted"
}

#[get("/ping")]
async fn ping() -> &'static str {
    "pong"
}

fn enabled_config() -> AutumnConfig {
    let mut config = AutumnConfig::default();
    config.bot_protection.enabled = true;
    config.bot_protection.dev_bypass = false;
    config
}

fn dev_bypass_config() -> AutumnConfig {
    let mut config = AutumnConfig::default();
    config.bot_protection.enabled = true;
    config.bot_protection.dev_bypass = true;
    config
}

// ── Missing token → 400 ────────────────────────────────────────────────────

#[tokio::test]
async fn missing_token_on_post_yields_400() {
    let client = TestApp::new()
        .routes(routes![submit])
        .config(enabled_config())
        .build();

    let resp = client
        .post("/submit")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("field=value")
        .send()
        .await;

    resp.assert_status(400);
}

#[tokio::test]
async fn missing_token_response_is_problem_details() {
    let client = TestApp::new()
        .routes(routes![submit])
        .config(enabled_config())
        .build();

    let resp = client
        .post("/submit")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("")
        .send()
        .await;

    resp.assert_status(400);
    resp.assert_header("content-type", "application/problem+json");
}

// ── Dev bypass passes without token ────────────────────────────────────────

#[tokio::test]
async fn dev_bypass_passes_without_token() {
    let client = TestApp::new()
        .routes(routes![submit])
        .config(dev_bypass_config())
        .build();

    client
        .post("/submit")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("field=value")
        .send()
        .await
        .assert_status(200);
}

// ── Disabled bot protection is passthrough ─────────────────────────────────

#[tokio::test]
async fn disabled_bot_protection_is_passthrough() {
    let client = TestApp::new().routes(routes![submit]).build(); // default config has bot_protection.enabled = false

    client
        .post("/submit")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("field=value")
        .send()
        .await
        .assert_status(200);
}

// ── GET requests are exempt ─────────────────────────────────────────────────

#[tokio::test]
async fn get_request_exempt_from_bot_protection() {
    let client = TestApp::new()
        .routes(routes![ping])
        .config(enabled_config())
        .build();

    client.get("/ping").send().await.assert_status(200);
}

// ── Valid token via dev-bypass equivalent (AlwaysPassProvider) ─────────────

#[tokio::test]
async fn valid_token_with_bypass_provider_passes() {
    use autumn_web::security::captcha::AlwaysPassProvider;
    use std::sync::Arc;

    let layer = BotProtectionLayer::new(Arc::new(AlwaysPassProvider));

    let client = TestApp::new().routes(routes![submit]).layer(layer).build();

    client
        .post("/submit")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("cf-turnstile-response=any-token-value")
        .send()
        .await
        .assert_status(200);
}

// ── Invalid token with TestProvider → 400 ─────────────────────────────────

#[tokio::test]
async fn invalid_token_yields_400() {
    use autumn_web::security::captcha::TestCaptchaProvider;
    use std::sync::Arc;

    let layer = BotProtectionLayer::new(Arc::new(TestCaptchaProvider::new("correct-token")));

    let client = TestApp::new().routes(routes![submit]).layer(layer).build();

    // Send wrong token
    client
        .post("/submit")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("cf-turnstile-response=wrong-token")
        .send()
        .await
        .assert_status(400);
}

// ── Correct token with TestProvider → 200 ─────────────────────────────────

#[tokio::test]
async fn correct_token_passes() {
    use autumn_web::security::captcha::TestCaptchaProvider;
    use std::sync::Arc;

    let layer = BotProtectionLayer::new(Arc::new(TestCaptchaProvider::new("correct-token")));

    let client = TestApp::new().routes(routes![submit]).layer(layer).build();

    client
        .post("/submit")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("cf-turnstile-response=correct-token")
        .send()
        .await
        .assert_status(200);
}

// ── Maud widget helper emits markup ────────────────────────────────────────

#[cfg(feature = "maud")]
#[test]
fn bot_protection_widget_turnstile_emits_markup() {
    use autumn_web::security::captcha::{
        BotProtectionConfig, CaptchaProviderKind, bot_protection_widget,
    };

    let config = BotProtectionConfig {
        enabled: true,
        provider: CaptchaProviderKind::Turnstile,
        site_key: Some("test-site-key".to_string()),
        secret_key: None,
        form_field: None,
        dev_bypass: false,
    };

    let markup = bot_protection_widget(&config);
    let html = markup.into_string();
    assert!(
        html.contains("cf-turnstile"),
        "Should contain Turnstile widget class"
    );
    assert!(html.contains("test-site-key"), "Should contain site key");
    assert!(
        html.contains("challenges.cloudflare.com"),
        "Should contain Turnstile script src"
    );
}

#[cfg(feature = "maud")]
#[test]
fn bot_protection_widget_hcaptcha_emits_markup() {
    use autumn_web::security::captcha::{
        BotProtectionConfig, CaptchaProviderKind, bot_protection_widget,
    };

    let config = BotProtectionConfig {
        enabled: true,
        provider: CaptchaProviderKind::HCaptcha,
        site_key: Some("hcaptcha-site-key".to_string()),
        secret_key: None,
        form_field: None,
        dev_bypass: false,
    };

    let markup = bot_protection_widget(&config);
    let html = markup.into_string();
    assert!(
        html.contains("h-captcha"),
        "Should contain hCaptcha widget class"
    );
    assert!(
        html.contains("hcaptcha-site-key"),
        "Should contain site key"
    );
    assert!(
        html.contains("js.hcaptcha.com"),
        "Should contain hCaptcha script src"
    );
}

// ── Config deserialization ─────────────────────────────────────────────────

#[test]
fn bot_protection_config_parses_from_toml() {
    let toml = r#"
[bot_protection]
enabled = true
provider = "turnstile"
site_key = "site-abc"
secret_key = "secret-xyz"
dev_bypass = false
"#;
    let config: AutumnConfig = toml::from_str(toml).expect("should parse");
    assert!(config.bot_protection.enabled);
    assert_eq!(config.bot_protection.site_key.as_deref(), Some("site-abc"));
    assert_eq!(
        config.bot_protection.secret_key.as_deref(),
        Some("secret-xyz")
    );
    assert!(!config.bot_protection.dev_bypass);
}

#[test]
fn bot_protection_config_defaults_to_disabled() {
    let config = AutumnConfig::default();
    assert!(!config.bot_protection.enabled);
}
