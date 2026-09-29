use autumn_web::middleware::{MetricsCollector, MetricsLayer};
use axum::body::Body;
use axum::http::Request;
use loom::thread;
use std::task::{Context, Poll};
use tower::{Layer, Service};

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

#[test]
fn metrics_layer_concurrent_drops() {
    loom::model(|| {
        let collector = MetricsCollector::new();
        let svc = MetricsLayer::new(collector.clone()).layer(PendingService);

        let mut s1 = svc.clone();
        let t1 = thread::spawn(move || {
            let req = Request::builder().uri("/").body(Body::empty()).unwrap();
            let fut = s1.call(req);
            // Drop it immediately
            drop(fut);
        });

        let mut s2 = svc;
        let t2 = thread::spawn(move || {
            let req = Request::builder().uri("/").body(Body::empty()).unwrap();
            let fut = s2.call(req);
            drop(fut);
        });

        t1.join().unwrap();
        t2.join().unwrap();

        assert_eq!(collector.snapshot().http.requests_active, 0);
    });
}
