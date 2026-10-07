//! Isolated integration test: adaptive admission adds no allocations to an
//! admitted request (issue #3068).
//!
//! This is the CI half of the criterion "admission overhead ≤ the static
//! limiter's + ε". Timing in CI is noisy; allocation counts are exact. The
//! Criterion bench `benches/admission.rs` measures the time.
//!
//! Its own binary: `allocation-counter` installs a process-wide counting
//! `#[global_allocator]` (see `config_alloc_gate`).

use std::convert::Infallible;
use std::time::Duration;

use autumn_web::Criticality;
use autumn_web::admission::{AdaptiveLimiter, Gradient2, LimitAlgorithm, LimitBounds, Vegas};
use autumn_web::middleware::{LoadShedLayer, MetricsCollector};
use axum::body::Body;
use axum::http::{Request, Response};
use futures::FutureExt as _;
use tower::{Layer, Service, service_fn};

const CALLS: u64 = 1_000;

fn bounds() -> LimitBounds {
    LimitBounds::new(1, 10_000, 1_000).expect("valid bounds")
}

/// Allocations over `CALLS` calls through `layer`, with the requests built
/// outside the measured window.
fn allocations(layer: &LoadShedLayer) -> u64 {
    let mut svc = layer.layer(service_fn(|_req: Request<Body>| {
        std::future::ready(Ok::<_, Infallible>(Response::new(Body::empty())))
    }));
    let mut requests: Vec<Request<Body>> = (0..CALLS)
        .map(|_| {
            let mut req = Request::builder().uri("/work").body(Body::empty()).unwrap();
            req.extensions_mut().insert(Criticality::Default);
            req
        })
        .collect();
    // Warm up: lazy statics, the first lock, and so on.
    for _ in 0..10 {
        let req = Request::builder().uri("/work").body(Body::empty()).unwrap();
        let _ = svc.call(req).now_or_never();
    }
    let info = allocation_counter::measure(|| {
        for req in std::mem::take(&mut requests) {
            let response = svc.call(req).now_or_never();
            std::hint::black_box(&response);
            drop(response);
        }
    });
    info.count_total
}

#[test]
fn adaptive_admission_allocates_no_more_than_static() {
    let metrics = MetricsCollector::new();
    let static_layer = LoadShedLayer::new(1_000, metrics.clone());
    let gradient2 = LoadShedLayer::adaptive(
        AdaptiveLimiter::new(LimitAlgorithm::Gradient2(Gradient2::new(bounds()))),
        metrics.clone(),
    );
    let vegas = LoadShedLayer::adaptive(
        AdaptiveLimiter::new(LimitAlgorithm::Vegas(Vegas::new(bounds()))),
        metrics,
    );
    let static_allocs = allocations(&static_layer);
    for (name, layer) in [("gradient2", &gradient2), ("vegas", &vegas)] {
        let adaptive_allocs = allocations(layer);
        println!("{CALLS} calls: static {static_allocs} allocations, {name} {adaptive_allocs}");
        assert!(
            adaptive_allocs <= static_allocs,
            "adaptive ({name}) admission made {adaptive_allocs} allocations in {CALLS} calls, \
             static {static_allocs}"
        );
    }
    // The algorithm update is pure arithmetic: no allocation either.
    let limiter = AdaptiveLimiter::new(LimitAlgorithm::Gradient2(Gradient2::new(bounds())));
    // Warm up: on some platforms the first lock of a `Mutex` allocates.
    let _ = limiter.record(autumn_web::admission::Sample {
        rtt: Duration::from_millis(10),
        in_flight: 600,
        dropped: false,
        at: Duration::ZERO,
    });
    let info = allocation_counter::measure(|| {
        for i in 0..CALLS {
            let _ = limiter.record(autumn_web::admission::Sample {
                rtt: Duration::from_millis(10),
                in_flight: 600,
                dropped: false,
                at: Duration::from_millis((i + 1) * 100),
            });
        }
    });
    assert_eq!(info.count_total, 0, "a limit update must not allocate");
}
