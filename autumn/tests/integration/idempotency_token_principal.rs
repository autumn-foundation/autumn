//! Bearer-token clients under `.idempotent()`.
//!
//! A token client has no cookie-backed session, so the idempotency storage key
//! used to carry no principal for it at all: two token clients that sent the
//! same `Idempotency-Key` and body shared one cache slot. The key now carries
//! the bearer scope — the principal `RequireApiToken` verified, or a digest of
//! the raw `Authorization` header when the handler checks the credential
//! itself — so each client replays only its own response.
//!
//! `RequireApiToken` used to force `fail_closed_on_replay()` app-wide, which
//! kept the leak shut but turned every legitimate same-client retry into a
//! `409`. It is now on `router.rs`'s idempotency-transparent allowlist, so a
//! retry replays.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use autumn_web::auth::{
    ApiToken, ApiTokenStore, InMemoryApiTokenStore, IssueTokenSpec, RequireApiToken,
};
use autumn_web::reexports::axum::http::HeaderMap;
use autumn_web::test::{TestApp, TestClient};
use autumn_web::{AutumnResult, post, routes};

static VERIFIED_ORDER_CALLS: AtomicUsize = AtomicUsize::new(0);
static HANDLER_AUTH_CALLS: AtomicUsize = AtomicUsize::new(0);
static OPTED_OUT_CALLS: AtomicUsize = AtomicUsize::new(0);

/// Body echoes the calling token's principal so a cross-principal replay is
/// visible in the response text.
#[post("/orders")]
#[autumn_web::secured(scopes = ["orders:write"])]
async fn create_order(ApiToken(principal): ApiToken) -> AutumnResult<String> {
    VERIFIED_ORDER_CALLS.fetch_add(1, Ordering::SeqCst);
    Ok(format!("order-for-{principal}"))
}

/// Checks the credential inside the handler, the way a durable workflow
/// route resolves its own scope. No layer has verified anything before the
/// idempotency layer computes its key.
#[post("/handler-auth")]
async fn handler_auth(headers: HeaderMap) -> String {
    HANDLER_AUTH_CALLS.fetch_add(1, Ordering::SeqCst);
    let caller = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("anonymous");
    format!("run-for-{caller}")
}

/// Dedupes by its own key, so it opts out of the app-level layer.
#[post("/opted-out", idempotent = false)]
async fn opted_out() -> String {
    let call = OPTED_OUT_CALLS.fetch_add(1, Ordering::SeqCst);
    format!("call-{call}")
}

async fn issue(store: &InMemoryApiTokenStore, principal: &str, name: &str) -> String {
    store
        .issue_scoped(IssueTokenSpec {
            principal_id: principal,
            name,
            scopes: &["orders:write".to_owned()],
            expires_at: None,
        })
        .await
        .expect("token issuance must succeed")
}

async fn post_with(
    app: &TestClient,
    path: &str,
    authorization: Option<&str>,
    key: &str,
) -> autumn_web::test::TestResponse {
    let mut request = app.post(path).header("idempotency-key", key).body("{}");
    if let Some(authorization) = authorization {
        request = request.header("authorization", authorization);
    }
    request.send().await
}

#[tokio::test]
async fn verified_token_principals_replay_only_their_own_response() {
    VERIFIED_ORDER_CALLS.store(0, Ordering::SeqCst);
    let store = Arc::new(InMemoryApiTokenStore::default());
    let app = TestApp::new()
        .idempotent()
        .routes(routes![create_order])
        .layer(RequireApiToken::new(store.clone()))
        .build();

    let token_a = format!("Bearer {}", issue(&store, "customer-a", "a").await);
    let token_b = format!("Bearer {}", issue(&store, "customer-b", "b").await);

    let first = post_with(&app, "/orders", Some(&token_a), "shared-key").await;
    first.assert_ok();
    assert_eq!(first.text(), "order-for-customer-a");
    assert_eq!(first.header("x-idempotent-replayed"), None);

    // Customer B sends the same key and body: a fresh run for B, never A's
    // stored answer.
    let other = post_with(&app, "/orders", Some(&token_b), "shared-key").await;
    other.assert_ok();
    assert_eq!(
        other.text(),
        "order-for-customer-b",
        "customer B must not receive customer A's cached response"
    );
    assert_eq!(other.header("x-idempotent-replayed"), None);

    // Customer A retries: a replay, not a 409 and not a second run.
    let retry = post_with(&app, "/orders", Some(&token_a), "shared-key").await;
    retry.assert_ok();
    assert_eq!(retry.text(), "order-for-customer-a");
    assert_eq!(
        retry.header("x-idempotent-replayed"),
        Some("true"),
        "a same-principal retry through RequireApiToken must replay"
    );
    assert_eq!(VERIFIED_ORDER_CALLS.load(Ordering::SeqCst), 2);
}

/// The key carries the verified principal, not the token, so an agent that
/// rotates its token between attempts still replays.
#[tokio::test]
async fn verified_principal_replays_across_token_rotation() {
    let store = Arc::new(InMemoryApiTokenStore::default());
    let app = TestApp::new()
        .idempotent()
        .routes(routes![create_order])
        .layer(RequireApiToken::new(store.clone()))
        .build();

    let old_token = format!("Bearer {}", issue(&store, "agent-1", "old").await);
    let new_token = format!("Bearer {}", issue(&store, "agent-1", "new").await);

    let first = post_with(&app, "/orders", Some(&old_token), "rotate-key").await;
    first.assert_ok();
    assert_eq!(first.header("x-idempotent-replayed"), None);

    let retry = post_with(&app, "/orders", Some(&new_token), "rotate-key").await;
    retry.assert_ok();
    assert_eq!(retry.text(), "order-for-agent-1");
    assert_eq!(retry.header("x-idempotent-replayed"), Some("true"));
}

/// A route that checks the credential in the handler has no verified
/// principal before the layer runs. The raw header's digest scopes the key.
#[tokio::test]
async fn handler_checked_bearer_tokens_do_not_share_a_cache_slot() {
    HANDLER_AUTH_CALLS.store(0, Ordering::SeqCst);
    let app = TestApp::new()
        .idempotent()
        .routes(routes![handler_auth])
        .build();

    let first = post_with(&app, "/handler-auth", Some("Bearer token-a"), "k").await;
    first.assert_ok();
    assert_eq!(first.text(), "run-for-Bearer token-a");

    let other = post_with(&app, "/handler-auth", Some("Bearer token-b"), "k").await;
    other.assert_ok();
    assert_eq!(
        other.text(),
        "run-for-Bearer token-b",
        "a second bearer client must not receive the first client's cached response"
    );
    assert_eq!(other.header("x-idempotent-replayed"), None);

    // A request with no credential at all must not land in a bearer slot.
    let anonymous = post_with(&app, "/handler-auth", None, "k").await;
    anonymous.assert_ok();
    assert_eq!(anonymous.text(), "run-for-anonymous");
    assert_eq!(anonymous.header("x-idempotent-replayed"), None);

    let retry = post_with(&app, "/handler-auth", Some("Bearer token-a"), "k").await;
    retry.assert_ok();
    assert_eq!(retry.text(), "run-for-Bearer token-a");
    assert_eq!(retry.header("x-idempotent-replayed"), Some("true"));
    assert_eq!(HANDLER_AUTH_CALLS.load(Ordering::SeqCst), 3);
}

/// `idempotent = false` keeps the app-level layer off the route: no store, no
/// replay, the handler runs every time.
#[tokio::test]
async fn idempotent_false_route_is_not_cached() {
    OPTED_OUT_CALLS.store(0, Ordering::SeqCst);
    let app = TestApp::new()
        .idempotent()
        .routes(routes![opted_out])
        .build();

    let first = post_with(&app, "/opted-out", Some("Bearer token-a"), "k").await;
    first.assert_ok();
    assert_eq!(first.text(), "call-0");

    let retry = post_with(&app, "/opted-out", Some("Bearer token-a"), "k").await;
    retry.assert_ok();
    assert_eq!(retry.text(), "call-1", "an opted-out route must run again");
    assert_eq!(retry.header("x-idempotent-replayed"), None);
}
