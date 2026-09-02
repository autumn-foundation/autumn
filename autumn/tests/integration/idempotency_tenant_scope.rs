//! Cross-tenant idempotency replay (Warden 2026-09-02).
//!
//! Composes two documented framework features — `[tenancy] enabled = true`
//! (`docs/guide/tenant-cells.md`) and `AppBuilder::idempotent()`
//! (`docs/guide/idempotency.md`) — and asserts that a cached mutation recorded
//! for one tenant is never replayed to a request that resolved to a *different*
//! tenant.
//!
//! The idempotency storage key namespaces by method, request target and the
//! cookie-backed session id. A framework-resolved tenant is none of those, so
//! before the fix two requests that differ *only* by tenant shared one cache
//! slot: the second tenant received the first tenant's stored response body and
//! its own handler — and therefore every `tenant_scoped` repository filter
//! inside it — never ran.
//!
//! The router already fails closed for this exact hazard when the tenant is
//! resolved by an app-supplied `AppBuilder::layer` (see
//! `idempotency_middleware::test_app_wide_generated_route_fails_closed_for_opaque_tenant_scope`).
//! These tests hold the framework's own tenancy middleware to the same bar.

use std::sync::atomic::{AtomicUsize, Ordering};

use autumn_web::config::AutumnConfig;
use autumn_web::session::Session;
use autumn_web::test::TestApp;
use autumn_web::{post, public, routes};

/// Sentinel-bearing mutation: the body names the tenant the request resolved
/// to, so a replay across tenants is visible in the response text alone.
#[post("/orders")]
#[public]
async fn create_order(tenant: autumn_web::tenancy::Tenant) -> String {
    format!("order-for-{}-sentinel", tenant.0)
}

fn tenancy_config(source: &str) -> AutumnConfig {
    let mut config = AutumnConfig::default();
    config.tenancy.enabled = true;
    source.clone_into(&mut config.tenancy.source);
    "x-tenant-id".clone_into(&mut config.tenancy.header_name);
    config.tenancy.base_domain = Some("example.test".to_owned());
    // Subdomain tenancy resolves the tenant from `Host`, so both tenant hosts
    // have to clear the trusted-host policy before tenancy ever runs.
    config.security.trusted_hosts.hosts = vec![
        "tenant-a.example.test".to_owned(),
        "tenant-b.example.test".to_owned(),
    ];
    config
}

/// Header-sourced tenancy: tenant B must never be served tenant A's stored
/// response, however the two requests happen to agree on `Idempotency-Key`.
#[tokio::test]
async fn header_tenancy_does_not_replay_across_tenants() {
    let app = TestApp::new()
        .config(tenancy_config("header"))
        .idempotent()
        .routes(routes![create_order])
        .build();

    let first = app
        .post("/orders")
        .header("x-tenant-id", "tenant-a")
        .header("idempotency-key", "shared-key")
        .body("{}")
        .send()
        .await;
    first.assert_ok();
    assert_eq!(
        first.text(),
        "order-for-tenant-a-sentinel",
        "tenant A's own mutation runs normally"
    );

    let second = app
        .post("/orders")
        .header("x-tenant-id", "tenant-b")
        .header("idempotency-key", "shared-key")
        .body("{}")
        .send()
        .await;

    assert!(
        !second.text().contains("tenant-a"),
        "tenant B received tenant A's cached response body: {:?} (status {})",
        second.text(),
        second.status
    );
    assert_ne!(
        second.header("x-idempotent-replayed"),
        Some("true"),
        "tenant B's request must not be answered from tenant A's cache slot"
    );
}

/// Subdomain-sourced tenancy: the request target is byte-identical across
/// tenants (only the `Host` differs), so the storage key must carry the
/// resolved tenant or the two tenants share one slot.
#[tokio::test]
async fn subdomain_tenancy_does_not_replay_across_tenants() {
    let app = TestApp::new()
        .config(tenancy_config("subdomain"))
        .idempotent()
        .routes(routes![create_order])
        .build();

    let first = app
        .post("/orders")
        .header("host", "tenant-a.example.test")
        .header("idempotency-key", "shared-key")
        .body("{}")
        .send()
        .await;
    first.assert_ok();
    assert_eq!(first.text(), "order-for-tenant-a-sentinel");

    let second = app
        .post("/orders")
        .header("host", "tenant-b.example.test")
        .header("idempotency-key", "shared-key")
        .body("{}")
        .send()
        .await;

    assert!(
        !second.text().contains("tenant-a"),
        "tenant B received tenant A's cached response body: {:?} (status {})",
        second.text(),
        second.status
    );
}

/// The fix must not break the feature it protects: the *same* tenant retrying
/// the *same* key still gets the stored response back rather than re-executing
/// the mutation.
#[tokio::test]
async fn same_tenant_retry_still_replays() {
    let app = TestApp::new()
        .config(tenancy_config("header"))
        .idempotent()
        .routes(routes![create_order])
        .build();

    let first = app
        .post("/orders")
        .header("x-tenant-id", "tenant-a")
        .header("idempotency-key", "retry-key")
        .body("{}")
        .send()
        .await;
    first.assert_ok();

    let retry = app
        .post("/orders")
        .header("x-tenant-id", "tenant-a")
        .header("idempotency-key", "retry-key")
        .body("{}")
        .send()
        .await;
    retry.assert_ok();
    assert_eq!(retry.text(), "order-for-tenant-a-sentinel");
    assert_eq!(
        retry.header("x-idempotent-replayed"),
        Some("true"),
        "a same-tenant retry must still be served from the idempotency cache"
    );
}

static SWITCH_HANDLER_CALLS: AtomicUsize = AtomicUsize::new(0);

/// Exempt from tenancy (`public_paths`) so a session with no tenant yet can
/// reach it: establishes `tenant_id = "org-a"` for the rest of the test.
#[post("/login")]
#[public]
async fn establish_session_tenant(session: Session) -> &'static str {
    session.insert("tenant_id", "org-a").await;
    "ok"
}

/// Mimics an organization-switch handler (`examples/teams`'s
/// `switch_organization`): mutates the *same* session's tenancy key without
/// rotating the session id, so the request that resolved "org-a" leaves a
/// session now holding "org-b".
#[post("/switch-org")]
#[public]
async fn switch_org(session: Session) -> String {
    let calls = SWITCH_HANDLER_CALLS.fetch_add(1, Ordering::SeqCst) + 1;
    session.insert("tenant_id", "org-b").await;
    format!("switched-{calls}")
}

/// Session-sourced tenancy: the tenant captured before the handler ran is
/// stale the instant the handler itself changes the session's tenancy key.
/// A retry presents the *same* session (no id rotation), which now resolves
/// "org-b" — its deferred idempotency alias must be keyed by that finalized
/// tenant, not the "org-a" the request started as, or the retry misses the
/// cache and re-runs the switch a second time.
#[tokio::test]
async fn session_tenancy_alias_uses_finalized_tenant_after_switch() {
    let mut config = tenancy_config("session");
    config.tenancy.public_paths = vec!["/login".to_owned()];
    let app = TestApp::new()
        .config(config)
        .idempotent()
        .routes(routes![establish_session_tenant, switch_org])
        .build();

    let login = app.post("/login").body("{}").send().await;
    login.assert_ok();

    let first = app
        .post("/switch-org")
        .header("idempotency-key", "switch-key")
        .body("{}")
        .send()
        .await;
    first.assert_ok();
    assert_eq!(first.text(), "switched-1");

    let retry = app
        .post("/switch-org")
        .header("idempotency-key", "switch-key")
        .body("{}")
        .send()
        .await;
    retry.assert_ok();
    assert_eq!(
        retry.text(),
        "switched-1",
        "a retry after an org switch must replay the cached response instead of re-running \
         the handler under the tenant it switched *to*"
    );
    assert_eq!(
        retry.header("x-idempotent-replayed"),
        Some("true"),
        "the retry must be served from the idempotency cache"
    );
}
