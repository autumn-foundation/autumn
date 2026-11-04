//! Isolated integration test: allocation gate for the per-request layers that
//! only *carry* a request's matched route and request id
//! (`autumn_web::middleware::{AccessLogLayer, LogContextLayer}`).
//!
//! Its own binary for the same reason `config_alloc_gate` is:
//! `allocation-counter` installs a process-wide counting `#[global_allocator]`,
//! a process-wide side effect per CLAUDE.md's isolated-test rules.
//!
//! # What is measured
//!
//! Both values are already shared by the time these layers see them: axum's
//! `MatchedPath` is an `Arc<str>`, and `RequestId` is a few bytes with no heap
//! part that renders itself on the stack. Keeping them must cost a refcount
//! bump or a copy of those bytes, never a fresh `String`:
//!
//! - The access log (primary and fallback) keeps the `MatchedPath` and the
//!   `RequestId` until it emits, and renders the id into a stack buffer then.
//!   The fallback runs on every request and almost never emits.
//! - The log context keeps the `RequestId` and renders it to text only for a
//!   reader (`snapshot`, `current_request_id`), which most requests never have.
//!
//! Each layer is driven with requests that carry the route and id and with
//! requests that carry two unrelated extensions instead, so both arms' extension
//! maps hold the same number of entries and everything else the layer does
//! (its future, the fallback's sentinel, the context's `Arc`) cancels out. The
//! difference is what carrying the route and id costs. Run against the code it
//! replaced, this gate measured 2 allocations a call for the primary access
//! log (both copied into `String`s), 2 for the fallback (which copied whichever
//! it found; this gate hands it both, while in the real stack it sits outside
//! `RequestIdLayer` and finds only the route), and 1 for the log context (the
//! id rendered to text). Now each is 0.
//!
//! No subscriber is installed, so the access-log event itself is disabled: the
//! gate measures the call-side capture, which is where the copies were.
//!
//! A failure here means a refactor put a copy of the route or the id back on
//! the request path; re-measure and re-derive rather than raising a ceiling.

use std::convert::Infallible;
use std::sync::{Arc, Mutex};

/// The futures of the public ingress services (#2830).
#[path = "request_meta_alloc_gate/ingress_futures.rs"]
mod ingress_futures;

use autumn_web::log::filter::ParameterFilter;
use autumn_web::middleware::{AccessLogLayer, LogContextLayer, RequestId};
use axum::body::Body;
use axum::extract::MatchedPath;
use axum::http::{Request, Response};
use futures::FutureExt as _;
use tower::{Layer, Service, ServiceExt as _, service_fn};

/// Enough calls that a per-call allocation cannot hide in noise.
const CALLS: u64 = 1_000;

/// Stand-ins for the route and id in the comparison arm, so its extension map
/// is as full as the measured arm's.
#[derive(Clone)]
struct MarkerA;
#[derive(Clone)]
struct MarkerB;

/// A real `MatchedPath`. Its field is private to axum, so route one request
/// and keep the extension the router inserted.
fn matched_path() -> MatchedPath {
    let slot: Arc<Mutex<Option<MatchedPath>>> = Arc::new(Mutex::new(None));
    let seen = Arc::clone(&slot);
    let router: axum::Router = axum::Router::new().route_service(
        "/users/{id}",
        service_fn(move |req: Request<Body>| {
            *seen.lock().unwrap() = req.extensions().get::<MatchedPath>().cloned();
            std::future::ready(Ok::<_, Infallible>(Response::new(Body::empty())))
        }),
    );
    let request = Request::builder()
        .uri("/users/7")
        .body(Body::empty())
        .unwrap();
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(router.oneshot(request))
        .unwrap();
    let matched = slot.lock().unwrap().take().expect("the route matched");
    assert_eq!(matched.as_str(), "/users/{id}");
    matched
}

fn request_id() -> RequestId {
    RequestId::parse_inbound("0e4f1c2a-9b3d-4e5f-8a6b-7c8d9e0f1a2b").expect("well-formed id")
}

fn ready_service(_req: Request<Body>) -> std::future::Ready<Result<Response<Body>, Infallible>> {
    std::future::ready(Ok(Response::new(Body::empty())))
}

/// Allocations over `CALLS` calls through `svc`, with the requests built by
/// `build` outside the measured window.
fn allocations<S>(svc: &mut S, build: &dyn Fn() -> Request<Body>) -> u64
where
    S: Service<Request<Body>>,
{
    let mut requests: Vec<Request<Body>> = (0..CALLS).map(|_| build()).collect();
    // Warm up: lazy statics, callsite registration, the first lock, and so on.
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
    info.count_total
}

#[test]
fn access_log_carries_the_route_and_id_without_allocating() {
    let matched = matched_path();
    let id = request_id();
    let carrying = || {
        let mut req = Request::builder()
            .uri("/users/7")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(matched.clone());
        req.extensions_mut().insert(id.clone());
        req
    };
    let bare = || {
        let mut req = Request::builder()
            .uri("/users/7")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(MarkerA);
        req.extensions_mut().insert(MarkerB);
        req
    };
    for (name, layer) in [
        ("primary", AccessLogLayer::new(Vec::new())),
        ("fallback", AccessLogLayer::fallback(Vec::new())),
    ] {
        let mut svc = layer.layer(service_fn(ready_service));
        let with = allocations(&mut svc, &carrying);
        let without = allocations(&mut svc, &bare);
        println!("{name}: {CALLS} calls, {with} allocations with route and id, {without} without");
        assert_eq!(
            with, without,
            "the {name} access log must carry the route and id without copying them"
        );
    }
}

#[test]
fn log_context_carries_the_request_id_without_allocating() {
    let id = request_id();
    let carrying = || {
        let mut req = Request::builder().uri("/").body(Body::empty()).unwrap();
        req.extensions_mut().insert(id.clone());
        req
    };
    let bare = || {
        let mut req = Request::builder().uri("/").body(Body::empty()).unwrap();
        req.extensions_mut().insert(MarkerA);
        req
    };
    let mut svc =
        LogContextLayer::new(Arc::new(ParameterFilter::default())).layer(service_fn(ready_service));
    let with = allocations(&mut svc, &carrying);
    let without = allocations(&mut svc, &bare);
    println!("log context: {CALLS} calls, {with} allocations with an id, {without} without");
    assert_eq!(
        with, without,
        "the log context must keep the request id without rendering it"
    );
}
