use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use autumn_web::config::{AutumnConfig, MockEnv};
use autumn_web::prelude::*;
use autumn_web::security::{CsrfConfig, SecurityConfig};
use autumn_web::test::{TestApp, TestResponse};
use autumn_web::webhook::{
    InMemoryWebhookReplayStore, SignedWebhook, WebhookEndpointConfig, WebhookProvider,
    WebhookRegistry, WebhookReplayBackend, WebhookReplayFuture, WebhookReplayStore,
    WebhookReplayStoreError, hmac_sha256_hex,
};
use serde_json::json;

static HANDLER_CALLS: AtomicUsize = AtomicUsize::new(0);
static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const CURRENT_SECRET: &str = "current-webhook-secret-32-bytes!!";
const PREVIOUS_SECRET: &str = "previous-webhook-secret-32-bytes!";

#[derive(Debug)]
struct UnavailableReplayStore;

impl WebhookReplayStore for UnavailableReplayStore {
    fn check_and_insert<'a>(
        &'a self,
        _key: &'a str,
        _received_at: SystemTime,
        _window: Duration,
    ) -> WebhookReplayFuture<'a> {
        Box::pin(async {
            Err(WebhookReplayStoreError::new(
                "custom replay backend offline",
            ))
        })
    }

    fn remove<'a>(&'a self, _key: &'a str) -> WebhookReplayFuture<'a> {
        Box::pin(async {
            Err(WebhookReplayStoreError::new(
                "custom replay backend offline",
            ))
        })
    }
}

#[post("/webhooks/stripe")]
async fn stripe_webhook(webhook: SignedWebhook) -> Json<serde_json::Value> {
    HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
    Json(json!({
        "provider": webhook.provider(),
        "delivery_id": webhook.delivery_id(),
        "event_type": webhook.event_type(),
        "raw": String::from_utf8_lossy(webhook.raw_body()),
    }))
}

#[post("/webhooks/github")]
async fn github_webhook(webhook: SignedWebhook) -> Json<serde_json::Value> {
    HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
    Json(json!({
        "provider": webhook.provider(),
        "delivery_id": webhook.delivery_id(),
        "event_type": webhook.event_type(),
    }))
}

#[post("/webhooks/slack")]
async fn slack_webhook(webhook: SignedWebhook) -> Json<serde_json::Value> {
    HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
    Json(json!({
        "provider": webhook.provider(),
        "delivery_id": webhook.delivery_id(),
        "event_type": webhook.event_type(),
    }))
}

#[post("/webhooks/generic")]
async fn generic_webhook(webhook: SignedWebhook) -> Json<serde_json::Value> {
    HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
    Json(json!({
        "provider": webhook.provider(),
        "delivery_id": webhook.delivery_id(),
        "event_type": webhook.event_type(),
    }))
}

fn unix_now() -> i64 {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock must be after unix epoch")
        .as_secs();
    i64::try_from(secs).expect("current unix timestamp fits in i64")
}

fn webhook_config(endpoints: Vec<WebhookEndpointConfig>) -> AutumnConfig {
    AutumnConfig {
        profile: Some("test".to_owned()),
        security: SecurityConfig {
            csrf: CsrfConfig {
                enabled: false,
                ..Default::default()
            },
            webhooks: autumn_web::webhook::WebhookConfig {
                endpoints,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    }
}

fn client(endpoints: Vec<WebhookEndpointConfig>) -> autumn_web::test::TestClient {
    TestApp::new()
        .config(webhook_config(endpoints))
        .routes(routes![
            stripe_webhook,
            github_webhook,
            slack_webhook,
            generic_webhook
        ])
        .build()
}

fn client_with_registry(registry: WebhookRegistry) -> autumn_web::test::TestClient {
    let state = autumn_web::AppState::for_test().with_extension(registry);
    let mut route_defs = routes![github_webhook];
    let route = route_defs.remove(0);
    let router = axum::Router::new()
        .route(route.path, route.handler)
        .with_state(state.clone());
    TestApp::from_router(router, state)
}

fn endpoint(
    provider: WebhookProvider,
    path: &'static str,
    name: &'static str,
) -> WebhookEndpointConfig {
    WebhookEndpointConfig::new(name, path, provider, CURRENT_SECRET)
        .with_previous_secret(PREVIOUS_SECRET)
        .with_timestamp_tolerance_secs(300)
        .with_replay_window_secs(300)
}

fn stripe_signature(secret: &str, timestamp: i64, body: &[u8]) -> String {
    let mut signed_payload = timestamp.to_string().into_bytes();
    signed_payload.push(b'.');
    signed_payload.extend_from_slice(body);
    let signature = hmac_sha256_hex(secret.as_bytes(), &signed_payload);
    format!("t={timestamp},v1={signature}")
}

fn github_signature(secret: &str, body: &[u8]) -> String {
    format!("sha256={}", hmac_sha256_hex(secret.as_bytes(), body))
}

fn slack_signature(secret: &str, timestamp: i64, body: &[u8]) -> String {
    let timestamp = timestamp.to_string();
    let mut signed_payload = Vec::with_capacity(3 + timestamp.len() + 1 + body.len());
    signed_payload.extend_from_slice(b"v0:");
    signed_payload.extend_from_slice(timestamp.as_bytes());
    signed_payload.push(b':');
    signed_payload.extend_from_slice(body);
    format!("v0={}", hmac_sha256_hex(secret.as_bytes(), &signed_payload))
}

fn generic_signature(secret: &str, body: &[u8]) -> String {
    format!("sha256={}", hmac_sha256_hex(secret.as_bytes(), body))
}

fn problem_json(response: &TestResponse, status: u16) -> serde_json::Value {
    response.assert_status(status);
    response.assert_header_contains("content-type", "application/problem+json");
    let json: serde_json::Value = response.json();
    assert_eq!(json["status"], status);
    assert!(
        json["detail"]
            .as_str()
            .is_some_and(|detail| !detail.is_empty()),
        "Problem+JSON response should include a detail message"
    );
    assert!(
        json["instance"].as_str().is_some(),
        "Problem+JSON response should include an instance path"
    );
    assert!(
        json["request_id"].as_str().is_some(),
        "Problem+JSON response should include a request_id"
    );
    json
}

fn assert_duplicate_path_error(error: impl std::fmt::Display) {
    let message = error.to_string();
    assert!(
        message.contains("duplicate")
            && message.contains("/webhooks/duplicate")
            && message.contains("stripe")
            && message.contains("github"),
        "duplicate path error should identify both endpoints and the shared path, got: {message}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn provider_presets_verify_valid_requests_and_expose_metadata() {
    let _guard = TEST_LOCK.lock().await;
    HANDLER_CALLS.store(0, Ordering::SeqCst);
    let client = client(vec![
        endpoint(WebhookProvider::Stripe, "/webhooks/stripe", "stripe"),
        endpoint(WebhookProvider::Github, "/webhooks/github", "github"),
        endpoint(WebhookProvider::Slack, "/webhooks/slack", "slack"),
        endpoint(WebhookProvider::Generic, "/webhooks/generic", "generic"),
    ]);
    let now = unix_now();

    let stripe_body = br#"{"id":"evt_123","type":"invoice.paid"}"#;
    let response = client
        .post("/webhooks/stripe")
        .header("content-type", "application/json")
        .header(
            "stripe-signature",
            &stripe_signature(CURRENT_SECRET, now, stripe_body),
        )
        .body(stripe_body.as_slice())
        .send()
        .await;
    response.assert_ok();
    let stripe: serde_json::Value = response.json();
    assert_eq!(stripe["provider"], "stripe");
    assert_eq!(stripe["delivery_id"], "evt_123");
    assert_eq!(stripe["event_type"], "invoice.paid");
    assert_eq!(stripe["raw"], r#"{"id":"evt_123","type":"invoice.paid"}"#);

    let github_body = br#"{"action":"opened"}"#;
    let response = client
        .post("/webhooks/github")
        .header(
            "x-hub-signature-256",
            &github_signature(CURRENT_SECRET, github_body),
        )
        .header("x-github-delivery", "gh-delivery-1")
        .header("x-github-event", "pull_request")
        .body(github_body.as_slice())
        .send()
        .await;
    response.assert_ok();
    let github: serde_json::Value = response.json();
    assert_eq!(github["provider"], "github");
    assert_eq!(github["delivery_id"], "gh-delivery-1");
    assert_eq!(github["event_type"], "pull_request");

    let slack_body = br#"{"type":"event_callback","event":{"type":"message"},"event_id":"Ev-provider-preset","event_time":1234567890}"#;
    let response = client
        .post("/webhooks/slack")
        .header("content-type", "application/json")
        .header("x-slack-request-timestamp", &now.to_string())
        .header(
            "x-slack-signature",
            &slack_signature(CURRENT_SECRET, now, slack_body),
        )
        .body(slack_body.as_slice())
        .send()
        .await;
    response.assert_ok();
    let slack: serde_json::Value = response.json();
    assert_eq!(slack["provider"], "slack");
    assert_eq!(slack["delivery_id"], "Ev-provider-preset");
    assert_eq!(slack["event_type"], "event_callback");

    let generic_body = br#"{"kind":"cms.updated"}"#;
    let response = client
        .post("/webhooks/generic")
        .header(
            "x-webhook-signature",
            &generic_signature(CURRENT_SECRET, generic_body),
        )
        .header("x-webhook-delivery", "generic-delivery-1")
        .header("x-webhook-event", "cms.updated")
        .body(generic_body.as_slice())
        .send()
        .await;
    response.assert_ok();
    let generic: serde_json::Value = response.json();
    assert_eq!(generic["provider"], "generic");
    assert_eq!(generic["delivery_id"], "generic-delivery-1");
    assert_eq!(generic["event_type"], "cms.updated");

    assert_eq!(HANDLER_CALLS.load(Ordering::SeqCst), 4);
}

#[tokio::test(flavor = "current_thread")]
async fn slack_events_api_json_body_uses_event_id_for_replay_protection() {
    let _guard = TEST_LOCK.lock().await;
    HANDLER_CALLS.store(0, Ordering::SeqCst);
    let client = client(vec![endpoint(
        WebhookProvider::Slack,
        "/webhooks/slack",
        "slack",
    )]);
    let now = unix_now();
    let body = br#"{"type":"event_callback","event":{"type":"app_mention"},"event_id":"Ev123ABC456","event_time":1234567890}"#;
    let signature = slack_signature(CURRENT_SECRET, now, body);

    let first = client
        .post("/webhooks/slack")
        .header("content-type", "application/json")
        .header("x-slack-request-timestamp", &now.to_string())
        .header("x-slack-signature", &signature)
        .body(body.as_slice())
        .send()
        .await;
    first.assert_ok();
    let json: serde_json::Value = first.json();
    assert_eq!(json["provider"], "slack");
    assert_eq!(json["delivery_id"], "Ev123ABC456");
    assert_eq!(json["event_type"], "event_callback");

    let second = client
        .post("/webhooks/slack")
        .header("content-type", "application/json")
        .header("x-slack-request-timestamp", &now.to_string())
        .header("x-slack-signature", &signature)
        .body(body.as_slice())
        .send()
        .await;
    problem_json(&second, 409);
    assert_eq!(HANDLER_CALLS.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn slack_url_verification_json_body_uses_challenge_as_replay_id() {
    let _guard = TEST_LOCK.lock().await;
    HANDLER_CALLS.store(0, Ordering::SeqCst);
    let client = client(vec![endpoint(
        WebhookProvider::Slack,
        "/webhooks/slack",
        "slack",
    )]);
    let now = unix_now();
    let body =
        br#"{"token":"deprecated-token","challenge":"challenge-123","type":"url_verification"}"#;

    let response = client
        .post("/webhooks/slack")
        .header("content-type", "application/json")
        .header("x-slack-request-timestamp", &now.to_string())
        .header(
            "x-slack-signature",
            &slack_signature(CURRENT_SECRET, now, body),
        )
        .body(body.as_slice())
        .send()
        .await;
    response.assert_ok();
    let json: serde_json::Value = response.json();
    assert_eq!(json["provider"], "slack");
    assert_eq!(json["delivery_id"], "challenge-123");
    assert_eq!(json["event_type"], "url_verification");
    assert_eq!(HANDLER_CALLS.load(Ordering::SeqCst), 1);
}

#[test]
fn duplicate_webhook_paths_are_rejected_before_registry_construction() {
    let config = webhook_config(vec![
        endpoint(WebhookProvider::Stripe, "/webhooks/duplicate", "stripe"),
        endpoint(WebhookProvider::Github, "/webhooks/duplicate", "github"),
    ]);

    let error = config
        .validate()
        .expect_err("duplicate webhook paths must fail app config validation");
    assert_duplicate_path_error(error);

    let error = WebhookRegistry::from_config(&config.security.webhooks)
        .expect_err("duplicate webhook paths must fail direct registry construction");
    assert_duplicate_path_error(error);
}

#[tokio::test(flavor = "current_thread")]
async fn byte_modified_payload_fails_before_handler_even_when_json_is_equivalent() {
    let _guard = TEST_LOCK.lock().await;
    HANDLER_CALLS.store(0, Ordering::SeqCst);
    let client = client(vec![endpoint(
        WebhookProvider::Stripe,
        "/webhooks/stripe",
        "stripe",
    )]);
    let now = unix_now();
    let signed_body = br#"{"id":"evt_tamper","type":"invoice.paid"}"#;
    let modified_body = br#"{"id": "evt_tamper", "type": "invoice.paid"}"#;

    let response = client
        .post("/webhooks/stripe")
        .header("content-type", "application/json")
        .header(
            "stripe-signature",
            &stripe_signature(CURRENT_SECRET, now, signed_body),
        )
        .body(modified_body.as_slice())
        .send()
        .await;

    let json = problem_json(&response, 401);
    assert!(
        json["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("signature"))
    );
    assert_eq!(HANDLER_CALLS.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn rejects_missing_malformed_stale_and_bad_signatures_with_problem_details() {
    let _guard = TEST_LOCK.lock().await;
    HANDLER_CALLS.store(0, Ordering::SeqCst);
    let client = client(vec![endpoint(
        WebhookProvider::Stripe,
        "/webhooks/stripe",
        "stripe",
    )]);
    let body = br#"{"id":"evt_bad","type":"invoice.paid"}"#;
    let now = unix_now();

    let missing = client
        .post("/webhooks/stripe")
        .header("content-type", "application/json")
        .body(body.as_slice())
        .send()
        .await;
    problem_json(&missing, 400);

    let malformed = client
        .post("/webhooks/stripe")
        .header("content-type", "application/json")
        .header("stripe-signature", "not-a-stripe-signature")
        .body(body.as_slice())
        .send()
        .await;
    problem_json(&malformed, 400);

    // Use a 600s offset — well beyond the 300s tolerance — so that the few
    // seconds of test execution time on a slow Windows CI runner never shrinks
    // the skew below the threshold and causes a spurious 200.
    let stale_timestamp = now - 600;
    let stale = client
        .post("/webhooks/stripe")
        .header("content-type", "application/json")
        .header(
            "stripe-signature",
            &stripe_signature(CURRENT_SECRET, stale_timestamp, body),
        )
        .body(body.as_slice())
        .send()
        .await;
    problem_json(&stale, 401);

    // Capture the future timestamp immediately before sending so that only the
    // round-trip time (not prior requests) affects the skew.
    let future_timestamp = unix_now() + 600;
    let future = client
        .post("/webhooks/stripe")
        .header("content-type", "application/json")
        .header(
            "stripe-signature",
            &stripe_signature(CURRENT_SECRET, future_timestamp, body),
        )
        .body(body.as_slice())
        .send()
        .await;
    problem_json(&future, 401);

    let bad = client
        .post("/webhooks/stripe")
        .header("content-type", "application/json")
        .header(
            "stripe-signature",
            &stripe_signature("wrong-webhook-secret-32-bytes!!", now, body),
        )
        .body(body.as_slice())
        .send()
        .await;
    problem_json(&bad, 401);

    assert_eq!(HANDLER_CALLS.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn duplicate_delivery_ids_are_rejected_deterministically() {
    let _guard = TEST_LOCK.lock().await;
    HANDLER_CALLS.store(0, Ordering::SeqCst);
    let client = client(vec![endpoint(
        WebhookProvider::Github,
        "/webhooks/github",
        "github",
    )]);
    let body = br#"{"action":"opened"}"#;
    let signature = github_signature(CURRENT_SECRET, body);

    let first = client
        .post("/webhooks/github")
        .header("x-hub-signature-256", &signature)
        .header("x-github-delivery", "same-delivery")
        .header("x-github-event", "pull_request")
        .body(body.as_slice())
        .send()
        .await;
    first.assert_ok();

    let second = client
        .post("/webhooks/github")
        .header("x-hub-signature-256", &signature)
        .header("x-github-delivery", "same-delivery")
        .header("x-github-event", "pull_request")
        .body(body.as_slice())
        .send()
        .await;
    let json = problem_json(&second, 409);
    assert!(
        json["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("duplicate"))
    );
    assert_eq!(HANDLER_CALLS.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn custom_replay_store_failures_are_reported_as_service_unavailable() {
    let _guard = TEST_LOCK.lock().await;
    HANDLER_CALLS.store(0, Ordering::SeqCst);
    let config = autumn_web::webhook::WebhookConfig {
        endpoints: vec![endpoint(
            WebhookProvider::Github,
            "/webhooks/github",
            "github",
        )],
        ..Default::default()
    };
    let registry = WebhookRegistry::from_config_with_replay_store(&config, UnavailableReplayStore)
        .expect("custom replay store should be installable");
    let client = client_with_registry(registry);
    let body = br#"{"action":"opened"}"#;

    let response = client
        .post("/webhooks/github")
        .header(
            "x-hub-signature-256",
            &github_signature(CURRENT_SECRET, body),
        )
        .header("x-github-delivery", "store-outage-delivery")
        .header("x-github-event", "pull_request")
        .body(body.as_slice())
        .send()
        .await;

    response.assert_status(503);
    let json: serde_json::Value = response.json();
    assert_eq!(json["status"], 503);
    assert!(
        json["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("custom replay backend offline"))
    );
    assert_eq!(HANDLER_CALLS.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn in_memory_replay_store_rejects_duplicates_until_window_expires() {
    let store = InMemoryWebhookReplayStore::default();
    let received_at = UNIX_EPOCH + Duration::from_secs(1_000);
    let window = Duration::from_secs(300);

    assert!(
        store
            .check_and_insert("stripe:stripe:evt_replay", received_at, window)
            .await
            .expect("in-memory replay store should not fail")
    );
    assert!(
        !store
            .check_and_insert(
                "stripe:stripe:evt_replay",
                received_at + Duration::from_secs(299),
                window,
            )
            .await
            .expect("in-memory replay store should not fail")
    );
    assert!(
        store
            .check_and_insert(
                "stripe:stripe:evt_replay",
                received_at + Duration::from_secs(301),
                window,
            )
            .await
            .expect("in-memory replay store should not fail")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn previous_secret_is_accepted_during_rotation() {
    let _guard = TEST_LOCK.lock().await;
    HANDLER_CALLS.store(0, Ordering::SeqCst);
    let client = client(vec![endpoint(
        WebhookProvider::Github,
        "/webhooks/github",
        "github",
    )]);
    let body = br#"{"action":"closed"}"#;

    let response = client
        .post("/webhooks/github")
        .header(
            "x-hub-signature-256",
            &github_signature(PREVIOUS_SECRET, body),
        )
        .header("x-github-delivery", "rotated-delivery")
        .header("x-github-event", "pull_request")
        .body(body.as_slice())
        .send()
        .await;

    response.assert_ok();
    assert_eq!(HANDLER_CALLS.load(Ordering::SeqCst), 1);
}

#[test]
fn production_config_rejects_missing_or_weak_webhook_secret() {
    let mut missing = webhook_config(vec![WebhookEndpointConfig {
        name: "stripe".to_owned(),
        path: "/webhooks/stripe".to_owned(),
        provider: WebhookProvider::Stripe,
        secret: None,
        previous_secrets: Vec::new(),
        ..Default::default()
    }]);
    missing.profile = Some("prod".to_owned());
    let error = missing
        .validate()
        .expect_err("prod webhook secret must be required");
    assert!(
        error.to_string().contains("webhook")
            && error.to_string().contains("stripe")
            && error.to_string().contains("secret")
    );

    let mut weak = webhook_config(vec![endpoint(
        WebhookProvider::Github,
        "/webhooks/github",
        "github",
    )]);
    weak.profile = Some("prod".to_owned());
    weak.security.webhooks.endpoints[0].secret = Some("secret".to_owned());
    let error = weak
        .validate()
        .expect_err("weak prod webhook secret must fail");
    assert!(
        error.to_string().contains("webhook")
            && error.to_string().contains("github")
            && error.to_string().contains("template")
    );
}

#[test]
fn production_config_requires_shared_replay_backend_unless_explicitly_allowed() {
    let mut config = webhook_config(vec![endpoint(
        WebhookProvider::Github,
        "/webhooks/github",
        "github",
    )]);
    config.profile = Some("prod".to_owned());

    let error = config
        .validate()
        .expect_err("prod webhook replay protection must reject memory backend");
    assert!(
        error.to_string().contains("replay")
            && error.to_string().contains("memory")
            && error.to_string().contains("production")
    );

    config.security.webhooks.replay.allow_memory_in_production = true;
    config
        .validate()
        .expect("explicit memory replay opt-in should validate");
}

#[test]
fn webhook_replay_backend_loads_from_toml_and_env() {
    let config: AutumnConfig = toml::from_str(
        r#"
            [security.webhooks.replay]
            backend = "redis"
            allow_memory_in_production = false

            [security.webhooks.replay.redis]
            url = "redis://localhost:6379/3"
            key_prefix = "myapp:webhooks:replay"
        "#,
    )
    .expect("webhook replay config should parse");
    assert_eq!(
        config.security.webhooks.replay.backend,
        WebhookReplayBackend::Redis
    );
    assert_eq!(
        config.security.webhooks.replay.redis.url.as_deref(),
        Some("redis://localhost:6379/3")
    );
    assert_eq!(
        config.security.webhooks.replay.redis.key_prefix,
        "myapp:webhooks:replay"
    );

    let env = MockEnv::new()
        .with("AUTUMN_SECURITY__WEBHOOKS__REPLAY__BACKEND", "redis")
        .with(
            "AUTUMN_SECURITY__WEBHOOKS__REPLAY__REDIS__URL",
            "redis://redis:6379/5",
        )
        .with(
            "AUTUMN_SECURITY__WEBHOOKS__REPLAY__REDIS__KEY_PREFIX",
            "env:webhooks:replay",
        )
        .with(
            "AUTUMN_SECURITY__WEBHOOKS__REPLAY__ALLOW_MEMORY_IN_PRODUCTION",
            "true",
        );
    let mut config = AutumnConfig::default();
    config.apply_env_overrides_with_env(&env);
    assert_eq!(
        config.security.webhooks.replay.backend,
        WebhookReplayBackend::Redis
    );
    assert_eq!(
        config.security.webhooks.replay.redis.url.as_deref(),
        Some("redis://redis:6379/5")
    );
    assert_eq!(
        config.security.webhooks.replay.redis.key_prefix,
        "env:webhooks:replay"
    );
    assert!(config.security.webhooks.replay.allow_memory_in_production);
}

#[cfg(not(feature = "redis"))]
#[test]
fn redis_replay_backend_requires_redis_feature() {
    let mut config = webhook_config(vec![endpoint(
        WebhookProvider::Github,
        "/webhooks/github",
        "github",
    )]);
    config.security.webhooks.replay.backend = WebhookReplayBackend::Redis;
    config.security.webhooks.replay.redis.url = Some("redis://localhost:6379/0".to_owned());

    let error = config
        .validate()
        .expect_err("redis replay backend must require redis feature");
    assert!(
        error.to_string().contains("redis") && error.to_string().contains("feature"),
        "{error}"
    );
}

#[cfg(feature = "redis")]
#[test]
fn redis_replay_backend_requires_url() {
    let mut config = webhook_config(vec![endpoint(
        WebhookProvider::Github,
        "/webhooks/github",
        "github",
    )]);
    config.security.webhooks.replay.backend = WebhookReplayBackend::Redis;

    let error = config
        .validate()
        .expect_err("redis replay backend must require a url");
    assert!(
        error.to_string().contains("redis") && error.to_string().contains("url"),
        "{error}"
    );
}

#[test]
fn disabled_replay_endpoint_does_not_require_replay_backend_configuration() {
    let mut config = autumn_web::webhook::WebhookConfig {
        replay: autumn_web::webhook::WebhookReplayConfig {
            backend: WebhookReplayBackend::Redis,
            ..Default::default()
        },
        endpoints: vec![
            endpoint(WebhookProvider::Github, "/webhooks/github", "github")
                .without_replay_protection(),
        ],
    };

    config
        .validate(false)
        .expect("disabled replay should not validate the replay backend");
    WebhookRegistry::from_config(&config)
        .expect("disabled replay should not construct the unused replay backend");

    config.endpoints[0].replay_protection = true;
    let error = config
        .validate(false)
        .expect_err("enabled replay should validate the configured backend");
    assert!(
        error.to_string().contains("redis"),
        "unexpected validation error: {error}"
    );
}

#[test]
fn webhook_secret_can_be_sourced_from_environment() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(
        dir.path().join("autumn.toml"),
        r#"
            [security.webhooks]

            [[security.webhooks.endpoints]]
            name = "stripe"
            path = "/webhooks/stripe"
            provider = "stripe"
            secret_env = "STRIPE_WEBHOOK_SECRET"
            previous_secret_envs = ["STRIPE_WEBHOOK_SECRET_PREVIOUS"]
        "#,
    )
    .expect("write config");

    let env = MockEnv::new()
        .with("AUTUMN_ENV", "test")
        .with("AUTUMN_MANIFEST_DIR", dir.path().to_str().unwrap())
        .with("STRIPE_WEBHOOK_SECRET", CURRENT_SECRET)
        .with("STRIPE_WEBHOOK_SECRET_PREVIOUS", PREVIOUS_SECRET);

    let config = AutumnConfig::load_with_env(&env).expect("config should load");
    let endpoint = &config.security.webhooks.endpoints[0];
    assert_eq!(endpoint.secret.as_deref(), Some(CURRENT_SECRET));
    assert_eq!(endpoint.previous_secrets, vec![PREVIOUS_SECRET.to_owned()]);
}

#[tokio::test(flavor = "current_thread")]
async fn toml_provider_preset_applies_signature_header_defaults() {
    let _guard = TEST_LOCK.lock().await;
    HANDLER_CALLS.store(0, Ordering::SeqCst);
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(
        dir.path().join("autumn.toml"),
        r#"
            [security.webhooks]

            [[security.webhooks.endpoints]]
            name = "stripe"
            path = "/webhooks/stripe"
            provider = "stripe"
            secret_env = "STRIPE_WEBHOOK_SECRET"
        "#,
    )
    .expect("write config");
    let env = MockEnv::new()
        .with("AUTUMN_ENV", "test")
        .with("AUTUMN_MANIFEST_DIR", dir.path().to_str().unwrap())
        .with("STRIPE_WEBHOOK_SECRET", CURRENT_SECRET);
    let config = AutumnConfig::load_with_env(&env).expect("config should load");
    let client = TestApp::new()
        .config(config)
        .routes(routes![stripe_webhook])
        .build();
    let now = unix_now();
    let body = br#"{"id":"evt_toml","type":"invoice.paid"}"#;

    let response = client
        .post("/webhooks/stripe")
        .header("content-type", "application/json")
        .header(
            "stripe-signature",
            &stripe_signature(CURRENT_SECRET, now, body),
        )
        .body(body.as_slice())
        .send()
        .await;

    response.assert_ok();
    assert_eq!(HANDLER_CALLS.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn replay_attack_with_modified_delivery_id_is_rejected() {
    let _guard = TEST_LOCK.lock().await;
    HANDLER_CALLS.store(0, Ordering::SeqCst);
    let client = client(vec![endpoint(
        WebhookProvider::Github,
        "/webhooks/github",
        "github",
    )]);
    let body = br#"{"action":"opened"}"#;
    let signature = github_signature(CURRENT_SECRET, body);

    let first = client
        .post("/webhooks/github")
        .header("x-hub-signature-256", &signature)
        .header("x-github-delivery", "delivery-1")
        .header("x-github-event", "pull_request")
        .body(body.as_slice())
        .send()
        .await;
    first.assert_ok();

    // Replay the same body & signature, but with a different delivery ID.
    // This should still be rejected as a duplicate because the body (signature) has not changed.
    let second = client
        .post("/webhooks/github")
        .header("x-hub-signature-256", &signature)
        .header("x-github-delivery", "delivery-2")
        .header("x-github-event", "pull_request")
        .body(body.as_slice())
        .send()
        .await;
    let json = problem_json(&second, 409);
    assert!(
        json["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("duplicate"))
    );
    assert_eq!(HANDLER_CALLS.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn webhook_endpoints_exempt_from_csrf() {
    let _guard = TEST_LOCK.lock().await;
    HANDLER_CALLS.store(0, Ordering::SeqCst);

    let endpoints = vec![endpoint(
        WebhookProvider::Stripe,
        "/webhooks/stripe",
        "stripe",
    )];
    let mut config = webhook_config(endpoints);
    config.security.csrf.enabled = true;

    let client = TestApp::new()
        .config(config)
        .routes(routes![stripe_webhook])
        .build();

    let timestamp = unix_now();
    let body = br#"{"id":"evt_123"}"#;
    let signature = stripe_signature(CURRENT_SECRET, timestamp, body);

    client
        .post("/webhooks/stripe")
        .header("stripe-signature", &signature)
        .body(body.as_slice())
        .send()
        .await
        .assert_ok();

    assert_eq!(HANDLER_CALLS.load(Ordering::SeqCst), 1);
}

#[post("/webhooks/failing")]
async fn failing_webhook(_webhook: SignedWebhook) -> impl IntoResponse {
    HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
    StatusCode::INTERNAL_SERVER_ERROR
}

#[tokio::test]
async fn webhook_replay_key_released_on_failure() {
    let _guard = TEST_LOCK.lock().await;
    HANDLER_CALLS.store(0, Ordering::SeqCst);

    let endpoints = vec![endpoint(
        WebhookProvider::Github,
        "/webhooks/failing",
        "failing",
    )];
    let client = TestApp::new()
        .config(webhook_config(endpoints))
        .routes(routes![failing_webhook])
        .build();

    let body = br#"{"action":"opened"}"#;
    let signature = github_signature(CURRENT_SECRET, body);

    // 1. Send first request. It should hit the handler and fail with 500.
    client
        .post("/webhooks/failing")
        .header("x-hub-signature-256", &signature)
        .header("x-github-delivery", "delivery-fail")
        .header("x-github-event", "pull_request")
        .body(body.as_slice())
        .send()
        .await
        .assert_status(500);

    assert_eq!(HANDLER_CALLS.load(Ordering::SeqCst), 1);

    // 2. Since it failed with 500, the replay key must be released.
    // Send the duplicate request. It should hit the handler again (returning 500)
    // instead of being rejected as a duplicate (which would return 409).
    client
        .post("/webhooks/failing")
        .header("x-hub-signature-256", &signature)
        .header("x-github-delivery", "delivery-fail")
        .header("x-github-event", "pull_request")
        .body(body.as_slice())
        .send()
        .await
        .assert_status(500);

    assert_eq!(HANDLER_CALLS.load(Ordering::SeqCst), 2);
}

#[post("/webhooks/panicking")]
async fn panicking_webhook(_webhook: SignedWebhook) -> impl IntoResponse {
    HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
    assert!(
        !std::convert::identity(true),
        "intentional panic in webhook handler"
    );
    StatusCode::OK
}

#[tokio::test]
async fn webhook_replay_key_released_on_panic() {
    let _guard = TEST_LOCK.lock().await;
    HANDLER_CALLS.store(0, Ordering::SeqCst);

    let endpoints = vec![endpoint(
        WebhookProvider::Github,
        "/webhooks/panicking",
        "panicking",
    )];
    let client = TestApp::new()
        .config(webhook_config(endpoints))
        .routes(routes![panicking_webhook])
        .build();

    let body = br#"{"action":"opened"}"#;
    let signature = github_signature(CURRENT_SECRET, body);

    // 1. Send first request. It should hit the handler and panic.
    // Axum/ReportingLayer will catch the panic and return 500.
    client
        .post("/webhooks/panicking")
        .header("x-hub-signature-256", &signature)
        .header("x-github-delivery", "delivery-panic")
        .header("x-github-event", "pull_request")
        .body(body.as_slice())
        .send()
        .await
        .assert_status(500);

    assert_eq!(HANDLER_CALLS.load(Ordering::SeqCst), 1);

    // Give background spawned cleanup task a chance to run
    tokio::task::yield_now().await;

    // 2. Since it panicked, the replay key must be released.
    // Send the duplicate request. It should hit the handler again (and panic/return 500)
    // instead of being rejected as a duplicate (which would return 409).
    client
        .post("/webhooks/panicking")
        .header("x-hub-signature-256", &signature)
        .header("x-github-delivery", "delivery-panic")
        .header("x-github-event", "pull_request")
        .body(body.as_slice())
        .send()
        .await
        .assert_status(500);

    assert_eq!(HANDLER_CALLS.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn test_duplicate_id_replay_same_id_different_sig_rejected() {
    let _guard = TEST_LOCK.lock().await;
    HANDLER_CALLS.store(0, Ordering::SeqCst);
    let client = client(vec![endpoint(
        WebhookProvider::Github,
        "/webhooks/github",
        "github",
    )]);
    let body1 = br#"{"action":"opened"}"#;
    let signature1 = github_signature(CURRENT_SECRET, body1);

    let first = client
        .post("/webhooks/github")
        .header("x-hub-signature-256", &signature1)
        .header("x-github-delivery", "same-id")
        .header("x-github-event", "pull_request")
        .body(body1.as_slice())
        .send()
        .await;
    first.assert_ok();

    let body2 = br#"{"action":"synchronize"}"#;
    let signature2 = github_signature(CURRENT_SECRET, body2);

    let second = client
        .post("/webhooks/github")
        .header("x-hub-signature-256", &signature2)
        .header("x-github-delivery", "same-id")
        .header("x-github-event", "pull_request")
        .body(body2.as_slice())
        .send()
        .await;
    let json = problem_json(&second, 409);
    assert!(
        json["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("duplicate"))
    );
    assert_eq!(HANDLER_CALLS.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn test_modified_id_replay_different_id_same_sig_rejected() {
    let _guard = TEST_LOCK.lock().await;
    HANDLER_CALLS.store(0, Ordering::SeqCst);
    let client = client(vec![endpoint(
        WebhookProvider::Github,
        "/webhooks/github",
        "github",
    )]);
    let body = br#"{"action":"opened"}"#;
    let signature = github_signature(CURRENT_SECRET, body);

    let first = client
        .post("/webhooks/github")
        .header("x-hub-signature-256", &signature)
        .header("x-github-delivery", "id-1")
        .header("x-github-event", "pull_request")
        .body(body.as_slice())
        .send()
        .await;
    first.assert_ok();

    let second = client
        .post("/webhooks/github")
        .header("x-hub-signature-256", &signature)
        .header("x-github-delivery", "id-2")
        .header("x-github-event", "pull_request")
        .body(body.as_slice())
        .send()
        .await;
    let json = problem_json(&second, 409);
    assert!(
        json["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("duplicate"))
    );
    assert_eq!(HANDLER_CALLS.load(Ordering::SeqCst), 1);
}
