use autumn_web::security::RateLimitConfig;
use autumn_web::security::RateLimitLayer;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::Request;
use proptest::prelude::*;
use std::net::SocketAddr;
use std::task::{Context, Poll};
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

proptest! {
    #[test]
    fn rate_limit_fuzzing(ip in ".*") {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(async {
                let config = RateLimitConfig {
                    enabled: true,
                    requests_per_second: 10.0,
                    burst: 2,
                    trust_forwarded_headers: true,
                    trusted_proxies: Vec::new(),
                    ..Default::default()
                };
                let layer = RateLimitLayer::from_config(&config);
                let mut svc = layer.layer(MockService);

                // Allow request builder to fail if header value is totally invalid (e.g. \0)
                // but if it builds, the rate limiter should not panic.
                let mut req_builder = Request::builder();
                if let Ok(hv) = axum::http::HeaderValue::from_str(&ip) {
                    req_builder = req_builder.header("X-Forwarded-For", hv);
                }

                if let Ok(mut req) = req_builder.body(Body::empty()) {
                    let peer: SocketAddr = "198.51.100.1:2000".parse().unwrap();
                    req.extensions_mut().insert(ConnectInfo(peer));
                    let _ = svc.call(req).await;
                }
            });
    }
}
