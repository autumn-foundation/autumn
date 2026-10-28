//! Criterion benchmarks for admission control (issue #3068).
//!
//! The acceptance criterion: adaptive admission costs at most the static
//! limiter's cost plus a small ε. Each benchmark sends one request through a
//! `LoadShedService` that wraps a service that answers at once. The inner
//! future is ready on the first poll, so the numbers show the layer's own
//! cost: the limit read, the criticality lookup, the CAS, and (adaptive) the
//! RTT sample on completion.
//!
//! Run:
//!
//! ```sh
//! cargo bench -p autumn-web --bench admission
//! ```
//!
//! `autumn/tests/admission_alloc_gate.rs` checks the allocation half of the
//! criterion in CI.

use std::convert::Infallible;
use std::hint::black_box;
use std::sync::Arc;
use std::time::Duration;

use autumn_web::Criticality;
use autumn_web::admission::{
    AdaptiveLimiter, Aimd, Gradient2, LimitAlgorithm, LimitBounds, PartitionShares, Vegas,
};
use autumn_web::middleware::{LoadShedLayer, MetricsCollector};
use axum::body::Body;
use axum::http::{Request, Response};
use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use futures::FutureExt as _;
use tower::{Layer, Service, service_fn};

type Inner = tower::util::ServiceFn<
    fn(Request<Body>) -> std::future::Ready<Result<Response<Body>, Infallible>>,
>;

fn inner() -> Inner {
    service_fn(|_req: Request<Body>| std::future::ready(Ok(Response::new(Body::empty()))))
}

fn bounds() -> LimitBounds {
    LimitBounds::new(1, 10_000, 1_000).expect("valid bounds")
}

fn layers() -> Vec<(&'static str, LoadShedLayer)> {
    let metrics = MetricsCollector::new();
    let adaptive =
        |algorithm| LoadShedLayer::adaptive(AdaptiveLimiter::new(algorithm), metrics.clone());
    vec![
        ("static", LoadShedLayer::new(1_000, metrics.clone())),
        (
            "static_flat_shares",
            LoadShedLayer::new(1_000, metrics.clone())
                .with_partitions(PartitionShares::new(1.0, 1.0).expect("valid shares")),
        ),
        (
            "adaptive_gradient2",
            adaptive(LimitAlgorithm::Gradient2(Gradient2::new(bounds()))),
        ),
        (
            "adaptive_vegas",
            adaptive(LimitAlgorithm::Vegas(Vegas::new(bounds()))),
        ),
        (
            "adaptive_aimd",
            adaptive(LimitAlgorithm::Aimd(Aimd::new(
                bounds(),
                Duration::from_secs(1),
            ))),
        ),
    ]
}

fn request(criticality: Option<Criticality>) -> Request<Body> {
    let mut req = Request::builder()
        .uri("/work")
        .body(Body::empty())
        .expect("valid request");
    if let Some(c) = criticality {
        req.extensions_mut().insert(c);
    }
    req
}

fn bench_admission(c: &mut Criterion) {
    let mut group = c.benchmark_group("admission");
    let mut baseline = inner();
    group.bench_function("no_layer", |b| {
        b.iter_batched(
            || request(None),
            |req| black_box(baseline.call(req).now_or_never()),
            BatchSize::SmallInput,
        );
    });
    for (name, layer) in layers() {
        let mut svc = layer.layer(inner());
        group.bench_function(name, |b| {
            b.iter_batched(
                || request(None),
                |req| black_box(svc.call(req).now_or_never()),
                BatchSize::SmallInput,
            );
        });
        let mut svc = layer.layer(inner());
        group.bench_function(format!("{name}/sheddable"), |b| {
            b.iter_batched(
                || request(Some(Criticality::Sheddable)),
                |req| black_box(svc.call(req).now_or_never()),
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

fn bench_algorithms(c: &mut Criterion) {
    let mut group = c.benchmark_group("limit_update");
    let sample = autumn_web::admission::Sample {
        rtt: Duration::from_millis(10),
        in_flight: 600,
        dropped: false,
        at: Duration::ZERO,
    };
    let algorithms = [
        (
            "gradient2",
            LimitAlgorithm::Gradient2(Gradient2::new(bounds())),
        ),
        ("vegas", LimitAlgorithm::Vegas(Vegas::new(bounds()))),
        (
            "aimd",
            LimitAlgorithm::Aimd(Aimd::new(bounds(), Duration::from_secs(1))),
        ),
    ];
    for (name, algorithm) in algorithms {
        let limiter: Arc<AdaptiveLimiter> = AdaptiveLimiter::new(algorithm);
        let mut at = Duration::ZERO;
        group.bench_function(name, |b| {
            b.iter(|| {
                at += Duration::from_millis(1);
                black_box(limiter.record(autumn_web::admission::Sample { at, ..sample }))
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_admission, bench_algorithms);
criterion_main!(benches);
