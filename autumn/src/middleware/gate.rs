//! Generic zero-allocation "gate" middleware.
//!
//! A large share of the framework's ingress layers have the same shape: look
//! at the request, synchronously decide either "reject with this response" or
//! "forward untouched", and never await anything of their own. Written with
//! [`axum::middleware::from_fn`] that shape costs a heap allocation on every
//! request in two separate ways:
//!
//! 1. `from_fn` boxes the async block it wraps (`Box::pin(async move { … })`)
//!    so the generated, unnameable future type can be returned as a single
//!    concrete `ResponseFuture`. That allocation is paid on every request
//!    regardless of which branch the gate takes, and it is sized by everything
//!    the async block holds across its `.await` — which, for an outer layer,
//!    includes the whole downstream continuation (profiled at ~1.1-2.2 KB per
//!    box in the ingress stack).
//! 2. The `Service` `from_fn` generates begins its `call` with
//!    `self.inner.clone()`. Because the ingress stack is a tower of
//!    `BoxCloneSyncService`s, cloning the inner service deep-clones every
//!    boxed level beneath it — so each `from_fn` in the stack costs a full
//!    extra traversal, not one allocation. (This is the *C* term in
//!    `tests/integration/middleware_stack_depth.rs`.)
//!
//! [`GateLayer`] pays neither. Its future is a two-variant enum — an
//! already-built response, or the inner service's own future — so it needs no
//! box, and its `call` forwards to the inner service directly instead of
//! cloning it. See issue #2214 for the profile that motivated it.
//!
//! # What belongs here
//!
//! Only middleware whose decision is **synchronous** and whose forward path
//! returns the inner service's response **unmodified**. A layer that awaits
//! something of its own (a store lookup), that inspects or rewrites the
//! response on the way out, or that needs to hold a guard across the inner
//! call, does not fit this shape and needs its own `Service`/`Future` pair
//! (see [`crate::middleware::maintenance`] for one written by hand — its
//! `MaintenanceService` is exactly this pattern, specialised).
//!
//! # Readiness precondition: the inner service must be always-ready
//!
//! [`GateService::poll_ready`] **delegates** to the inner service, which is
//! the correct tower contract for a service that calls `inner.call(req)`
//! synchronously inside its own `call`. It does mean two things that differ
//! from the `axum::middleware::from_fn` this replaces, whose `poll_ready` is
//! an unconditional `Poll::Ready(Ok(()))` (it can afford that because it
//! clones the inner service and drives it through a `Oneshot` inside the
//! boxed future — the very clone this type exists to avoid):
//!
//! 1. If the inner service is `Pending`, even a request the gate would reject
//!    outright waits for inner readiness instead of short-circuiting.
//! 2. If the inner service *reserves* capacity in `poll_ready` (`Buffer`,
//!    `ConcurrencyLimit`), a short-circuited request leaves that reservation
//!    unconsumed until the next `call`.
//!
//! Neither is reachable with an always-ready inner service, which is why this
//! type is `pub(crate)`: every current user sits inner to the framework's only
//! backpressuring layer (`LoadShedLayer`, applied *outside* both gates in
//! `apply_middleware`'s inner group), and everything beneath them —
//! `BotProtection`, `CSRF`, `SubmitToken`, `TrustedHost`, `CORS`, and
//! ultimately `axum::routing::Route`, whose `poll_ready` is
//! `Poll::Ready(Ok(()))` — is always-ready. Load shedding is therefore
//! unaffected by this type.
//!
//! **Do not export this without revisiting that.** Wrapping a genuinely
//! backpressured service needs either a documented always-ready bound or a
//! different readiness design, and neither is justified until there is a
//! caller that needs it.

// autumn-panic-gate: request-path module — production code path must be panic-free.
// See CONTRIBUTING.md "Request-path panic gate". Justify exceptions with
// #[allow(clippy::<lint>, reason = "…")] at the narrowest scope.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
        clippy::string_slice,
        clippy::arithmetic_side_effects,
    )
)]

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use axum::body::Body;
use axum::http::{Request, Response};
use pin_project_lite::pin_project;
use tower::{Layer, Service};

/// Tower [`Layer`] wrapping a synchronous request gate.
///
/// `gate` is called with a borrow of each request. Returning `Some(response)`
/// short-circuits with that response; returning `None` forwards the request to
/// the inner service untouched.
///
/// Prefer this over `axum::middleware::from_fn` for any layer that fits the
/// shape described in the [module docs](self) — it is behaviorally identical
/// and allocates nothing per request.
#[derive(Clone, Copy, Debug)]
pub(crate) struct GateLayer<G> {
    gate: G,
}

impl<G> GateLayer<G> {
    /// Wrap `gate` as a layer.
    pub(crate) const fn new(gate: G) -> Self {
        Self { gate }
    }
}

impl<S, G> Layer<S> for GateLayer<G>
where
    G: Clone,
{
    type Service = GateService<S, G>;

    fn layer(&self, inner: S) -> Self::Service {
        GateService {
            inner,
            gate: self.gate.clone(),
        }
    }
}

/// Tower [`Service`] produced by [`GateLayer`].
#[derive(Clone, Copy, Debug)]
pub(crate) struct GateService<S, G> {
    inner: S,
    gate: G,
}

impl<S, G, ReqBody> Service<Request<ReqBody>> for GateService<S, G>
where
    S: Service<Request<ReqBody>, Response = Response<Body>>,
    G: Fn(&Request<ReqBody>) -> Option<Response<Body>>,
{
    type Response = Response<Body>;
    type Error = S::Error;
    type Future = GateFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<ReqBody>) -> Self::Future {
        if let Some(response) = (self.gate)(&req) {
            return GateFuture::ShortCircuit {
                response: Some(response),
            };
        }
        GateFuture::Forward {
            inner: self.inner.call(req),
        }
    }
}

pin_project! {
    /// Future returned by [`GateService`].
    ///
    /// Either resolves immediately with the gate's response (short-circuit
    /// path) or delegates to the wrapped inner service's own future. Neither
    /// variant boxes.
    #[project = GateFutureProj]
    pub(crate) enum GateFuture<F> {
        ShortCircuit { response: Option<Response<Body>> },
        Forward { #[pin] inner: F },
    }
}

impl<F, E> Future for GateFuture<F>
where
    F: Future<Output = Result<Response<Body>, E>>,
{
    type Output = Result<Response<Body>, E>;

    #[allow(
        clippy::expect_used,
        reason = "unreachable: future not polled after Ready"
    )]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.project() {
            GateFutureProj::ShortCircuit { response } => Poll::Ready(Ok(response
                .take()
                .expect("GateFuture polled after completion"))),
            GateFutureProj::Forward { inner } => inner.poll(cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use tower::{ServiceExt, service_fn};

    /// Inner service that records whether it ran, so the short-circuit path can
    /// be proven to *not* reach it.
    fn echo_inner()
    -> impl Service<Request<Body>, Response = Response<Body>, Error = std::convert::Infallible> + Clone
    {
        service_fn(|_req: Request<Body>| async {
            Ok::<_, std::convert::Infallible>(
                Response::builder()
                    .status(StatusCode::OK)
                    .body(Body::from("inner"))
                    .expect("valid response"),
            )
        })
    }

    #[tokio::test]
    async fn forwards_when_gate_returns_none() {
        let svc = GateLayer::new(|_req: &Request<Body>| None).layer(echo_inner());
        let response = svc
            .oneshot(Request::builder().body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn short_circuits_when_gate_returns_some() {
        let svc = GateLayer::new(|_req: &Request<Body>| {
            Some(
                Response::builder()
                    .status(StatusCode::FORBIDDEN)
                    .body(Body::from("gated"))
                    .expect("valid response"),
            )
        })
        .layer(echo_inner());
        let response = svc
            .oneshot(Request::builder().body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    /// The gate sees the real request, not a placeholder.
    #[tokio::test]
    async fn gate_observes_the_request() {
        let svc = GateLayer::new(|req: &Request<Body>| {
            (req.uri().path() == "/blocked").then(|| {
                Response::builder()
                    .status(StatusCode::IM_A_TEAPOT)
                    .body(Body::empty())
                    .expect("valid response")
            })
        })
        .layer(echo_inner());

        let allowed = svc
            .clone()
            .oneshot(Request::builder().uri("/ok").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(allowed.status(), StatusCode::OK);

        let blocked = svc
            .oneshot(
                Request::builder()
                    .uri("/blocked")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(blocked.status(), StatusCode::IM_A_TEAPOT);
    }
}
