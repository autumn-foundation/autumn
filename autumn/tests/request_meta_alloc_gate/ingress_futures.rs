//! Allocation gate for the futures of the public ingress services (issue
//! #2830).
//!
//! It is a module of the `request_meta_alloc_gate` binary, which installs the
//! `allocation-counter` global allocator. It is not a binary of its own: each
//! test binary costs disk on the trybuild runners.
//!
//! # What is measured
//!
//! Each service is put over a trivial inner service and called 1000 times.
//! The gate counts the heap blocks of one call. A boxed `Future` is one block
//! on each request, so these counts show if a box comes back.
//!
//! Each request holds one marker extension. Thus the extension map exists
//! before the call, and its allocation is not counted.
//!
//! | service                         | request         | before | after |
//! |---------------------------------|-----------------|--------|-------|
//! | `LogContextService`             | any             |      3 |     2 |
//! | `SessionService`                | no cookie       |      7 |     3 |
//! | `MethodOverrideService`         | GET, JSON POST  |      1 |     0 |
//! | `IdempotencyReplayService`      | no replay       |      1 |     0 |
//! | `SubmitTokenLayer` service      | GET             |      5 |     4 |
//! | `SubmitTokenLayer` service      | JSON POST       |      6 |     4 |
//!
//! The 2 blocks of `LogContextService` are its two scopes: the log context
//! and the actor scope. The 3 blocks of `SessionService` are the session id,
//! the session state, and the session extension. The 4 blocks of the
//! submit-token service are the new token, a copy of the field name, and the
//! two request extensions. Before, a JSON POST also had a boxed future and
//! a parse of its content type.
//!
//! If this test fails, do not increase a count. Measure again and find the
//! new allocation.

use std::convert::Infallible;
use std::sync::Arc;

use autumn_web::log::filter::ParameterFilter;
use autumn_web::middleware::{LogContextLayer, MethodOverrideLayer};
use autumn_web::session::{MemoryStore, SessionConfig, SessionLayer};
use axum::body::Body;
use axum::http::{Request, Response};
use futures::FutureExt as _;
use tower::{Layer, Service, service_fn};

/// Enough calls that one allocation for each call is clear.
const CALLS: u64 = 1_000;

/// Fills the extension map of each request before the call.
#[derive(Clone)]
struct Marker;

fn ready_service(_req: Request<Body>) -> std::future::Ready<Result<Response<Body>, Infallible>> {
    std::future::ready(Ok(Response::new(Body::empty())))
}

/// Allocations for each call through `svc`. The requests are made by `build`
/// before the measurement starts.
fn allocations_per_call<S>(svc: &mut S, build: &dyn Fn() -> Request<Body>) -> u64
where
    S: Service<Request<Body>>,
{
    let mut requests: Vec<Request<Body>> = (0..CALLS)
        .map(|_| {
            let mut req = build();
            req.extensions_mut().insert(Marker);
            req
        })
        .collect();
    // Warm up: lazy statics, callsite registration, first locks.
    for _ in 0..10 {
        let _ = svc.call(build()).now_or_never();
    }
    let info = allocation_counter::measure(|| {
        for req in std::mem::take(&mut requests) {
            let response = svc.call(req).now_or_never();
            std::hint::black_box(&response);
            drop(response);
        }
    });
    assert_eq!(
        info.count_total % CALLS,
        0,
        "{} blocks in {CALLS} calls is not the same for each call",
        info.count_total
    );
    info.count_total / CALLS
}

fn get() -> Request<Body> {
    Request::builder().uri("/").body(Body::empty()).unwrap()
}

fn json_post() -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/users")
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap()
}

#[test]
fn log_context_future_is_not_boxed() {
    let mut svc =
        LogContextLayer::new(Arc::new(ParameterFilter::default())).layer(service_fn(ready_service));
    let per_call = allocations_per_call(&mut svc, &get);
    println!("log context: {per_call} allocations per call");
    assert_eq!(
        per_call, 2,
        "the log context must allocate only its two scopes, not a boxed future"
    );
}

#[test]
fn session_future_without_a_cookie_is_not_boxed() {
    let mut svc = SessionLayer::new(MemoryStore::new(), SessionConfig::default())
        .layer(service_fn(ready_service));
    let per_call = allocations_per_call(&mut svc, &get);
    println!("session, no cookie: {per_call} allocations per call");
    assert_eq!(
        per_call, 3,
        "a request without a session cookie must allocate only the session id, \
         the session state and its request extension"
    );
}

#[test]
fn method_override_future_is_not_boxed_for_an_ineligible_request() {
    let mut svc = MethodOverrideLayer::new().layer(service_fn(ready_service));
    for (name, build) in [
        ("GET", &get as &dyn Fn() -> Request<Body>),
        ("JSON POST", &json_post),
    ] {
        let per_call = allocations_per_call(&mut svc, build);
        println!("method override, {name}: {per_call} allocations per call");
        assert_eq!(
            per_call, 0,
            "method override must not allocate for a {name} it does not rewrite"
        );
    }
}

#[cfg(feature = "idempotency")]
#[test]
fn idempotency_replay_future_is_not_boxed_without_a_replay() {
    let mut svc = autumn_web::idempotency::IdempotencyReplayLayer.layer(service_fn(ready_service));
    let per_call = allocations_per_call(&mut svc, &json_post);
    println!("idempotency replay, no replay: {per_call} allocations per call");
    assert_eq!(
        per_call, 0,
        "the replay layer must not allocate for a request it does not replay"
    );
}

#[cfg(feature = "idempotency")]
#[test]
fn submit_token_future_is_not_boxed_without_a_form() {
    let store = Arc::new(autumn_web::idempotency::MemoryIdempotencyStore::new(
        std::time::Duration::from_secs(60),
    ));
    let mut svc = autumn_web::security::SubmitTokenLayer::new(
        store,
        &autumn_web::security::SubmitTokenConfig::default(),
    )
    .layer(service_fn(ready_service));
    for (name, build) in [
        ("GET", &get as &dyn Fn() -> Request<Body>),
        ("JSON POST", &json_post),
    ] {
        let per_call = allocations_per_call(&mut svc, build);
        println!("submit token, {name}: {per_call} allocations per call");
        assert_eq!(
            per_call, 4,
            "a {name} has no token to read: the layer must allocate only its \
             new token, the field name and two request extensions"
        );
    }
}
