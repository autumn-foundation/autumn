//! A client must not select its tenant. The tenant comes from a credential,
//! from a trusted proxy, or from an explicit unsafe opt-in. A session that
//! names a tenant binds the request to that tenant.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use autumn_web::config::{AutumnConfig, TenantHeaderTrust};
use autumn_web::session::Session;
use autumn_web::test::TestApp;
use autumn_web::{AppState, get, post, public, routes, secured};
use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use tower::ServiceExt as _;

/// A router whose handler returns `CURRENT_TENANT`, the value that generated
/// repositories filter by. `reached` counts the handler calls.
fn echo_app(config: AutumnConfig) -> (Router, Arc<AtomicUsize>) {
    let state = AppState::for_test();
    state.insert_extension(config);
    let reached = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&reached);
    let app = Router::new()
        .route(
            "/data",
            axum::routing::get(move || {
                let counter = Arc::clone(&counter);
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    autumn_web::tenancy::CURRENT_TENANT
                        .try_with(Clone::clone)
                        .ok()
                        .flatten()
                        .unwrap_or_default()
                }
            }),
        )
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            autumn_web::tenancy::tenancy_middleware,
        ))
        .with_state(state);
    (app, reached)
}

fn session_for(tenant: &str) -> Session {
    Session::new_for_test(
        "sid".to_owned(),
        HashMap::from([
            ("user_id".to_owned(), "42".to_owned()),
            ("role".to_owned(), "admin".to_owned()),
            ("tenant_id".to_owned(), tenant.to_owned()),
        ]),
    )
}

fn request(header_tenant: Option<&str>, session: Option<Session>) -> Request<Body> {
    let mut builder = Request::builder().uri("/data");
    if let Some(tenant) = header_tenant {
        builder = builder.header("x-tenant-id", tenant);
    }
    if let Some(session) = session {
        builder = builder.extension(session);
    }
    builder.body(Body::empty()).expect("request")
}

async fn body_text(response: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("body");
    String::from_utf8(bytes.to_vec()).expect("utf-8")
}

fn enabled_only() -> AutumnConfig {
    let mut config = AutumnConfig::default();
    config.tenancy.enabled = true;
    config
}

fn header_source() -> AutumnConfig {
    let mut config = enabled_only();
    "header".clone_into(&mut config.tenancy.source);
    config
}

#[tokio::test]
async fn default_source_reads_the_session_not_the_header() {
    let (app, _) = echo_app(enabled_only());
    let response = app
        .oneshot(request(Some("tenant-b"), Some(session_for("tenant-a"))))
        .await
        .expect("infallible");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_text(response).await, "tenant-a");
}

#[tokio::test]
async fn default_source_does_not_accept_a_header_alone() {
    let (app, reached) = echo_app(enabled_only());
    let no_tenant = Session::new_for_test(
        "sid".to_owned(),
        HashMap::from([("user_id".to_owned(), "42".to_owned())]),
    );
    let response = app
        .oneshot(request(Some("tenant-b"), Some(no_tenant)))
        .await
        .expect("infallible");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(reached.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn header_source_without_a_trust_mode_fails_closed() {
    let (app, reached) = echo_app(header_source());
    let response = app
        .oneshot(request(Some("tenant-b"), None))
        .await
        .expect("infallible");
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert!(body_text(response).await.contains("header_trust"));
    assert_eq!(reached.load(Ordering::SeqCst), 0);
}

fn header_source_with(trust: TenantHeaderTrust) -> AutumnConfig {
    let mut config = header_source();
    config.tenancy.header_trust = Some(trust);
    config
}

/// A gateway at `10.0.0.0/8` sets the tenant header.
fn behind_gateway() -> AutumnConfig {
    let mut config = header_source_with(TenantHeaderTrust::TrustedProxy);
    config.security.trusted_proxies.trust_forwarded_headers = true;
    config.security.trusted_proxies.ranges = vec!["10.0.0.0/8".to_owned()];
    config
}

fn from_peer(peer: &str, tenants: &[&str]) -> Request<Body> {
    let addr: SocketAddr = peer.parse().expect("socket address");
    let mut builder = Request::builder().uri("/data").extension(ConnectInfo(addr));
    for tenant in tenants {
        builder = builder.header("x-tenant-id", *tenant);
    }
    builder.body(Body::empty()).expect("request")
}

#[test]
fn header_source_without_a_trust_mode_fails_boot_validation() {
    let err = header_source()
        .validate_tenancy_source()
        .expect_err("header source with no header_trust must not boot");
    assert!(err.to_string().contains("header_trust"), "{err}");
}

#[test]
fn trusted_proxy_without_forwarding_trust_fails_boot_validation() {
    let err = header_source_with(TenantHeaderTrust::TrustedProxy)
        .validate_tenancy_source()
        .expect_err("trusted_proxy with no trusted proxies must not boot");
    assert!(err.to_string().contains("trusted_proxies"), "{err}");
}

#[test]
fn explicit_trust_modes_pass_boot_validation() {
    assert!(behind_gateway().validate_tenancy_source().is_ok());
    assert!(
        header_source_with(TenantHeaderTrust::AnyClient)
            .validate_tenancy_source()
            .is_ok()
    );
    assert!(enabled_only().validate_tenancy_source().is_ok());
    let mut disabled = header_source();
    disabled.tenancy.enabled = false;
    assert!(disabled.validate_tenancy_source().is_ok());
}

/// With no range, `trusted_proxy` must not trust every peer. A hop count
/// does not count: a client that connects directly can forge the chain.
#[test]
fn trusted_proxy_needs_a_proxy_range_at_boot() {
    let mut no_ranges = behind_gateway();
    no_ranges.security.trusted_proxies.ranges.clear();
    assert!(no_ranges.validate_tenancy_source().is_err());

    let mut hops_only = no_ranges.clone();
    hops_only.security.trusted_proxies.trusted_hops = Some(1);
    assert!(hops_only.validate_tenancy_source().is_err());

    let mut bad_range = no_ranges;
    bad_range.security.trusted_proxies.ranges = vec!["not-a-range".to_owned()];
    assert!(bad_range.validate_tenancy_source().is_err());
}

/// A router built without `run` skips the boot check. Then a request from
/// any peer still fails.
#[tokio::test]
async fn trusted_proxy_with_no_range_trusts_no_peer() {
    let mut config = behind_gateway();
    config.security.trusted_proxies.ranges.clear();
    config.security.trusted_proxies.trusted_hops = Some(1);
    let (app, reached) = echo_app(config);
    let mut forged = from_peer("203.0.113.9:4000", &["tenant-b"]);
    forged.headers_mut().insert(
        "x-forwarded-for",
        "198.51.100.1, 198.51.100.2".parse().expect("header"),
    );
    let response = app.oneshot(forged).await.expect("infallible");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(reached.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn any_client_rejects_two_header_values() {
    let (app, reached) = echo_app(header_source_with(TenantHeaderTrust::AnyClient));
    let request = Request::builder()
        .uri("/data")
        .header("x-tenant-id", "tenant-b")
        .header("x-tenant-id", "tenant-a")
        .body(Body::empty())
        .expect("request");
    let response = app.oneshot(request).await.expect("infallible");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(reached.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn an_empty_session_tenant_does_not_bind() {
    let (app, _) = echo_app(header_source_with(TenantHeaderTrust::AnyClient));
    let response = app
        .oneshot(request(Some("tenant-b"), Some(session_for("  "))))
        .await
        .expect("infallible");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_text(response).await, "tenant-b");
}

#[tokio::test]
async fn the_session_check_compares_case() {
    let (app, reached) = echo_app(header_source_with(TenantHeaderTrust::AnyClient));
    let response = app
        .oneshot(request(Some("tenant-a"), Some(session_for("Tenant-A"))))
        .await
        .expect("infallible");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(reached.load(Ordering::SeqCst), 0);
}

/// The host name is also client-selected. The session binds it too.
#[tokio::test]
async fn a_subdomain_for_another_tenant_is_refused() {
    let mut config = enabled_only();
    "subdomain".clone_into(&mut config.tenancy.source);
    let (app, reached) = echo_app(config);
    let host = |tenant: &str| {
        Request::builder()
            .uri("/data")
            .header("host", format!("{tenant}.example.com"))
            .extension(session_for("tenant-a"))
            .body(Body::empty())
            .expect("request")
    };
    let response = app
        .clone()
        .oneshot(host("tenant-b"))
        .await
        .expect("infallible");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(reached.load(Ordering::SeqCst), 0);

    let response = app.oneshot(host("tenant-a")).await.expect("infallible");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_text(response).await, "tenant-a");
}

/// A verified token for another tenant does not override the session tenant.
#[cfg(feature = "jwt")]
#[tokio::test]
async fn a_token_for_another_tenant_is_refused() {
    use jsonwebtoken::{EncodingKey, Header, encode};

    let mut config = enabled_only();
    "jwt".clone_into(&mut config.tenancy.source);
    config.tenancy.jwt_secret = Some("secret".to_owned().into());
    let token = encode(
        &Header::default(),
        &serde_json::json!({ "tenant_id": "tenant-b", "exp": 10_000_000_000_u64 }),
        &EncodingKey::from_secret(b"secret"),
    )
    .expect("token");
    let (mut parts, ()) = Request::builder()
        .header("authorization", format!("Bearer {token}"))
        .extension(session_for("tenant-a"))
        .body(())
        .expect("request")
        .into_parts();
    let err = autumn_web::tenancy::extract_tenant_from_parts(&mut parts, &config)
        .await
        .expect_err("a token must not override the session tenant");
    assert_eq!(err.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn tenant_a_admin_cannot_select_tenant_b_by_header() {
    let (app, reached) = echo_app(header_source_with(TenantHeaderTrust::AnyClient));
    let response = app
        .oneshot(request(Some("tenant-b"), Some(session_for("tenant-a"))))
        .await
        .expect("infallible");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(reached.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_header_that_matches_the_session_tenant_passes() {
    let (app, _) = echo_app(header_source_with(TenantHeaderTrust::AnyClient));
    let response = app
        .oneshot(request(Some("tenant-a"), Some(session_for("tenant-a"))))
        .await
        .expect("infallible");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_text(response).await, "tenant-a");
}

#[tokio::test]
async fn trusted_proxy_rejects_a_direct_client() {
    let (app, reached) = echo_app(behind_gateway());
    let response = app
        .oneshot(from_peer("203.0.113.9:4000", &["tenant-b"]))
        .await
        .expect("infallible");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(body_text(response).await.contains("trusted proxy"));
    assert_eq!(reached.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn trusted_proxy_accepts_the_gateway() {
    let (app, _) = echo_app(behind_gateway());
    let response = app
        .oneshot(from_peer("10.0.0.5:4000", &["tenant-b"]))
        .await
        .expect("infallible");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body_text(response).await, "tenant-b");
}

#[tokio::test]
async fn trusted_proxy_rejects_two_header_values() {
    let (app, reached) = echo_app(behind_gateway());
    let response = app
        .oneshot(from_peer("10.0.0.5:4000", &["tenant-b", "tenant-a"]))
        .await
        .expect("infallible");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(reached.load(Ordering::SeqCst), 0);
}

/// `extract_tenant_from_parts` is public. It must apply the session binding too.
#[tokio::test]
async fn direct_extraction_applies_the_session_binding() {
    let config = header_source_with(TenantHeaderTrust::AnyClient);
    let (mut parts, ()) = Request::builder()
        .header("x-tenant-id", "tenant-b")
        .extension(session_for("tenant-a"))
        .body(())
        .expect("request")
        .into_parts();
    let err = autumn_web::tenancy::extract_tenant_from_parts(&mut parts, &config)
        .await
        .expect_err("a header must not override the session tenant");
    assert_eq!(err.status(), StatusCode::FORBIDDEN);
}

/// A route marked `#[secured("admin")]` passes the role check for a tenant-A
/// admin, so the tenant check must stop the request first.
#[tokio::test]
async fn secured_admin_route_does_not_serve_another_tenant() {
    let mut config = header_source_with(TenantHeaderTrust::AnyClient);
    config.tenancy.public_paths = vec!["/login".to_owned()];
    let app = TestApp::new()
        .config(config)
        .routes(routes![login, admin_data])
        .build();

    app.post("/login").body("{}").send().await.assert_ok();

    let own = app
        .get("/admin/data")
        .header("x-tenant-id", "tenant-a")
        .send()
        .await;
    own.assert_ok();
    assert_eq!(own.text(), "tenant-a");

    let other = app
        .get("/admin/data")
        .header("x-tenant-id", "tenant-b")
        .send()
        .await;
    other.assert_status(403);
}

/// On a public path the middleware does not run. The `Tenant` extractor
/// must still apply the session binding.
#[tokio::test]
async fn the_tenant_extractor_on_a_public_path_is_bound() {
    let mut config = header_source_with(TenantHeaderTrust::AnyClient);
    config.tenancy.public_paths = vec!["/login".to_owned(), "/open".to_owned()];
    let app = TestApp::new()
        .config(config)
        .routes(routes![login, open_data])
        .build();

    app.post("/login").body("{}").send().await.assert_ok();

    app.get("/open")
        .header("x-tenant-id", "tenant-b")
        .send()
        .await
        .assert_status(403);
}

#[get("/open")]
#[public]
async fn open_data(tenant: autumn_web::tenancy::Tenant) -> String {
    tenant.0
}

#[post("/login")]
#[public]
async fn login(session: Session) -> &'static str {
    session.insert("user_id", "42").await;
    session.insert("role", "admin").await;
    session.insert("tenant_id", "tenant-a").await;
    "ok"
}

#[get("/admin/data")]
#[secured("admin")]
async fn admin_data(tenant: autumn_web::tenancy::Tenant) -> String {
    tenant.0
}
