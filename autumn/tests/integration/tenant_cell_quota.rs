//! Integration test proving AC #3 of issue #1766: an over-quota tenant's
//! request fails with HTTP 503 while another tenant proceeds unaffected.
//!
//! This drives the REAL `tenancy_middleware` end-to-end via `oneshot`, so it
//! exercises the actual registry wiring, cell binding, and task-local
//! propagation rather than a hand-scoped stand-in.

use autumn_web::AppState;
use axum::Router;
use axum::http::StatusCode;
use axum::routing::post;
use tower::ServiceExt;

/// Allocates 600 bytes of scratch per request in the current tenant's cell. The
/// cell is shared across requests via the process-wide registry, so repeated
/// calls for the same tenant accumulate until the quota (1000) is exceeded, at
/// which point `scratch_insert` returns `QuotaExceeded` -> HTTP 503 for THAT
/// tenant only.
async fn charge_handler() -> Result<StatusCode, autumn_web::AutumnError> {
    static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let cell = autumn_web::tenant_cell::current_tenant_cell().ok_or_else(|| {
        autumn_web::AutumnError::internal_server_error_msg("no tenant cell bound")
    })?;
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    cell.scratch_insert(format!("req-{n}"), vec![0u8; 600])?;
    Ok(StatusCode::OK)
}

fn build_app() -> Router {
    let state = AppState::for_test();

    let mut config = autumn_web::config::AutumnConfig::default();
    config.tenancy.enabled = true;
    config.tenancy.source = "header".to_string();
    config.tenancy.header_name = "x-tenant-id".to_string();
    config.tenancy.quota_bytes = 1000;
    state.insert_extension(config);

    Router::new()
        .route("/charge", post(charge_handler))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            autumn_web::tenancy::tenancy_middleware,
        ))
        .with_state(state)
}

fn charge_request(tenant: &str) -> axum::http::Request<axum::body::Body> {
    axum::http::Request::builder()
        .method("POST")
        .uri("/charge")
        .header("x-tenant-id", tenant)
        .body(axum::body::Body::empty())
        .expect("valid request")
}

/// tenant-a's second request pushes accumulated scratch to 1200 > 1000 and must
/// be rejected with 503, while tenant-b (independent, only 600 tracked) proceeds
/// with 200.
#[tokio::test]
async fn over_quota_tenant_gets_503_others_proceed() {
    let app = build_app();

    // tenant-a: first request charges 600 -> OK.
    let res = app
        .clone()
        .oneshot(charge_request("tenant-a"))
        .await
        .expect("first request completes");
    assert_eq!(res.status(), StatusCode::OK, "first tenant-a charge fits");

    // tenant-a: second request would bring it to 1200 > 1000 -> 503.
    let res = app
        .clone()
        .oneshot(charge_request("tenant-a"))
        .await
        .expect("second request completes");
    assert_eq!(
        res.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "second tenant-a charge exceeds the quota and must 503"
    );

    // tenant-b: independent cell, only 600 tracked -> OK.
    let res = app
        .clone()
        .oneshot(charge_request("tenant-b"))
        .await
        .expect("tenant-b request completes");
    assert_eq!(
        res.status(),
        StatusCode::OK,
        "tenant-b is independent and must not be affected by tenant-a"
    );
}
