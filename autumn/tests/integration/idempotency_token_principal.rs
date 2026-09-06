//! Cross-principal idempotency replay for bearer-token auth — negative result
//! (Warden 2026-09-06).
//!
//! Composes two documented, first-class framework features: `#[secured(scopes
//! = [...])]` scoped service tokens (`autumn_web::secured`'s own doc: "No
//! session is required for a scopes-only gate, so a pure service token
//! authorizes on scopes alone") and `AppBuilder::idempotent()`
//! (`docs/guide/idempotency.md`). Neither requires `SessionLayer` or
//! `[tenancy] enabled = true` — this is exactly how a token-only B2B API
//! (each customer holds its own bearer token; no cookie session ever exists)
//! is documented to compose the two features, mirroring
//! `scoped_tokens.rs`'s own `token_app` helper.
//!
//! ## Hypothesis
//!
//! `docs/guide/idempotency.md` states the general contract: "Cached
//! responses are scoped to a principal." The storage key computation
//! (`autumn::idempotency::build_storage_key`) only ever folds in the
//! cookie-backed session id and the framework-resolved tenant — never a
//! bearer token's identity. A request authenticated purely by scopes (no
//! session, no tenancy) contributes the exact same "empty principal"
//! component regardless of which token authenticated it
//! (`principal_scope_digest(None)` is a fixed digest, independent of the
//! token). Two *different* customers' tokens that happen to agree on the
//! client-supplied `Idempotency-Key` for the same route would then collide
//! on one storage key — naively suggesting customer B's request could be
//! answered with customer A's stored response body.
//!
//! ## Negative result
//!
//! It does not leak. `RequireApiToken` can only ever reach a request as an
//! `AppBuilder::layer()` (there is no other way to populate
//! `ApiTokenScopes`/`ApiToken` before the handler runs), and
//! `autumn::router::custom_layers_require_fail_closed_idempotency` treats
//! every custom layer as idempotency-opaque unless it is explicitly
//! whitelisted (today: only `SessionLayer` and the i18n bundle extension —
//! see `is_idempotency_transparent_app_layer`). Installing `RequireApiToken`
//! therefore flips `opaque_app_layers_present` for the whole app, which
//! forces `IdempotencyLayer::fail_closed_on_replay()` on every route
//! (`idempotency_layer_for_route`) instead of the normal
//! `replay_through_inner()` path — trading idempotent retries for safety: a
//! same-key collision from a *different* principal gets `409 Conflict`
//! ("idempotency replay requires an inner replay stop for this route"),
//! never a replayed body. The mechanism is coarser than principal-aware
//! storage-key partitioning (it fails closed for a genuine same-customer
//! retry too, so retries silently stop being idempotent app-wide the moment
//! any non-whitelisted layer is installed — a reliability cost, not a
//! leak), but it closes exactly the cross-principal read this hypothesis
//! predicted.
//!
//! Committed as a regression test — end-to-end through `TestApp`, rather
//! than only the pure-function unit tests already covering
//! `custom_layers_require_fail_closed_idempotency` in `router.rs` — so a
//! future change that whitelists `RequireApiToken` (or any other
//! principal-resolving layer) as "idempotency transparent" without also
//! teaching the storage key about its principal is caught immediately.

use std::sync::Arc;

use autumn_web::auth::{
    ApiToken, ApiTokenStore, InMemoryApiTokenStore, IssueTokenSpec, RequireApiToken,
};
use autumn_web::test::TestApp;
use autumn_web::{AutumnResult, post, routes};

/// Sentinel-bearing mutation: the body names the calling token's own
/// verified principal, so a replay across bearer principals would be visible
/// in the response text alone. Gated purely on scopes — no session
/// parameter, no session extractor anywhere in this file — matching the
/// documented "no session is required" scopes-only `#[secured]` form.
#[post("/orders")]
#[autumn_web::secured(scopes = ["orders:write"])]
async fn create_order(ApiToken(principal): ApiToken) -> AutumnResult<String> {
    Ok(format!("order-for-{principal}"))
}

/// Two different bearer-token principals — no session, no tenancy — that
/// happen to pick the same `Idempotency-Key` for the same route must never
/// share a cache slot: customer B must not receive customer A's stored
/// response. Pins the actual mechanism (fail-closed `409`, not principal-aware
/// replay) so this regression fails loudly, not just "vacuously passes for a
/// different reason", if the framework's answer here ever changes shape.
#[tokio::test]
async fn bearer_token_principals_do_not_replay_across_each_other() {
    let store = Arc::new(InMemoryApiTokenStore::default());

    let app = TestApp::new()
        .idempotent()
        .routes(routes![create_order])
        .layer(RequireApiToken::new(store.clone()))
        .build();

    let token_a = store
        .issue_scoped(IssueTokenSpec {
            principal_id: "customer-a",
            name: "customer-a-token",
            scopes: &["orders:write".to_owned()],
            expires_at: None,
        })
        .await
        .expect("token issuance must succeed");
    let token_b = store
        .issue_scoped(IssueTokenSpec {
            principal_id: "customer-b",
            name: "customer-b-token",
            scopes: &["orders:write".to_owned()],
            expires_at: None,
        })
        .await
        .expect("token issuance must succeed");

    let first = app
        .post("/orders")
        .header("authorization", &format!("Bearer {token_a}"))
        .header("idempotency-key", "shared-key")
        .body("{}")
        .send()
        .await;
    first.assert_ok();
    assert_eq!(
        first.text(),
        "order-for-customer-a",
        "customer A's own mutation runs normally"
    );

    let second = app
        .post("/orders")
        .header("authorization", &format!("Bearer {token_b}"))
        .header("idempotency-key", "shared-key")
        .body("{}")
        .send()
        .await;

    assert!(
        !second.text().contains("customer-a"),
        "customer B received customer A's cached response body: {:?} (status {})",
        second.text(),
        second.status
    );
    assert_ne!(
        second.header("x-idempotent-replayed"),
        Some("true"),
        "customer B's request must not be answered from customer A's cache slot"
    );
    // Pin the actual safety mechanism: `RequireApiToken` is not on the
    // idempotency-transparent layer allowlist, so the whole app fails closed
    // on a same-key collision (409) rather than partitioning the storage key
    // by principal. If a future change teaches the storage key about the
    // bearer principal instead, this assertion — not just the two above —
    // is the one that should be revisited.
    assert_eq!(
        second.status,
        http::StatusCode::CONFLICT,
        "expected the fail-closed-on-replay path (RequireApiToken is an opaque \
         custom layer), got: {:?}",
        second.text()
    );
}
