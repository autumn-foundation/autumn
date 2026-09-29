use autumn_web::maintenance::{MaintenanceConfig, MaintenanceState};
use autumn_web::middleware::maintenance::MaintenanceLayer;
use autumn_web::security::{TrustedProxiesConfig, TrustedProxiesLayer};
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use axum::{Router, body::Body, routing::get};
use std::net::SocketAddr;
use tower::ServiceExt;

#[tokio::test]
async fn maintenance_trusted_proxy_bypass() {
    let state = MaintenanceState::new();
    state.enable(MaintenanceConfig {
        message: Some("Maintenance Mode Active".to_string()),
        allow_ips: vec!["192.168.1.10".to_string()], // only this IP is allowed
        ..Default::default()
    });

    let app = Router::new()
        .route("/", get(|| async { "Hello" }))
        .layer(MaintenanceLayer::new(state))
        .layer(TrustedProxiesLayer::from_config(&TrustedProxiesConfig {
            ranges: vec!["203.0.113.10".to_owned()],
            trusted_hops: None,
            trust_forwarded_headers: true,
        }));

    let peer: SocketAddr = "203.0.113.10:4000".parse().unwrap();

    // 1. Request from allowed IP via trusted proxy
    let mut req1 = Request::builder()
        .uri("/")
        .header("X-Forwarded-For", "192.168.1.10")
        .body(Body::empty())
        .unwrap();
    req1.extensions_mut().insert(ConnectInfo(peer));

    let resp1 = app.clone().oneshot(req1).await.unwrap();
    assert_eq!(resp1.status(), StatusCode::OK);

    // 2. Request from disallowed IP via trusted proxy
    let mut req2 = Request::builder()
        .uri("/")
        .header("X-Forwarded-For", "192.168.1.9")
        .body(Body::empty())
        .unwrap();
    req2.extensions_mut().insert(ConnectInfo(peer));

    let resp2 = app.clone().oneshot(req2).await.unwrap();
    assert_eq!(resp2.status(), StatusCode::SERVICE_UNAVAILABLE);

    // 3. Request from allowed IP via untrusted peer (should not be trusted)
    let peer_untrusted: SocketAddr = "203.0.113.11:4000".parse().unwrap();
    let mut req3 = Request::builder()
        .uri("/")
        .header("X-Forwarded-For", "192.168.1.10")
        .body(Body::empty())
        .unwrap();
    req3.extensions_mut().insert(ConnectInfo(peer_untrusted));

    let resp3 = app.clone().oneshot(req3).await.unwrap();
    assert_eq!(resp3.status(), StatusCode::SERVICE_UNAVAILABLE);
}
#[tokio::test]
async fn maintenance_bypass_paths() {
    let state = MaintenanceState::new();
    state.enable(MaintenanceConfig::default());

    let app = Router::new()
        .route("/", get(|| async { "Hello" }))
        .route("/health", get(|| async { "Hello" }))
        .route("/health/live", get(|| async { "Hello" }))
        .route("/health-admin", get(|| async { "Hello" }))
        .layer(MaintenanceLayer::new(state).with_bypass_paths(vec!["/health".to_string()]));

    // 1. Bypass exact path
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // 2. Bypass subtree path
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/health/live")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // 3. Do not bypass non-exact prefix path
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/health-admin")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);

    // 4. Do not bypass other paths
    let resp = app
        .clone()
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn maintenance_synchronous_load_on_startup() {
    let path = std::path::Path::new(autumn_web::maintenance::MAINTENANCE_FLAG_FILE);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }

    let config = MaintenanceConfig {
        message: Some("Startup Maintenance Mode Active".to_string()),
        ..Default::default()
    };
    MaintenanceState::save_to_file(path, &config).unwrap();

    // Verify that loading from file synchronously works and populates a new MaintenanceState
    let state = MaintenanceState::new();
    let loaded = MaintenanceState::load_from_file(path).unwrap();
    assert!(loaded.is_some());
    state.enable(loaded.unwrap());

    assert!(state.is_active());
    assert_eq!(
        state.get().unwrap().message.unwrap(),
        "Startup Maintenance Mode Active"
    );

    // Clean up
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn maintenance_root_bypass_is_exact_only() {
    let state = MaintenanceState::new();
    state.enable(MaintenanceConfig::default());

    let app = Router::new()
        .route("/", get(|| async { "Root" }))
        .route("/admin", get(|| async { "Admin" }))
        .layer(MaintenanceLayer::new(state).with_bypass_paths(vec!["/".to_string()]));

    // 1. Root bypasses (exact match)
    let resp = app
        .clone()
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // 2. /admin does NOT bypass (not treating / as a global subtree bypass)
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn maintenance_probe_paths_are_exact_only() {
    let state = MaintenanceState::new();
    state.enable(MaintenanceConfig::default());

    let app = Router::new()
        .route("/health", get(|| async { "Health" }))
        .route("/health/live", get(|| async { "Live" }))
        .layer(MaintenanceLayer::new(state).with_probe_paths(vec!["/health".to_string()]));

    // 1. Exact match /health bypasses
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // 2. /health/live does NOT bypass (subpath/prefix is blocked)
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/health/live")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
}
