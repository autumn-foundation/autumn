//! The type-system half of the edge capability check (AC-5).
//!
//! `autumn build` shells out to `cargo`, so the *compiler* is the enforcement
//! point for "this handler cannot run at the edge". [`EdgeHandler`] is the
//! bound that turns an unavailable extractor into an actionable message
//! instead of a wall of trait-resolution noise.
//!
//! ## Why a whitelist, not just `EdgeState`
//!
//! An early version bounded a handler on `axum::handler::Handler<T,
//! EdgeState>` alone — "must be an axum handler over the unit state" — and
//! nothing more. That bound is too open: axum's `Extension<T>` extractor
//! works for *any* state, so it satisfies `Handler<_, EdgeState>` too, even
//! though the capsule installs no request extensions except [`EdgeCache`](crate::extract::EdgeCache).
//! The same is true of the whole-`Request` extractor: it also works for any
//! state, and a handler that takes it can call `.extensions()` on it by hand.
//! Either shape compiles, passes locally against the origin (where the real
//! extension *is* present), and only diverges at the edge — a silent gap
//! between the two substrates, exactly what this crate exists to close.
//!
//! [`EdgeExtract`] closes it with a positive list instead: an edge handler's
//! parameters must each be one of the few extractors that behave the same
//! way on both substrates. Nothing outside that list satisfies
//! [`EdgeHandler`], whatever name or wrapper hides it — a type alias for
//! `Extension<T>` is still `Extension<T>` to the compiler, and the
//! whole-`Request` extractor is simply not on the list.
//!
//! A type alias does not hide `Extension<T>` from [`edge_get`]:
//!
//! ```compile_fail
//! use autumn_edge::edge_get;
//!
//! type Hidden = axum::Extension<u32>;
//!
//! async fn handler(_ext: Hidden) -> &'static str {
//!     "never reached"
//! }
//!
//! let _ = edge_get(handler);
//! ```
//!
//! Nor does the whole-`Request` extractor, which could read `.extensions()`
//! by hand:
//!
//! ```compile_fail
//! use autumn_edge::edge_get;
//!
//! async fn handler(req: axum::extract::Request) -> &'static str {
//!     let _ = req.extensions();
//!     "never reached"
//! }
//!
//! let _ = edge_get(handler);
//! ```

use crate::route::EdgeState;

mod sealed {
    /// Not nameable outside this crate, so [`super::EdgeHandler`] cannot be
    /// implemented downstream: the set of edge-eligible handlers is exactly
    /// the set of axum handlers over [`super::EdgeState`] whose extractors are
    /// each [`super::EdgeExtract`], by construction.
    pub trait Sealed<T> {}

    /// Not nameable outside this crate, so [`super::EdgeExtract`] cannot be
    /// implemented downstream: the whitelist is exactly the types this module
    /// lists, and nothing else.
    pub trait ExtractSealed {}
}

impl<H, T> sealed::Sealed<T> for H where H: axum::handler::Handler<T, EdgeState> {}

/// Marker bound for handlers the edge lane can serve.
///
/// Sealed and blanket-implemented; there is nothing to implement by hand.
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot serve as an `#[edge]` handler",
    label = "this handler uses an extractor or return type unavailable at the edge",
    note = "edge handlers may use only `Path`, `Query`, `HeaderMap`, `EdgeCache`, and tuples of \
            these — nothing else, including `Extension<T>` (even through a type alias) and the \
            whole-`Request` extractor, satisfies this bound",
    note = "remove `#[edge]` from this route, or replace the offending extractor; see docs/guide/edge.md"
)]
pub trait EdgeHandler<T>: sealed::Sealed<T> {}

impl<H, T> EdgeHandler<T> for H
where
    H: axum::handler::Handler<T, EdgeState>,
    T: EdgeExtract,
{
}

/// The fixed set of extractors an `#[edge]` handler may take.
///
/// Sealed and blanket-implemented for exactly [`axum::extract::Path`],
/// [`axum::extract::Query`], [`http::HeaderMap`], [`EdgeCache`](crate::extract::EdgeCache),
/// the empty tuple (a handler with no extractors), and tuples of up to eight
/// of these — nothing else. This is what makes [`EdgeHandler`] a whitelist
/// rather than a blacklist: a new native-only extractor needs no refusal
/// added here, because it was never on the list to begin with.
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not one of the extractors an `#[edge]` handler may use",
    note = "allowed: `Path`, `Query`, `HeaderMap`, `EdgeCache`, and tuples of these"
)]
pub trait EdgeExtract: sealed::ExtractSealed {}

// axum's zero-argument `Handler` impl uses `T = ((),)`, a one-element tuple
// wrapping unit — not bare `()` — see `impl_handler!`'s neighbor in
// `axum::handler` for the exact shape.
impl sealed::ExtractSealed for ((),) {}
impl EdgeExtract for ((),) {}

impl<T> sealed::ExtractSealed for axum::extract::Path<T> {}
impl<T> EdgeExtract for axum::extract::Path<T> {}

impl<T> sealed::ExtractSealed for axum::extract::Query<T> {}
impl<T> EdgeExtract for axum::extract::Query<T> {}

impl sealed::ExtractSealed for http::HeaderMap {}
impl EdgeExtract for http::HeaderMap {}

impl sealed::ExtractSealed for crate::extract::EdgeCache {}
impl EdgeExtract for crate::extract::EdgeCache {}

/// Implement [`EdgeExtract`] for a tuple of extractor types that are each
/// `EdgeExtract`.
///
/// axum's own `Handler<T, S>` blanket impl (see `impl_handler!` in
/// `axum::handler`) does not use `T = (E1, .., En)` — it prepends a private
/// marker type that records whether the last extractor reads the body or
/// only the request parts, so `T = (M, E1, .., En)`. `M` is not nameable
/// outside axum, so it is left as a free type parameter here: this trait
/// only judges the extractor types, and axum's own bound (required
/// alongside this one everywhere `EdgeHandler` is used) already proves the
/// tuple is a real, valid handler signature.
macro_rules! impl_edge_extract_for_handler_arity {
    ($($t:ident),+) => {
        impl<M, $($t: EdgeExtract),+> sealed::ExtractSealed for (M, $($t,)+) {}
        impl<M, $($t: EdgeExtract),+> EdgeExtract for (M, $($t,)+) {}
    };
}

impl_edge_extract_for_handler_arity!(T1);
impl_edge_extract_for_handler_arity!(T1, T2);
impl_edge_extract_for_handler_arity!(T1, T2, T3);
impl_edge_extract_for_handler_arity!(T1, T2, T3, T4);
impl_edge_extract_for_handler_arity!(T1, T2, T3, T4, T5);
impl_edge_extract_for_handler_arity!(T1, T2, T3, T4, T5, T6);
impl_edge_extract_for_handler_arity!(T1, T2, T3, T4, T5, T6, T7);
impl_edge_extract_for_handler_arity!(T1, T2, T3, T4, T5, T6, T7, T8);

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
