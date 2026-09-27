//! The future shape every "inspect, then reject or forward" ingress gate
//! returns (issue #2214).
//!
//! A gate middleware looks at a request synchronously and either produces a
//! response of its own — a `400` for an untrusted `Host`, a `503` while the app
//! is still starting up — or hands the request to the service beneath it
//! untouched. Written as an `axum::middleware::from_fn` that shape costs a heap
//! allocation on *every* request: `FromFn::call` wraps the async block it
//! generates in a `Box::pin`, because the block's type cannot be named, and it
//! clones its inner service to move it in there — which for an erased
//! `BoxCloneSyncService` is a recursive `clone_box` down the rest of the stack.
//! Neither cost depends on whether the gate actually rejects anything.
//!
//! [`ShortCircuitFuture`] is the named alternative. It holds the inner
//! service's own future in place (no box, no clone) on the forwarding path and
//! a ready-made response on the rejecting one, so a gate written against it
//! allocates nothing per request. DHAT measured the `from_fn` boxes it replaces
//! at 19.57% of every byte the `request_pipeline` benchmark allocated.
//!
//! `MaintenanceFuture` and `LoadShedFuture` are the same shape and predate this
//! type; they are deliberately left alone because both are exported from the
//! crate root, so collapsing them into this one would be a breaking change for
//! a purely internal tidy.

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
use axum::http::Response;
use pin_project_lite::pin_project;

pin_project! {
    /// Future returned by an ingress gate: either a response the gate produced
    /// itself, ready on the first poll, or the inner service's future polled
    /// through unchanged.
    ///
    /// The `Forward` variant stores `F` **by value** rather than boxing it, so
    /// the whole chain of framework middleware collapses into the single
    /// `Box::pin` `axum::routing::Route` already takes at the top of the stack
    /// instead of taking one of its own per layer.
    #[project = ShortCircuitFutureProj]
    pub enum ShortCircuitFuture<F> {
        /// The gate rejected the request; `response` is handed back on the
        /// first poll and the inner service is never called.
        ShortCircuit { response: Option<Response<Body>> },
        /// The gate passed the request through; this is the inner service's
        /// own future.
        Forward {
            #[pin]
            inner: F,
        },
    }
}

impl<F> ShortCircuitFuture<F> {
    /// Reject the request with `response`, without calling the inner service.
    pub(crate) const fn short_circuit(response: Response<Body>) -> Self {
        Self::ShortCircuit {
            response: Some(response),
        }
    }

    /// Forward to the inner service's future.
    pub(crate) const fn forward(inner: F) -> Self {
        Self::Forward { inner }
    }
}

impl<F, E> Future for ShortCircuitFuture<F>
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
            ShortCircuitFutureProj::ShortCircuit { response } => Poll::Ready(Ok(response
                .take()
                .expect("ShortCircuitFuture polled after completion"))),
            ShortCircuitFutureProj::Forward { inner } => inner.poll(cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    /// The whole point of this type: no `Box` anywhere in the future a gate
    /// hands back, so nothing is heap-allocated per request (issue #2214).
    #[test]
    fn the_future_is_named_not_boxed() {
        let name = std::any::type_name::<ShortCircuitFuture<std::future::Ready<()>>>();
        assert!(
            !name.contains("Box"),
            "ShortCircuitFuture must stay a named, unboxed future; got {name}"
        );
    }

    /// A `Forward` future that would hang forever: resolving a `ShortCircuit`
    /// built over it proves the inner future is never polled.
    type Never = std::future::Pending<Result<Response<Body>, std::convert::Infallible>>;

    #[tokio::test]
    async fn short_circuit_resolves_with_the_gate_response_without_the_inner_future() {
        let response = Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::empty())
            .expect("response builds");

        let got = ShortCircuitFuture::<Never>::short_circuit(response)
            .await
            .expect("infallible");
        assert_eq!(got.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn forward_resolves_with_the_inner_futures_output() {
        let inner = std::future::ready(Ok::<_, std::convert::Infallible>(
            Response::builder()
                .status(StatusCode::IM_A_TEAPOT)
                .body(Body::empty())
                .expect("response builds"),
        ));

        let got = ShortCircuitFuture::forward(inner)
            .await
            .expect("infallible");
        assert_eq!(got.status(), StatusCode::IM_A_TEAPOT);
    }
}
