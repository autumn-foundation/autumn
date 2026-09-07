//! Regression: bearer-token principals must not cross-replay idempotent
//! responses. `RequireApiToken` is not on `router.rs`'s
//! `is_idempotency_transparent_app_layer` allowlist, so installing it forces
//! `fail_closed_on_replay()` app-wide — a same-key collision from a different
//! principal gets `409`, never a replayed body.

use std::sync::Arc;

use autumn_web::auth::{
    ApiToken, ApiTokenStore, InMemoryApiTokenStore, IssueTokenSpec, RequireApiToken,
};
use autumn_web::test::TestApp;
use autumn_web::{AutumnResult, post, routes};

/// Body echoes the calling token's principal so a cross-principal replay is
/// visible in the response text.
#[post("/orders")]
#[autumn_web::secured(scopes = ["orders:write"])]
async fn create_order(ApiToken(principal): ApiToken) -> AutumnResult<String> {
    Ok(format!("order-for-{principal}"))
}

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
    // `in_flight_conflict_response()` also returns 409 with no
    // `X-Idempotent-Replayed` header (if the first request's lock were still
    // held), so assert the fail-closed-on-replay path's distinctive body
    // rather than the status code alone.
    assert_eq!(
        second.status,
        http::StatusCode::CONFLICT,
        "expected the fail-closed-on-replay path, got: {:?}",
        second.text()
    );
    assert_eq!(
        second.text(),
        "idempotency replay requires an inner replay stop for this route"
    );
}
