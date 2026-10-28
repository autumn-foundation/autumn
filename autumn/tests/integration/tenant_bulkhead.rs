//! Issue #3072: a per-tenant request bulkhead. One tenant at its cap gets
//! `503`; another tenant is not affected.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use autumn_web::AppState;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header::RETRY_AFTER};
use axum::routing::get;
use tokio::sync::Notify;
use tower::ServiceExt;

/// Requests that wait in `/hold`, and the signal that releases them.
#[derive(Clone, Default)]
struct Gate {
    entered: Arc<AtomicUsize>,
    release: Arc<Notify>,
}

fn app(max_concurrent_requests: usize) -> (Router, Gate) {
    app_with_cors(max_concurrent_requests, Vec::new())
}

fn app_with_cors(max_concurrent_requests: usize, origins: Vec<String>) -> (Router, Gate) {
    let state = AppState::for_test();
    let mut config = autumn_web::config::AutumnConfig::default();
    config.cors.allowed_origins = origins;
    config.tenancy.enabled = true;
    config.tenancy.source = "header".to_string();
    config.tenancy.header_name = "x-tenant-id".to_string();
    config.tenancy.max_concurrent_requests = max_concurrent_requests;
    state.insert_extension(config);

    let gate = Gate::default();
    let hold_gate = gate.clone();
    let app = Router::new()
        .route(
            "/hold",
            get(move || {
                let gate = hold_gate.clone();
                async move {
                    let released = gate.release.notified();
                    gate.entered.fetch_add(1, Ordering::SeqCst);
                    released.await;
                    StatusCode::OK
                }
            }),
        )
        .route("/fast", get(|| async { StatusCode::OK }))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            autumn_web::tenancy::tenancy_middleware,
        ))
        .with_state(state);
    (app, gate)
}

fn request(path: &str, tenant: &str) -> Request<Body> {
    Request::builder()
        .uri(path)
        .header("x-tenant-id", tenant)
        .body(Body::empty())
        .expect("request")
}

/// Start `n` held requests for `tenant` and wait until all are in the handler.
async fn hold(
    app: &Router,
    gate: &Gate,
    tenant: &str,
    n: usize,
) -> Vec<tokio::task::JoinHandle<StatusCode>> {
    let before = gate.entered.load(Ordering::SeqCst);
    let handles: Vec<_> = (0..n)
        .map(|_| {
            let app = app.clone();
            let req = request("/hold", tenant);
            tokio::spawn(async move { app.oneshot(req).await.expect("infallible").status() })
        })
        .collect();
    while gate.entered.load(Ordering::SeqCst) < before + n {
        tokio::task::yield_now().await;
    }
    handles
}

#[tokio::test]
async fn a_tenant_at_its_cap_gets_503_and_another_tenant_does_not() {
    let (app, gate) = app(2);
    let held = hold(&app, &gate, "noisy", 2).await;

    let over = app
        .clone()
        .oneshot(request("/fast", "noisy"))
        .await
        .expect("infallible");
    assert_eq!(over.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        over.headers()
            .get(RETRY_AFTER)
            .and_then(|v| v.to_str().ok()),
        Some("1"),
        "a bulkhead 503 tells the client when to retry"
    );

    let quiet = app
        .clone()
        .oneshot(request("/fast", "quiet"))
        .await
        .expect("infallible");
    assert_eq!(
        quiet.status(),
        StatusCode::OK,
        "another tenant is not capped"
    );

    gate.release.notify_waiters();
    for handle in held {
        assert_eq!(handle.await.expect("join"), StatusCode::OK);
    }
    let after = app
        .clone()
        .oneshot(request("/fast", "noisy"))
        .await
        .expect("infallible");
    assert_eq!(after.status(), StatusCode::OK, "released permits come back");
}

#[tokio::test]
async fn a_zero_cap_does_not_limit_a_tenant() {
    let (app, gate) = app(0);
    let held = hold(&app, &gate, "noisy", 5).await;
    let more = app
        .clone()
        .oneshot(request("/fast", "noisy"))
        .await
        .expect("infallible");
    assert_eq!(more.status(), StatusCode::OK);
    gate.release.notify_waiters();
    for handle in held {
        assert_eq!(handle.await.expect("join"), StatusCode::OK);
    }
}

/// The tenancy layer runs outside `CorsLayer`, so the bulkhead `503` carries
/// the CORS headers itself. A browser then reads the `503`, not a CORS error.
#[tokio::test]
async fn a_bulkhead_503_carries_the_cors_headers() {
    let (app, gate) = app_with_cors(1, vec!["https://app.example".to_owned()]);
    let held = hold(&app, &gate, "noisy", 1).await;
    let mut req = request("/fast", "noisy");
    req.headers_mut().insert(
        axum::http::header::ORIGIN,
        axum::http::HeaderValue::from_static("https://app.example"),
    );
    let over = app.clone().oneshot(req).await.expect("infallible");
    assert_eq!(over.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        over.headers()
            .get(axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .and_then(|v| v.to_str().ok()),
        Some("https://app.example")
    );
    gate.release.notify_waiters();
    for handle in held {
        assert_eq!(handle.await.expect("join"), StatusCode::OK);
    }
}
