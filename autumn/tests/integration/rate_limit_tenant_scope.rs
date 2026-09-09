//! Cross-tenant rate-limit bucket collision (Warden 2026-09-09).
//!
//! Composes two documented, independent framework features — `[tenancy]
//! enabled = true` (`docs/guide/tenant-cells.md`) and
//! `key_strategy = "authenticated_principal"` / `#[throttle(key =
//! "principal")]` (`docs/guide/rate-limiting.md`) — and asserts that a
//! rate-limit bucket exhausted by one tenant's user never throttles a
//! *different* tenant's user, even when the two users' session-carried
//! principal identifiers happen to be identical.
//!
//! That collision is not a contrived edge case: Autumn's own sharding guide
//! documents that per-tenant primary keys are shard-local `BIGSERIAL`s, not
//! globally unique (`docs/guide/sharding.md`: "the shard-local `BIGSERIAL`
//! id is not copied, so re-runs never collide on the primary key" — i.e. two
//! tenants routed to two different shards each mint their own `1, 2, 3, ...`
//! sequence). The first user provisioned on tenant A's shard and the first
//! user provisioned on tenant B's shard are both, quite normally, `id = 1`.
//! An app that follows the framework's own documented login pattern
//! (`docs/guide/authentication.md`: `session.insert("user_id",
//! user.id.to_string())`) stores exactly that shard-local id as the
//! rate-limit principal — and the bucket key built from it
//! (`security::rate_limit::Limiter::extract_key`) folds in no tenant,
//! unlike every `tenant_scoped` repository operation, which resolves the
//! ambient `CURRENT_TENANT` task-local automatically.
//!
//! Both consumers of that key are covered here: the global tower layer
//! (`[security.rate_limit] key_strategy = "authenticated_principal"`) and
//! the per-route guard (`#[throttle(key = "principal")]`).

use autumn_web::config::AutumnConfig;
use autumn_web::security::KeyStrategy;
use autumn_web::test::TestApp;
use autumn_web::{get, routes, throttle};

fn tenancy_config() -> AutumnConfig {
    let mut config = AutumnConfig::default();
    config.tenancy.enabled = true;
    "header".clone_into(&mut config.tenancy.source);
    "x-tenant-id".clone_into(&mut config.tenancy.header_name);
    config
}

// ── Handlers ─────────────────────────────────────────────────────────────────

#[get("/rl-login/{user_id}")]
async fn rl_login(
    session: autumn_web::session::Session,
    autumn_web::extract::Path(user_id): autumn_web::extract::Path<String>,
) -> &'static str {
    session.insert("user_id", user_id).await;
    "ok"
}

#[get("/ping")]
async fn ping() -> &'static str {
    "pong"
}

#[get("/throttled-ping")]
#[throttle(limit = 1, per = "1s", key = "principal")]
async fn throttled_ping() -> &'static str {
    "pong"
}

/// Log in `shared-id` under two different tenants, returning
/// `(cookie_a, cookie_b)`. Modeling the shard-local id collision from the
/// module doc comment: both tenants' sessions carry the exact same
/// `user_id` value, as they would if each were their tenant's first
/// provisioned user on independent shards.
async fn login_both_tenants(client: &autumn_web::test::TestClient) -> (String, String) {
    let login_a = client
        .get("/rl-login/shared-id")
        .header("x-tenant-id", "tenant-a")
        .send()
        .await;
    login_a.assert_status(200);
    let cookie_a = login_a
        .header("set-cookie")
        .expect("login must set a session cookie")
        .to_owned();

    // Drop tenant A's jar cookie: `TestClient` carries a cookie jar, so
    // without this the tenant-B login replays tenant A's session cookie and
    // overwrites that one shared session's `user_id` instead of minting a
    // fresh one (see the identical caveat in rate_limit_principal.rs #1725).
    client.log_out();

    let login_b = client
        .get("/rl-login/shared-id")
        .header("x-tenant-id", "tenant-b")
        .send()
        .await;
    login_b.assert_status(200);
    let cookie_b = login_b
        .header("set-cookie")
        .expect("login must set a session cookie")
        .to_owned();

    (cookie_a, cookie_b)
}

// ── Global limiter: key_strategy = "authenticated_principal" ────────────────

#[tokio::test]
async fn global_limiter_principal_bucket_isolated_by_tenant() {
    let mut config = tenancy_config();
    config.security.rate_limit.enabled = true;
    config.security.rate_limit.requests_per_second = 0.1;
    config.security.rate_limit.burst = 1;
    config.security.rate_limit.trust_forwarded_headers = true;
    config.security.rate_limit.key_strategy = KeyStrategy::AuthenticatedPrincipal;

    let client = TestApp::new()
        .routes(routes![rl_login, ping])
        .config(config)
        .build();

    let (cookie_a, cookie_b) = login_both_tenants(&client).await;

    // Tenant A's user spends their one allotted token.
    client
        .get("/ping")
        .header("x-tenant-id", "tenant-a")
        .header("cookie", &cookie_a)
        .send()
        .await
        .assert_status(200);

    // Tenant A's own bucket is now exhausted.
    client
        .get("/ping")
        .header("x-tenant-id", "tenant-a")
        .header("cookie", &cookie_a)
        .send()
        .await
        .assert_status(429);

    // Tenant B's user has made ZERO requests of their own and belongs to a
    // completely different tenant. They must still get a fresh bucket.
    let tenant_b_response = client
        .get("/ping")
        .header("x-tenant-id", "tenant-b")
        .header("cookie", &cookie_b)
        .send()
        .await;
    assert_eq!(
        tenant_b_response.status, 200,
        "tenant B's user was denied service (status {}) purely because tenant A's user \
         happens to share the same session-carried principal id — the rate-limit bucket key \
         must fold in the resolved tenant",
        tenant_b_response.status
    );
}

// ── Per-route `#[throttle(key = "principal")]` ───────────────────────────────

#[tokio::test]
async fn per_route_throttle_principal_bucket_isolated_by_tenant() {
    // Isolate the process-global `#[throttle]` registry (see the identical
    // convention in throttle_route.rs, #1725): take TEST_LOCK first, then
    // reset, holding the guard for the whole test.
    let _throttle_lock = autumn_web::security::TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    autumn_web::security::__throttle_registry_reset();

    let mut config = tenancy_config();
    // The global limiter is irrelevant here; only the per-route guard's own
    // `key = "principal"` derivation is under test.
    config.security.rate_limit.enabled = false;

    let client = TestApp::new()
        .routes(routes![rl_login, throttled_ping])
        .config(config)
        .build();

    let (cookie_a, cookie_b) = login_both_tenants(&client).await;

    client
        .get("/throttled-ping")
        .header("x-tenant-id", "tenant-a")
        .header("cookie", &cookie_a)
        .send()
        .await
        .assert_status(200);

    client
        .get("/throttled-ping")
        .header("x-tenant-id", "tenant-a")
        .header("cookie", &cookie_a)
        .send()
        .await
        .assert_status(429);

    let tenant_b_response = client
        .get("/throttled-ping")
        .header("x-tenant-id", "tenant-b")
        .header("cookie", &cookie_b)
        .send()
        .await;
    assert_eq!(
        tenant_b_response.status, 200,
        "tenant B's user was denied service (status {}) by #[throttle(key = \"principal\")] \
         purely because tenant A's user happens to share the same session-carried principal \
         id — the per-route bucket key must fold in the resolved tenant",
        tenant_b_response.status
    );
}
