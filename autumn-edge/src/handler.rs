//! The type-system half of the edge capability check (AC-5).
//!
//! `autumn build` shells out to `cargo`, so the *compiler* is the enforcement
//! point for "this handler cannot run at the edge". [`EdgeHandler`] is the
//! bound that turns an unavailable extractor into an actionable message
//! instead of a wall of trait-resolution noise: the handler is required to be
//! an axum handler over [`EdgeState`] — a unit state — so any extractor that
//! needs real application state (`Db`, `Session`, `Clock`, `Rng`, …) simply
//! does not fit, and the diagnostic explains why.

use crate::route::EdgeState;

mod sealed {
    /// Not nameable outside this crate, so [`super::EdgeHandler`] cannot be
    /// implemented downstream: the set of edge-eligible handlers is exactly
    /// the set of axum handlers over [`super::EdgeState`], by construction.
    pub trait Sealed<T> {}
}

impl<H, T> sealed::Sealed<T> for H where H: axum::handler::Handler<T, EdgeState> {}

/// Marker bound for handlers the edge lane can serve.
///
/// Sealed and blanket-implemented; there is nothing to implement by hand.
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot serve as an `#[edge]` handler",
    label = "this handler uses an extractor or return type unavailable at the edge",
    note = "edge handlers may use extractors that work for any state (e.g. `Path`, `Query`, `HeaderMap`, `EdgeCache`) and must not use native-only extractors (e.g. `Db`, `Session`, `Clock`, `Rng`)",
    note = "remove `#[edge]` from this route, or replace the offending extractor; see docs/guide/edge.md"
)]
pub trait EdgeHandler<T>: sealed::Sealed<T> {}

impl<H, T> EdgeHandler<T> for H where H: axum::handler::Handler<T, EdgeState> {}

/// Adapt a `GET` handler into the `MethodRouter` an
/// [`EdgeRoute`](crate::route::EdgeRoute) carries.
///
/// This is what the `__autumn_edge_route_*` companion the `#[edge]` macro
/// emits calls. The redundant-looking [`EdgeHandler`] bound is load bearing:
/// it is the bound that carries the diagnostic.
///
/// `HEAD` is served by the same handler — axum's `MethodRouter` routes it to
/// the `GET` service and strips the body — so the edge lane needs no separate
/// registration for it.
pub fn edge_get<H, T>(handler: H) -> axum::routing::MethodRouter<EdgeState>
where
    H: EdgeHandler<T> + axum::handler::Handler<T, EdgeState>,
    T: 'static,
{
    axum::routing::get(handler)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::EdgeCache;
    use axum::extract::{Path, Query};
    use http::HeaderMap;
    use std::collections::BTreeMap;

    async fn nullary() -> &'static str {
        "ok"
    }

    async fn with_path(Path(name): Path<String>) -> String {
        name
    }

    async fn with_query(Query(q): Query<BTreeMap<String, String>>) -> String {
        q.into_iter().fold(String::new(), |mut acc, (k, v)| {
            acc.push_str(&k);
            acc.push('=');
            acc.push_str(&v);
            acc
        })
    }

    async fn with_headers(headers: HeaderMap) -> String {
        headers.len().to_string()
    }

    async fn with_cache(cache: EdgeCache) -> String {
        cache.get_string("k").unwrap_or_default()
    }

    async fn with_everything(
        Path(name): Path<String>,
        headers: HeaderMap,
        cache: EdgeCache,
    ) -> (http::StatusCode, [(&'static str, &'static str); 1], String) {
        (
            http::StatusCode::OK,
            [("x-edge-lane", "edge")],
            format!("{name}{}{}", headers.len(), cache.get_string("k").is_some()),
        )
    }

    /// The prelude's extractors are exactly the ones an edge handler may use;
    /// if any of these stopped satisfying the bound this would not compile.
    #[test]
    fn prelude_extractors_are_edge_eligible() {
        let _ = edge_get(nullary);
        let _ = edge_get(with_path);
        let _ = edge_get(with_query);
        let _ = edge_get(with_headers);
        let _ = edge_get(with_cache);
        let _ = edge_get(with_everything);
    }

    #[test]
    fn edge_get_produces_a_method_router_that_also_answers_head() {
        use tower::ServiceExt as _;

        let router = crate::router::build_edge_router(vec![crate::route::EdgeRoute {
            method: http::Method::GET,
            path: "/hello",
            handler: edge_get(nullary),
            name: "nullary",
            needs: &[],
        }]);

        for method in [http::Method::GET, http::Method::HEAD] {
            let request = http::Request::builder()
                .method(method.clone())
                .uri("/hello")
                .body(axum::body::Body::empty())
                .expect("request");
            let response = futures::executor::block_on(router.clone().oneshot(request))
                .expect("dispatch is infallible");
            assert_eq!(response.status(), http::StatusCode::OK, "{method}");
        }
    }
}
