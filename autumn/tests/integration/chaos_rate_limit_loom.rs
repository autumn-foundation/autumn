use autumn_web::security::RateLimitConfig;
use autumn_web::security::RateLimitLayer;
use axum::body::Body;
use axum::http::Request;
use std::task::{Context, Poll};
use std::thread;
use tower::{Layer, Service};

#[derive(Clone)]
struct MockService;

impl Service<Request<Body>> for MockService {
    type Response = axum::http::Response<Body>;
    type Error = std::convert::Infallible;
    type Future = std::future::Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _req: Request<Body>) -> Self::Future {
        std::future::ready(Ok(axum::http::Response::new(Body::empty())))
    }
}

// RateLimitLayer uses std::sync::Mutex and AtomicU64 internally, which are not
// Loom-instrumented.  This is a regular multithreaded stress test that exercises
// concurrent request-decision paths through the rate limiter.
#[test]
fn rate_limit_concurrent_requests() {
    let config = RateLimitConfig {
        enabled: true,
        requests_per_second: 10.0,
        burst: 2,
        trust_forwarded_headers: true,
        trusted_proxies: Vec::new(),
        ..Default::default()
    };
    let layer = RateLimitLayer::from_config(&config);
    let svc = layer.layer(MockService);

    let mut s1 = svc.clone();
    let t1 = thread::spawn(move || {
        let req = Request::builder()
            .header("X-Forwarded-For", "1.2.3.4")
            .body(Body::empty())
            .unwrap();
        let _ = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(s1.call(req));
    });

    let mut s2 = svc.clone();
    let t2 = thread::spawn(move || {
        let req = Request::builder()
            .header("X-Forwarded-For", "1.2.3.4")
            .body(Body::empty())
            .unwrap();
        let _ = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(s2.call(req));
    });

    let mut s3 = svc;
    let t3 = thread::spawn(move || {
        let req = Request::builder()
            .header("X-Forwarded-For", "1.2.3.4")
            .body(Body::empty())
            .unwrap();
        let _ = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(s3.call(req));
    });

    t1.join().unwrap();
    t2.join().unwrap();
    t3.join().unwrap();
}
