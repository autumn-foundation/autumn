//! Request criticality for admission control (issue #3068).
//!
//! [`CriticalityLayer`] finds the [`Criticality`] of each request and:
//!
//! - inserts it as a request extension, which
//!   [`LoadShedLayer`](crate::middleware::LoadShedLayer) reads;
//! - runs the rest of the request in [`crate::admission::current_criticality`]
//!   scope, so the HTTP client sends it on outbound calls.
//!
//! The class comes from the `X-Autumn-Criticality` header when
//! `server.admission.trust_criticality_header = true` and the header is
//! valid. Otherwise it comes from the route's `criticality = "..."`
//! attribute, else it is `default`.
//!
//! The router installs this layer only when a route sets a criticality or
//! the header is trusted, so other apps pay nothing.

// autumn-panic-gate: request-path module — production code path must be panic-free.
// See CONTRIBUTING.md "Request-path panic gate".
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

use std::task::{Context, Poll};

use axum::http::Request;
use tower::{Layer, Service};

use crate::admission::{CRITICALITY_HEADER, Criticality};
use crate::router::RouteAttrTable;

/// Tower [`Layer`] that sets the [`Criticality`] of each request.
#[derive(Clone)]
pub struct CriticalityLayer {
    table: RouteAttrTable<Criticality>,
    trust_header: bool,
}

impl CriticalityLayer {
    /// The layer for a route table, or `None` when the table is empty and the
    /// header is not trusted (nothing to do).
    pub(crate) fn from_table(
        table: RouteAttrTable<Criticality>,
        trust_header: bool,
    ) -> Option<Self> {
        (trust_header || !table.is_empty()).then_some(Self {
            table,
            trust_header,
        })
    }

    fn resolve<B>(&self, req: &Request<B>) -> Criticality {
        // An outer user layer may set the class itself; keep it.
        if let Some(c) = req.extensions().get::<Criticality>() {
            return *c;
        }
        if self.trust_header
            && let Some(c) = req
                .headers()
                .get(CRITICALITY_HEADER)
                .and_then(|v| v.to_str().ok())
                .and_then(Criticality::from_name)
        {
            return c;
        }
        req.extensions()
            .get::<axum::extract::MatchedPath>()
            .and_then(|p| self.table.get(p.as_str()))
            .and_then(|by_method| by_method.get(req.method()))
            .copied()
            .unwrap_or_default()
    }
}

impl<S> Layer<S> for CriticalityLayer {
    type Service = CriticalityService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        CriticalityService {
            inner,
            layer: self.clone(),
        }
    }
}

/// Tower [`Service`] produced by [`CriticalityLayer`].
#[derive(Clone)]
pub struct CriticalityService<S> {
    inner: S,
    layer: CriticalityLayer,
}

impl<S, B> Service<Request<B>> for CriticalityService<S>
where
    S: Service<Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = tokio::task::futures::TaskLocalFuture<Criticality, S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request<B>) -> Self::Future {
        let criticality = self.layer.resolve(&req);
        req.extensions_mut().insert(criticality);
        crate::admission::scope_criticality(criticality, self.inner.call(req))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::routing::get;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tower::ServiceExt;

    async fn echo(req: Request<Body>) -> String {
        let ext = req.extensions().get::<Criticality>().copied();
        let task = crate::admission::current_criticality();
        format!("{ext:?}/{task:?}")
    }

    fn table(path: &str, method: http::Method, c: Criticality) -> RouteAttrTable<Criticality> {
        let mut by_method = HashMap::new();
        by_method.insert(method, c);
        let mut t = HashMap::new();
        t.insert(path.to_owned(), by_method);
        Arc::new(t)
    }

    async fn call(layer: CriticalityLayer, uri: &str, header: Option<&str>) -> String {
        let app = Router::new()
            .route("/batch/{id}", get(echo).post(echo))
            .route("/other", get(echo))
            .layer(layer);
        let mut req = Request::builder().uri(uri);
        if let Some(h) = header {
            req = req.header(CRITICALITY_HEADER, h);
        }
        let resp = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[test]
    fn empty_table_without_header_trust_installs_nothing() {
        assert!(CriticalityLayer::from_table(Arc::new(HashMap::new()), false).is_none());
        assert!(CriticalityLayer::from_table(Arc::new(HashMap::new()), true).is_some());
    }

    #[tokio::test]
    async fn route_value_is_set_on_the_extension_and_task() {
        let layer = CriticalityLayer::from_table(
            table("/batch/{id}", http::Method::GET, Criticality::Sheddable),
            false,
        )
        .unwrap();
        assert_eq!(
            call(layer.clone(), "/batch/7", None).await,
            "Some(Sheddable)/Some(Sheddable)"
        );
        assert_eq!(
            call(layer, "/other", None).await,
            "Some(Default)/Some(Default)",
            "a route without an entry is default"
        );
    }

    #[tokio::test]
    async fn header_is_ignored_unless_trusted() {
        let layer = CriticalityLayer::from_table(
            table("/batch/{id}", http::Method::GET, Criticality::Sheddable),
            false,
        )
        .unwrap();
        assert_eq!(
            call(layer, "/batch/7", Some("critical")).await,
            "Some(Sheddable)/Some(Sheddable)"
        );
    }

    #[tokio::test]
    async fn an_extension_set_by_an_outer_layer_is_kept() {
        let layer = CriticalityLayer::from_table(
            table("/batch/{id}", http::Method::GET, Criticality::Sheddable),
            true,
        )
        .unwrap();
        let app = Router::new().route("/batch/{id}", get(echo)).layer(layer);
        let mut req = Request::builder()
            .uri("/batch/1")
            .header(CRITICALITY_HEADER, "default")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(Criticality::Critical);
        let resp = app.oneshot(req).await.unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        assert_eq!(&bytes[..], b"Some(Critical)/Some(Critical)");
    }

    #[tokio::test]
    async fn trusted_header_replaces_the_route_value() {
        let layer = CriticalityLayer::from_table(
            table("/batch/{id}", http::Method::GET, Criticality::Sheddable),
            true,
        )
        .unwrap();
        assert_eq!(
            call(layer.clone(), "/batch/7", Some("Critical")).await,
            "Some(Critical)/Some(Critical)"
        );
        assert_eq!(
            call(layer, "/batch/7", Some("bogus")).await,
            "Some(Sheddable)/Some(Sheddable)",
            "an invalid header falls back to the route"
        );
    }
}
