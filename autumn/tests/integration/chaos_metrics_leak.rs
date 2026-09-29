//!
//! Chaos tests for metrics leak.
//!
use autumn_web::middleware::{MetricsCollector, MetricsLayer};
use axum::body::Body;
use axum::http::Request;
use std::task::{Context, Poll};
use tower::{Layer, Service, ServiceExt};

#[derive(Clone)]
struct PendingService;

impl Service<Request<Body>> for PendingService {
    type Response = axum::http::Response<Body>;
    type Error = std::io::Error;
    type Future = std::future::Pending<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _req: Request<Body>) -> Self::Future {
        std::future::pending()
    }
}

#[tokio::test]
async fn metrics_layer_leaks_active_on_drop() {
    let collector = MetricsCollector::new();
    let svc = MetricsLayer::new(collector.clone()).layer(PendingService);

    assert_eq!(collector.snapshot().http.requests_active, 0);

    let req = Request::builder().uri("/").body(Body::empty()).unwrap();
    let mut svc_clone = svc.clone();
    let fut = svc_clone.ready().await.unwrap().call(req);

    // Now active count is 1
    assert_eq!(collector.snapshot().http.requests_active, 1);

    // Drop the future (simulate client disconnect or timeout)
    drop(fut);

    // If it's correct, active count should go back to 0.
    // If it's 1, we found a bug!
    assert_eq!(
        collector.snapshot().http.requests_active,
        0,
        "Memory leak found! requests_active did not decrement on drop."
    );
}
