//! `CacheResponseLayer` × framework tenancy.
//!
//! Composes two documented framework features — `[tenancy] enabled = true`
//! and `CacheResponseLayer` over a route group with `AppBuilder::scoped`,
//! whose layer is an ordinary expression (`docs/guide/middleware.md`) — and
//! asserts that a response stored while serving one tenant is never
//! served to a request that resolved to a *different* tenant.
//!
//! The handler below renders the same page for every visitor of its tenant,
//! which is exactly what the layer's contract asks for. Under subdomain
//! tenancy the request target (`/catalog`) is byte-identical across tenants;
//! only `Host` differs, and HTTP/1.1 request URIs carry no authority. Under
//! header tenancy only a header differs. A key built from the URI alone puts
//! both tenants in one slot, and on the hit the handler — with every
//! `Tenant` extractor and `tenant_scoped` repository filter inside it — never
//! runs for the second tenant.
//!
//! Attacker: any visitor of tenant B (no credentials needed for a public
//! tenant page; a tenant-B session for a gated one).
//! Defender: the cache key, which must carry the resolved tenant.
//! Flag: `catalog-of-tenant-a-sentinel` in a response served to tenant B.

use std::sync::atomic::{AtomicUsize, Ordering};

use autumn_web::cache::{CacheResponseLayer, MokaCache};
use autumn_web::config::AutumnConfig;
use autumn_web::test::TestApp;
use autumn_web::{get, public, routes};

static CATALOG_RENDERS: AtomicUsize = AtomicUsize::new(0);
static UNTENANTED_RENDERS: AtomicUsize = AtomicUsize::new(0);

fn store() -> CacheResponseLayer {
    CacheResponseLayer::from_cache(MokaCache::builder().max_capacity(100).build())
}

/// The same page for every visitor of a tenant: the per-tenant catalog.
#[get("/catalog")]
#[public]
async fn catalog(tenant: autumn_web::tenancy::Tenant) -> String {
    CATALOG_RENDERS.fetch_add(1, Ordering::SeqCst);
    format!("catalog-of-{}-sentinel", tenant.0)
}

/// A route with no tenant in scope (a tenancy public path): its cache key is
/// unchanged by tenancy, and it still caches.
#[get("/about")]
#[public]
async fn about() -> String {
    UNTENANTED_RENDERS.fetch_add(1, Ordering::SeqCst);
    "about-page".to_owned()
}

fn tenancy_config(source: &str) -> AutumnConfig {
    let mut config = AutumnConfig::default();
    config.tenancy.enabled = true;
    source.clone_into(&mut config.tenancy.source);
    config.tenancy.header_trust = Some(autumn_web::config::TenantHeaderTrust::AnyClient);
    "x-tenant-id".clone_into(&mut config.tenancy.header_name);
    config.tenancy.base_domain = Some("example.test".to_owned());
    config.security.trusted_hosts.hosts = vec![
        "tenant-a.example.test".to_owned(),
        "tenant-b.example.test".to_owned(),
    ];
    config.tenancy.public_paths = vec!["/shop/about".to_owned()];
    config
}

/// One test drives every case in order, because the render counters are
/// process-wide and the cases must not interleave.
#[tokio::test]
async fn response_cache_never_serves_one_tenant_to_another() {
    // ── Subdomain tenancy: only `Host` differs. ──────────────────────────
    let app = TestApp::new()
        .config(tenancy_config("subdomain"))
        .scoped("/shop", store(), routes![catalog, about])
        .build();

    let a = app
        .get("/shop/catalog")
        .header("host", "tenant-a.example.test")
        .send()
        .await;
    a.assert_ok();
    assert_eq!(a.text(), "catalog-of-tenant-a-sentinel");

    let b = app
        .get("/shop/catalog")
        .header("host", "tenant-b.example.test")
        .send()
        .await;
    b.assert_ok();
    assert_eq!(
        b.text(),
        "catalog-of-tenant-b-sentinel",
        "subdomain tenancy: tenant B was served tenant A's cached response"
    );

    // The cache still works within a tenant.
    let before = CATALOG_RENDERS.load(Ordering::SeqCst);
    let a_again = app
        .get("/shop/catalog")
        .header("host", "tenant-a.example.test")
        .send()
        .await;
    assert_eq!(a_again.text(), "catalog-of-tenant-a-sentinel");
    assert_eq!(
        CATALOG_RENDERS.load(Ordering::SeqCst),
        before,
        "a repeat request from the same tenant must be a cache hit"
    );

    // ── Header tenancy: only a header differs. ───────────────────────────
    // A fresh tenant pair, so these keys cannot be warm from the case above.
    let app = TestApp::new()
        .config(tenancy_config("header"))
        .scoped("/shop", store(), routes![catalog, about])
        .build();

    let c = app
        .get("/shop/catalog?page=2")
        .header("x-tenant-id", "tenant-c")
        .send()
        .await;
    assert_eq!(c.text(), "catalog-of-tenant-c-sentinel");

    let d = app
        .get("/shop/catalog?page=2")
        .header("x-tenant-id", "tenant-d")
        .send()
        .await;
    assert_eq!(
        d.text(),
        "catalog-of-tenant-d-sentinel",
        "header tenancy: tenant D was served tenant C's cached response"
    );

    // ── No tenant in scope: the key and the caching are unchanged. ───────
    let first = app.get("/shop/about").send().await;
    first.assert_ok();
    let renders = UNTENANTED_RENDERS.load(Ordering::SeqCst);
    let second = app.get("/shop/about").send().await;
    assert_eq!(second.text(), "about-page");
    assert_eq!(
        UNTENANTED_RENDERS.load(Ordering::SeqCst),
        renders,
        "an untenanted route must still be a cache hit"
    );
}
