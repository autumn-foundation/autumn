//! The built-in Web Push routes (`autumn_web::push::router`) — issue #1392.
//!
//! These exercise the routes end to end through `TestApp`: the browser's
//! `PushSubscription` JSON goes in over HTTP, and the resulting subscription
//! is proven by sending to it and inspecting what the transport received.

use autumn_web::push::{
    MemoryPushSubscriptionStore, PushMessage, RecordingPushTransport, VapidKey, WebPush,
};
use autumn_web::test::TestApp;
use serde_json::json;

const P256DH: &str =
    "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4";
const AUTH: &str = "BTBZMqHH6r4Tts7J_aSIgg";

fn subscription_json(endpoint: &str) -> serde_json::Value {
    json!({
        "endpoint": endpoint,
        // Browsers include this; the framework must ignore it rather than
        // reject the payload.
        "expirationTime": serde_json::Value::Null,
        "keys": { "p256dh": P256DH, "auth": AUTH },
    })
}

/// A `TestApp` mounting the built-in router over a `WebPush` whose transport
/// records everything, plus a test-only route that signs a caller in.
fn app_with_push(
    transport: RecordingPushTransport,
    vapid: VapidKey,
) -> (TestApp, autumn_web::push::WebPush) {
    let push = WebPush::new(
        MemoryPushSubscriptionStore::new(),
        vapid,
        "mailto:ops@example.com",
        transport,
    );
    let app = TestApp::new()
        .merge(autumn_web::push::router())
        .with_web_push(push.clone());
    (app, push)
}

/// The built-in router over a recording transport, with the client already
/// signed in as user 7 via the framework's own test sign-in seam.
async fn signed_in_client(
    transport: RecordingPushTransport,
    vapid: VapidKey,
) -> (autumn_web::test::TestClient, autumn_web::push::WebPush) {
    let (app, push) = app_with_push(transport, vapid);
    let client = app.build();
    client.acting_as(7).await;
    (client, push)
}

#[tokio::test]
async fn vapid_public_key_endpoint_serves_the_application_server_key() {
    let vapid = VapidKey::generate();
    let (app, _) = app_with_push(RecordingPushTransport::new(), vapid.clone());
    let client = app.build();

    let response = client.get("/push/vapid-public-key").send().await;
    response.assert_status(200);
    let body = response.text();
    assert_eq!(
        body.trim(),
        vapid.public_key_base64url(),
        "the client needs exactly this string for `applicationServerKey`"
    );
}

#[tokio::test]
async fn vapid_public_key_endpoint_is_public() {
    // The subscribe snippet fetches this before the user has done anything;
    // requiring auth would break the first-visit subscribe flow. The value is
    // public key material, so serving it to anyone is correct.
    let (app, _) = app_with_push(RecordingPushTransport::new(), VapidKey::generate());
    app.build()
        .get("/push/vapid-public-key")
        .send()
        .await
        .assert_status(200);
}

#[tokio::test]
async fn vapid_public_key_endpoint_is_503_when_push_is_unconfigured() {
    // Not a 200 with an empty body: the client must be able to tell "no key
    // configured" apart from "here is your key".
    let push = WebPush::without_vapid_key(
        MemoryPushSubscriptionStore::new(),
        "mailto:ops@example.com",
        RecordingPushTransport::new(),
    );
    let client = TestApp::new()
        .merge(autumn_web::push::router())
        .with_web_push(push)
        .build();
    client
        .get("/push/vapid-public-key")
        .send()
        .await
        .assert_status(503);
}

#[tokio::test]
async fn subscribe_records_the_subscription_for_the_signed_in_principal() {
    let transport = RecordingPushTransport::new();
    let (client, push) = signed_in_client(transport.clone(), VapidKey::generate()).await;

    client
        .post("/push/subscribe")
        .json(&subscription_json("https://push.example.com/abc"))
        .send()
        .await
        .assert_status(204);

    let report = push
        .send(7_i64, &PushMessage::new("Hi", "There"))
        .await
        .expect("send");
    assert_eq!(report.delivered, 1);
    assert_eq!(
        transport.requests()[0].endpoint,
        "https://push.example.com/abc"
    );
}

#[tokio::test]
async fn subscribe_requires_a_signed_in_caller() {
    // Without a principal there is nobody to bind the subscription to, and
    // guessing (e.g. by IP) would let an anonymous visitor receive another
    // user's notifications.
    let (app, _) = app_with_push(RecordingPushTransport::new(), VapidKey::generate());
    app.build()
        .post("/push/subscribe")
        .json(&subscription_json("https://push.example.com/abc"))
        .send()
        .await
        .assert_status(401);
}

#[tokio::test]
async fn subscribe_rejects_a_malformed_payload_with_a_400() {
    let (client, _) = signed_in_client(RecordingPushTransport::new(), VapidKey::generate()).await;
    client
        .post("/push/subscribe")
        .json(&json!({
            "endpoint": "https://push.example.com/abc",
            "keys": { "p256dh": "not-a-key", "auth": AUTH },
        }))
        .send()
        .await
        .assert_status(400);
}

#[tokio::test]
async fn subscribe_rejects_an_ssrf_shaped_endpoint_with_a_400() {
    // The endpoint is client-supplied and the framework later POSTs to it.
    let (client, _) = signed_in_client(RecordingPushTransport::new(), VapidKey::generate()).await;
    client
        .post("/push/subscribe")
        .json(&subscription_json(
            "https://169.254.169.254/latest/meta-data",
        ))
        .send()
        .await
        .assert_status(400);
}

#[tokio::test]
async fn subscribing_twice_is_idempotent() {
    // The recommended client pattern re-subscribes on every page load.
    let transport = RecordingPushTransport::new();
    let (client, push) = signed_in_client(transport.clone(), VapidKey::generate()).await;

    for _ in 0..3 {
        client
            .post("/push/subscribe")
            .json(&subscription_json("https://push.example.com/abc"))
            .send()
            .await
            .assert_status(204);
    }

    assert_eq!(
        push.send(7_i64, &PushMessage::new("Hi", "There"))
            .await
            .expect("send")
            .delivered,
        1,
        "re-subscribing must not create a second row (and a duplicate notification)"
    );
}

#[tokio::test]
async fn unsubscribe_removes_the_callers_subscription() {
    let transport = RecordingPushTransport::new();
    let (client, push) = signed_in_client(transport.clone(), VapidKey::generate()).await;

    client
        .post("/push/subscribe")
        .json(&subscription_json("https://push.example.com/abc"))
        .send()
        .await
        .assert_status(204);
    client
        .post("/push/unsubscribe")
        .json(&json!({ "endpoint": "https://push.example.com/abc" }))
        .send()
        .await
        .assert_status(204);

    assert_eq!(
        push.send(7_i64, &PushMessage::new("Hi", "There"))
            .await
            .expect("send")
            .delivered,
        0
    );
}

#[tokio::test]
async fn unsubscribe_requires_a_signed_in_caller() {
    let (app, _) = app_with_push(RecordingPushTransport::new(), VapidKey::generate());
    app.build()
        .post("/push/unsubscribe")
        .json(&json!({ "endpoint": "https://push.example.com/abc" }))
        .send()
        .await
        .assert_status(401);
}

#[tokio::test]
async fn unsubscribing_something_never_subscribed_is_still_204() {
    // Idempotent: a client that unsubscribes on sign-out must not see an error
    // just because permission had already been revoked in the browser.
    let (client, _) = signed_in_client(RecordingPushTransport::new(), VapidKey::generate()).await;
    client
        .post("/push/unsubscribe")
        .json(&json!({ "endpoint": "https://push.example.com/never" }))
        .send()
        .await
        .assert_status(204);
}

#[tokio::test]
async fn one_user_cannot_unsubscribe_anothers_device() {
    let transport = RecordingPushTransport::new();
    let (app, push) = app_with_push(transport.clone(), VapidKey::generate());

    push.subscribe(
        7_i64,
        &serde_json::from_value(subscription_json("https://push.example.com/victim"))
            .expect("payload"),
    )
    .await
    .expect("subscribe");

    // A *different* user asks to unsubscribe user 7's endpoint.
    let client = app.build();
    client.acting_as(9).await;
    client
        .post("/push/unsubscribe")
        .json(&json!({ "endpoint": "https://push.example.com/victim" }))
        .send()
        .await
        .assert_status(204);

    assert_eq!(
        push.send(7_i64, &PushMessage::new("Hi", "There"))
            .await
            .expect("send")
            .delivered,
        1,
        "user 9 must not have been able to remove user 7's device"
    );
}

// ── Configuration wiring ────────────────────────────────────────────────────

/// A `[push]` block with a valid key resolves a working service with no
/// application wiring at all — the extractor path a real app takes.
///
/// This is also the regression guard for a deadlock: resolving the service
/// *inside* `AppState::extension_or_insert_with`'s closure takes a read lock
/// on the extensions map that the helper already holds for writing, and the
/// request hangs forever. Every other test in this file registers a service
/// explicitly and so never reaches that path — only a test that lets the
/// extractor resolve on its own can catch it.
#[tokio::test]
async fn a_configured_push_block_resolves_a_working_service() {
    let vapid = VapidKey::generate();
    let mut config = autumn_web::config::AutumnConfig::default();
    config.push.private_key = Some(vapid.private_key_base64url().into());
    config.push.subject = Some("mailto:ops@example.com".to_owned());

    let client = TestApp::new()
        .merge(autumn_web::push::router())
        .config(config)
        .build();

    let response = client.get("/push/vapid-public-key").send().await;
    response.assert_status(200);
    assert_eq!(
        response.text().trim(),
        vapid.public_key_base64url(),
        "the key served to the browser must be the one from `[push] private_key`"
    );
}

/// With no `[push]` block the app still boots and the routes still exist —
/// they just report that push is unconfigured.
#[tokio::test]
async fn an_unconfigured_app_still_boots_and_reports_unconfigured() {
    let client = TestApp::new().merge(autumn_web::push::router()).build();
    let response = client.get("/push/vapid-public-key").send().await;
    response.assert_status(503);
    assert!(
        response.text().contains("private_key"),
        "the response must say how to configure push: {}",
        response.text()
    );
}

/// The boot-time guard `AppBuilder::run` applies before binding.
///
/// `run` exits the process on failure, which no in-process test can observe —
/// so the rule itself lives in `AutumnConfig::validate_push`, and `run` calls
/// exactly that. This pins the rule; the one-line call in `app.rs` is what
/// connects it to the boot.
#[test]
fn boot_validation_refuses_a_push_block_that_cannot_work() {
    let good = VapidKey::generate();

    let mut valid = autumn_web::config::AutumnConfig::default();
    valid.push.private_key = Some(good.private_key_base64url().into());
    valid.validate_push().expect("a valid key boots");

    let mut absent = autumn_web::config::AutumnConfig::default();
    absent.push.private_key = None;
    absent
        .validate_push()
        .expect("an app with no [push] block must still boot");

    // Each of these would otherwise start cleanly, accept subscriptions, and
    // silently never deliver anything.
    let mut typo = autumn_web::config::AutumnConfig::default();
    typo.push.private_key = Some("obviously-not-a-key".to_owned().into());
    typo.validate_push()
        .expect_err("an invalid key fails the boot");

    let mut blank = autumn_web::config::AutumnConfig::default();
    blank.push.private_key = Some(String::new().into());
    blank
        .validate_push()
        .expect_err("an env var that failed to interpolate fails the boot");

    let mut mismatched = autumn_web::config::AutumnConfig::default();
    mismatched.push.private_key = Some(good.private_key_base64url().into());
    mismatched.push.public_key = Some(VapidKey::generate().public_key_base64url());
    mismatched
        .validate_push()
        .expect_err("a mismatched key pair fails the boot");
}

/// A store registered through the builder pre-empts the default resolution.
#[tokio::test]
async fn a_registered_subscription_store_is_the_one_the_extractor_uses() {
    let vapid = VapidKey::generate();
    let mut config = autumn_web::config::AutumnConfig::default();
    config.push.private_key = Some(vapid.private_key_base64url().into());

    let client = TestApp::new()
        .merge(autumn_web::push::router())
        .config(config)
        // The resolved service must pick this store up rather than falling
        // back to a fresh in-memory one.
        .with_push_subscription_store(MemoryPushSubscriptionStore::new())
        .build();
    client.acting_as(7).await;

    client
        .post("/push/subscribe")
        .json(&subscription_json("https://push.example.com/registered"))
        .send()
        .await
        .assert_status(204);

    // The configured key still reaches the service, so registering a store
    // does not discard the rest of the resolved configuration.
    let response = client.get("/push/vapid-public-key").send().await;
    response.assert_status(200);
    assert_eq!(response.text().trim(), vapid.public_key_base64url());
}

/// Re-subscribing an endpoint another user owns, with different keys, is a
/// refusal — not a silent takeover.
#[tokio::test]
async fn one_user_cannot_claim_anothers_endpoint_over_http() {
    let transport = RecordingPushTransport::new();
    let (app, push) = app_with_push(transport, VapidKey::generate());
    let client = app.build();

    client.acting_as(7).await;
    client
        .post("/push/subscribe")
        .json(&subscription_json("https://push.example.com/victim"))
        .send()
        .await
        .assert_status(204);

    // A different user submits the same endpoint with keys they control.
    client.acting_as(9).await;
    let mut hostile = subscription_json("https://push.example.com/victim");
    hostile["keys"]["p256dh"] = serde_json::Value::String(
        "BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8"
            .to_owned(),
    );
    client
        .post("/push/subscribe")
        .json(&hostile)
        .send()
        .await
        .assert_status(409);

    assert_eq!(
        push.send(7_i64, &PushMessage::new("Hi", "There"))
            .await
            .expect("send")
            .delivered,
        1,
        "the original owner must still receive"
    );
}

/// The public-key response must never be held by a shared cache.
#[tokio::test]
async fn the_vapid_public_key_response_is_not_cacheable() {
    let (app, _) = app_with_push(RecordingPushTransport::new(), VapidKey::generate());
    let response = app.build().get("/push/vapid-public-key").send().await;
    response.assert_status(200);
    assert_eq!(
        response.header("cache-control"),
        Some("no-store"),
        "a key rotation must not be masked by a cached response"
    );
}

/// `AUTUMN_PUSH__PRIVATE_KEY` is the deployment path the guide documents, so
/// it has to actually reach the config.
///
/// Overrides are applied only through the explicit per-section methods
/// `apply_env_overrides_with_env` calls; a new section that forgets to add one
/// leaves every documented variable inert, and the operator is left looking at
/// a variable they did set while every send reports "not configured".
#[test]
fn push_settings_can_be_supplied_through_the_environment() {
    use autumn_web::config::AutumnConfig;
    use secrecy::ExposeSecret as _;

    let key = VapidKey::generate();
    let vars = [
        ("AUTUMN_PUSH__PRIVATE_KEY", key.private_key_base64url()),
        ("AUTUMN_PUSH__PUBLIC_KEY", key.public_key_base64url()),
        ("AUTUMN_PUSH__SUBJECT", "mailto:ops@example.com".to_owned()),
        ("AUTUMN_PUSH__TTL_SECS", "600".to_owned()),
    ];

    temp_env::with_vars(
        vars.iter()
            .map(|(k, v)| (*k, Some(v.as_str())))
            .collect::<Vec<_>>(),
        || {
            let mut config = AutumnConfig::default();
            config.apply_env_overrides();

            assert_eq!(
                config
                    .push
                    .private_key
                    .as_ref()
                    .map(|k| k.expose_secret().to_owned()),
                Some(key.private_key_base64url()),
                "the documented private-key variable must reach the config"
            );
            assert_eq!(
                config.push.public_key.as_deref(),
                Some(key.public_key_base64url().as_str())
            );
            assert_eq!(
                config.push.subject.as_deref(),
                Some("mailto:ops@example.com")
            );
            assert_eq!(config.push.ttl_secs, Some(600));

            // And the whole point: it produces a working, bootable key.
            config.validate_push().expect("boots");
            assert_eq!(
                config
                    .push
                    .load_vapid_key()
                    .expect("load")
                    .expect("present")
                    .public_key_base64url(),
                key.public_key_base64url()
            );
        },
    );
}

/// The public-key response carries the caller's CSRF token, because the
/// subscribe snippet has no other way to get one: the CSRF cookie is
/// `HttpOnly`, and the generated snippet runs on pages rendered by a
/// `layout()` the PWA generator does not change the signature of.
#[tokio::test]
async fn the_public_key_response_carries_a_csrf_token_for_the_snippet() {
    let vapid = VapidKey::generate();
    let mut config = autumn_web::config::AutumnConfig::default();
    config.push.private_key = Some(vapid.private_key_base64url().into());
    config.security.csrf.enabled = true;

    let client = TestApp::new()
        .merge(autumn_web::push::router())
        .config(config)
        .build();

    let response = client.get("/push/vapid-public-key").send().await;
    response.assert_status(200);

    let token = response
        .header(autumn_web::push::CSRF_TOKEN_HEADER)
        .expect("the response must carry a CSRF token when CSRF is enabled");
    assert!(!token.is_empty(), "an empty token is no token");
    // Compared case-insensitively: the framework normalizes the configured
    // name to lowercase (`x-csrf-token`), and HTTP header names are
    // case-insensitive anyway — what matters is that the snippet is told which
    // name to use rather than guessing.
    assert_eq!(
        response
            .header(autumn_web::push::CSRF_TOKEN_HEADER_NAME_HEADER)
            .map(str::to_ascii_lowercase),
        Some("x-csrf-token".to_owned()),
        "the snippet needs the configured header name, not a guess"
    );
}

/// Subscribing with that token succeeds where an unaccompanied POST is
/// rejected — the end-to-end shape of the fix.
#[tokio::test]
async fn subscribe_succeeds_with_the_token_from_the_public_key_response() {
    let vapid = VapidKey::generate();
    let mut config = autumn_web::config::AutumnConfig::default();
    config.push.private_key = Some(vapid.private_key_base64url().into());
    config.security.csrf.enabled = true;

    let client = TestApp::new()
        .merge(autumn_web::push::router())
        .config(config)
        .build();
    client.acting_as(7).await;

    let key_response = client.get("/push/vapid-public-key").send().await;
    let token = key_response
        .header(autumn_web::push::CSRF_TOKEN_HEADER)
        .expect("token")
        .to_owned();
    let header_name = key_response
        .header(autumn_web::push::CSRF_TOKEN_HEADER_NAME_HEADER)
        .expect("header name")
        .to_owned();

    client
        .post("/push/subscribe")
        .header(&header_name, &token)
        .json(&subscription_json("https://push.example.com/csrf"))
        .send()
        .await
        .assert_status(204);
}
