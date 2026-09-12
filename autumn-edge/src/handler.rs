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
//!
//! ## Why the tuple check has two layers
//!
//! axum also implements its extractor traits for plain tuples, so a single
//! handler parameter can itself be a tuple of extractors
//! (`async fn h(combo: (Path<T>, HeaderMap))`). [`EdgeExtract`]'s own tuple
//! impls exist to accept axum's *handler-arity* tuple — the `(M, E1, ..,
//! En)` shape [`edge_get`]'s doc explains below — and that shape is, on
//! purpose, indistinguishable at the type level from a tuple a handler
//! author wrote by hand: both are "some type, followed by more types."
//! Judging each position with [`EdgeExtract`] itself (recursively) would
//! make that ambiguity exploitable: `(Extension<Hidden>, HeaderMap)` would
//! read as a valid handler-arity tuple with `Extension<Hidden>` sitting in
//! the free marker slot, smuggling it straight past the whitelist. Each
//! position is judged by [`EdgeLeaf`] instead — sealed, and (this is the
//! part that matters) never implemented with a free/unconstrained type
//! parameter anywhere, tuples included: [`EdgeLeaf`] does have tuple impls,
//! for a handler author's own `(Path<T>, HeaderMap)`-style grouping, but
//! every position in those still has to satisfy `EdgeLeaf` itself, all the
//! way down — so nesting one more tuple around `Extension<T>` still cannot
//! manufacture a leaf:
//!
//! ```compile_fail
//! use autumn_edge::edge_get;
//!
//! type Hidden = axum::Extension<u32>;
//!
//! async fn handler(combo: (Hidden, http::HeaderMap)) -> &'static str {
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

    /// Not nameable outside this crate, so [`super::EdgeLeaf`] cannot be
    /// implemented downstream, and — just as load-bearing — cannot be
    /// implemented for a tuple *inside* this module either, since nothing
    /// outside `handler.rs` ever writes one. See [`super::EdgeLeaf`] for why
    /// that absence is what keeps [`super::EdgeExtract`]'s tuple check sound.
    pub trait LeafSealed {}
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
/// Blanket-implemented for exactly [`axum::extract::Path`],
/// [`axum::extract::Query`], [`http::HeaderMap`], [`EdgeCache`](crate::extract::EdgeCache),
/// the empty tuple (a handler with no extractors), and tuples of up to
/// sixteen [`EdgeLeaf`] types — nothing else. This is what makes
/// [`EdgeHandler`] a whitelist rather than a blacklist: a new native-only
/// extractor needs no refusal added here, because it was never on the list
/// to begin with.
///
/// Sealed, but — unlike [`EdgeLeaf`] — deliberately not the trait each tuple
/// position is judged by; see the module doc's "Why the tuple check has two
/// layers" section for why that distinction is load-bearing, not stylistic.
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

/// One extractor an `#[edge]` handler may take.
///
/// Standing alone, inside a tuple of leaves grouped by hand (axum implements
/// its extractor traits for plain tuples, so `(Path<T>, HeaderMap)` is itself
/// a single valid extractor), or inside the handler-arity tuple
/// [`EdgeExtract`] validates.
///
/// Sealed. Every impl here — including the tuple ones below — bounds *each*
/// element by [`EdgeLeaf`] itself, with no free/unconstrained type parameter
/// anywhere. That is what makes nesting a bad extractor one tuple deeper
/// (`(Extension<Hidden>, HeaderMap)` as a single handler parameter) fail
/// exactly like naming it directly: every position in every [`EdgeLeaf`]
/// tuple impl still has to independently satisfy [`EdgeLeaf`], all the way
/// down, so `Extension<Hidden>` never becomes a leaf just because it is
/// sitting next to one. This is a different — and load-bearing — shape from
/// [`EdgeExtract`]'s own tuple impls (below), which DO carry a free `M` slot
/// for axum's unnameable marker type; recursing through `EdgeExtract` there
/// instead of `EdgeLeaf` would let a rejected extractor hide inside that free
/// slot, which is exactly why each position is judged by `EdgeLeaf`, never
/// `EdgeExtract`, and why `EdgeLeaf` itself must never gain a free-parameter
/// tuple impl of its own.
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not one of the extractors an `#[edge]` handler may use",
    note = "allowed: `Path`, `Query`, `HeaderMap`, `EdgeCache`"
)]
pub trait EdgeLeaf: sealed::LeafSealed {}

impl<T> sealed::LeafSealed for axum::extract::Path<T> {}
impl<T> EdgeLeaf for axum::extract::Path<T> {}

impl<T> sealed::LeafSealed for axum::extract::Query<T> {}
impl<T> EdgeLeaf for axum::extract::Query<T> {}

impl sealed::LeafSealed for http::HeaderMap {}
impl EdgeLeaf for http::HeaderMap {}

impl sealed::LeafSealed for crate::extract::EdgeCache {}
impl EdgeLeaf for crate::extract::EdgeCache {}

/// Implement [`EdgeLeaf`] for a plain tuple of leaves, `(E1, .., En)` for `n`
/// from 1 to 16 — axum's own tuple `FromRequestParts` impls go up to 16
/// elements (`all_the_tuples_no_last_special_case!` in `axum_core::macros`),
/// so stopping short would reject an otherwise-valid grouping axum itself
/// accepts — when each `Ei` is itself an [`EdgeLeaf`]. axum implements
/// `FromRequestParts` for tuples (including the 1-element case), so a
/// handler author can group extractors by hand into a single parameter
/// (`async fn h(combo: (Path<T>, HeaderMap))`, or even `(Path<T>,)` alone),
/// and that grouped tuple is then itself the one extractor occupying a slot
/// in [`EdgeExtract`]'s handler-arity tuple.
///
/// Unlike [`impl_edge_extract_for_handler_arity`]'s macro, there is no free
/// marker parameter here: every `Ei` is bounded by `EdgeLeaf`, so this can
/// never manufacture a leaf out of a rejected extractor — see [`EdgeLeaf`]'s
/// own doc for why that distinction is load-bearing.
macro_rules! impl_edge_leaf_for_tuple {
    ($($t:ident),+) => {
        impl<$($t: EdgeLeaf),+> sealed::LeafSealed for ($($t,)+) {}
        impl<$($t: EdgeLeaf),+> EdgeLeaf for ($($t,)+) {}
    };
}

impl_edge_leaf_for_tuple!(T1);
impl_edge_leaf_for_tuple!(T1, T2);
impl_edge_leaf_for_tuple!(T1, T2, T3);
impl_edge_leaf_for_tuple!(T1, T2, T3, T4);
impl_edge_leaf_for_tuple!(T1, T2, T3, T4, T5);
impl_edge_leaf_for_tuple!(T1, T2, T3, T4, T5, T6);
impl_edge_leaf_for_tuple!(T1, T2, T3, T4, T5, T6, T7);
impl_edge_leaf_for_tuple!(T1, T2, T3, T4, T5, T6, T7, T8);
impl_edge_leaf_for_tuple!(T1, T2, T3, T4, T5, T6, T7, T8, T9);
impl_edge_leaf_for_tuple!(T1, T2, T3, T4, T5, T6, T7, T8, T9, T10);
impl_edge_leaf_for_tuple!(T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11);
impl_edge_leaf_for_tuple!(T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12);
impl_edge_leaf_for_tuple!(T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13);
impl_edge_leaf_for_tuple!(T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14);
impl_edge_leaf_for_tuple!(
    T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15
);
impl_edge_leaf_for_tuple!(
    T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16
);

/// Implement [`EdgeExtract`] for axum's handler-arity tuple, `(M, E1, ..,
/// En)` for `n` from 1 to 16 (axum's own `impl_handler!` — see
/// `all_the_tuples!` in `axum_core::macros` — goes up to 16 extractors, so
/// stopping short would reject an otherwise-valid handler axum itself
/// accepts), when each `Ei` is an [`EdgeLeaf`].
///
/// axum's own `Handler<T, S>` blanket impl (see `impl_handler!` in
/// `axum::handler`) does not use `T = (E1, .., En)` — it prepends a private
/// marker type that records whether the last extractor reads the body or
/// only the request parts, so `T = (M, E1, .., En)`. `M` is not nameable
/// outside axum, so it is left as a free type parameter here: this trait
/// only judges the extractor types, and axum's own bound (required
/// alongside this one everywhere `EdgeHandler` is used) already proves the
/// tuple is a real, valid handler signature.
///
/// Each `Ei` is bounded by [`EdgeLeaf`], not [`EdgeExtract`] recursively —
/// see the module doc for why that is the one detail this macro cannot get
/// wrong.
macro_rules! impl_edge_extract_for_handler_arity {
    ($($t:ident),+) => {
        impl<M, $($t: EdgeLeaf),+> sealed::ExtractSealed for (M, $($t,)+) {}
        impl<M, $($t: EdgeLeaf),+> EdgeExtract for (M, $($t,)+) {}
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
impl_edge_extract_for_handler_arity!(T1, T2, T3, T4, T5, T6, T7, T8, T9);
impl_edge_extract_for_handler_arity!(T1, T2, T3, T4, T5, T6, T7, T8, T9, T10);
impl_edge_extract_for_handler_arity!(T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11);
impl_edge_extract_for_handler_arity!(T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12);
impl_edge_extract_for_handler_arity!(T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13);
impl_edge_extract_for_handler_arity!(T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14);
impl_edge_extract_for_handler_arity!(
    T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15
);
impl_edge_extract_for_handler_arity!(
    T1, T2, T3, T4, T5, T6, T7, T8, T9, T10, T11, T12, T13, T14, T15, T16
);

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

    /// A handler author's own grouped tuple extractor (axum implements
    /// `FromRequestParts` for plain tuples, so `(Path<T>, HeaderMap)` is
    /// itself one valid extractor) must be accepted the same as writing the
    /// two extractors as separate parameters (Codex review on #2739, round
    /// 11, P2).
    async fn with_grouped_tuple((Path(name), headers): (Path<String>, HeaderMap)) -> String {
        format!("{name}{}", headers.len())
    }

    /// The one-element case of the same grouping (`(Path<T>,)` alone) — axum
    /// implements `FromRequestParts` for 1-tuples too, so this must be
    /// accepted the same as writing `Path<T>` directly (Codex review on
    /// #2739, round 13, P2).
    async fn with_one_element_grouped_tuple((Path(name),): (Path<String>,)) -> String {
        name
    }

    /// axum's own handler-arity impl (`impl_handler!` via `all_the_tuples!`
    /// in `axum_core::macros`) goes up to 16 extractors; stopping the
    /// `EdgeExtract`/`EdgeLeaf` tuple macros at 8 rejected a handler axum
    /// itself accepts. 16 separate extractors, then the same 16 grouped as
    /// one hand-written tuple, both exercise the new maximum arity (Codex
    /// review on #2739, round 14, P2).
    #[allow(clippy::too_many_arguments)]
    async fn with_sixteen_extractors(
        _e1: HeaderMap,
        _e2: HeaderMap,
        _e3: HeaderMap,
        _e4: HeaderMap,
        _e5: HeaderMap,
        _e6: HeaderMap,
        _e7: HeaderMap,
        _e8: HeaderMap,
        _e9: HeaderMap,
        _e10: HeaderMap,
        _e11: HeaderMap,
        _e12: HeaderMap,
        _e13: HeaderMap,
        _e14: HeaderMap,
        _e15: HeaderMap,
        cache: EdgeCache,
    ) -> String {
        cache.get_string("k").unwrap_or_default()
    }

    #[allow(clippy::type_complexity)]
    async fn with_sixteen_grouped_extractors(
        _group: (
            HeaderMap,
            HeaderMap,
            HeaderMap,
            HeaderMap,
            HeaderMap,
            HeaderMap,
            HeaderMap,
            HeaderMap,
            HeaderMap,
            HeaderMap,
            HeaderMap,
            HeaderMap,
            HeaderMap,
            HeaderMap,
            HeaderMap,
            HeaderMap,
        ),
    ) -> &'static str {
        "ok"
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
        let _ = edge_get(with_grouped_tuple);
        let _ = edge_get(with_one_element_grouped_tuple);
        let _ = edge_get(with_sixteen_extractors);
        let _ = edge_get(with_sixteen_grouped_extractors);
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
