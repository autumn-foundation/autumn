//! Building the edge lane's axum router.
//!
//! The edge router is assembled from the *same* `axum` (and therefore the same
//! `matchit`) the origin uses, from the same path patterns. Path matching
//! parity between origin and edge is therefore true by construction rather
//! than by a second implementation that has to be kept honest — percent
//! encoding, trailing slashes, `{param}` capture and `%2F` in a segment all
//! behave identically because it is literally the same matcher.

use axum::Router;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use http::{StatusCode, Uri};
use tower::ServiceExt as _;

use crate::route::{EdgeCapability, EdgeRoute, EdgeState};
use crate::wire::{FALLTHROUGH_SENTINEL, FallthroughReason};

/// Assemble the edge lane router.
///
/// The fallback answers an unmatched path with the fallthrough sentinel, so
/// "no such edge route" is reported through the same one channel every other
/// decline uses — the runtime does not need a second route-lookup pass, and a
/// caller driving this router directly (the conformance harness does) sees the
/// same answer the capsule would give.
///
/// # Panics
///
/// Panics if two routes register the same path — axum's `Router::route` is the
/// authority on route-shape conflicts and panics on a duplicate. This is the
/// same failure the origin router produces for the same route table, so a
/// conflict is caught natively long before a capsule is built.
pub fn build_edge_router(routes: Vec<EdgeRoute>) -> Router<()> {
    let mut lane: Router<EdgeState> = Router::new();
    for route in routes {
        lane = lane.route(route.path, route.handler);
    }
    lane.fallback(unknown_route).with_state(EdgeState)
}

/// The 404 fallback: decline with [`FallthroughReason::UnknownRoute`].
async fn unknown_route(uri: Uri) -> Response {
    (
        StatusCode::NOT_FOUND,
        [(
            FALLTHROUGH_SENTINEL,
            FallthroughReason::UnknownRoute.as_str(),
        )],
        format!("no edge route matches {}", uri.path()),
    )
        .into_response()
}

/// Answers "which capabilities does the route serving this URI declare?" using
/// the same matcher [`build_edge_router`] uses.
///
/// The capsule runtime consults this *before* dispatch so a route whose seam
/// the host cannot mediate never runs a single line of handler code — the
/// request falls through to the origin instead of half-executing.
///
/// It is a real (tiny) `axum` router rather than a hand-rolled path matcher on
/// purpose: a second matcher would be a second thing that can disagree with
/// the origin about what `/a/%2F/b` means.
pub struct CapabilityProbe {
    router: Router<()>,
}

impl CapabilityProbe {
    /// Build a probe for a route table.
    ///
    /// # Panics
    ///
    /// Same duplicate-path panic as [`build_edge_router`], on the same input.
    #[must_use]
    pub fn new(routes: &[EdgeRoute]) -> Self {
        let mut lane: Router<()> = Router::new();
        for route in routes {
            let needs = route.needs;
            lane = lane.route(route.path, get(move || async move { encode_needs(needs) }));
        }
        Self {
            router: lane.fallback(|| async { StatusCode::NOT_FOUND }),
        }
    }

    /// The capabilities declared by the route matching `uri`, or `None` when
    /// no edge route matches it at all.
    #[must_use]
    pub fn needs(&self, uri: &str) -> Option<Vec<EdgeCapability>> {
        let request = http::Request::builder()
            .method(http::Method::GET)
            .uri(uri)
            .body(axum::body::Body::empty())
            .ok()?;

        futures::executor::block_on(async {
            let response = self.router.clone().oneshot(request).await.ok()?;
            if response.status() != StatusCode::OK {
                return None;
            }
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .ok()?;
            let body = String::from_utf8(body.to_vec()).ok()?;
            Some(decode_needs(&body))
        })
    }
}

impl std::fmt::Debug for CapabilityProbe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CapabilityProbe").finish_non_exhaustive()
    }
}

fn encode_needs(needs: &[EdgeCapability]) -> String {
    needs
        .iter()
        .map(|capability| capability.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

fn decode_needs(encoded: &str) -> Vec<EdgeCapability> {
    encoded
        .split(',')
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse::<EdgeCapability>().ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::edge_get;

    async fn greet(axum::extract::Path(name): axum::extract::Path<String>) -> String {
        format!("hello {name}")
    }

    async fn stats() -> &'static str {
        "stats"
    }

    fn routes() -> Vec<EdgeRoute> {
        vec![
            EdgeRoute {
                method: http::Method::GET,
                path: "/greet/{name}",
                handler: edge_get(greet),
                name: "greet",
                needs: &[EdgeCapability::Kv],
            },
            EdgeRoute {
                method: http::Method::GET,
                path: "/stats",
                handler: edge_get(stats),
                name: "stats",
                needs: &[],
            },
        ]
    }

    fn call(router: &Router<()>, uri: &str) -> (StatusCode, Vec<(String, String)>, String) {
        let request = http::Request::builder()
            .uri(uri)
            .body(axum::body::Body::empty())
            .expect("request");
        futures::executor::block_on(async {
            let response = router
                .clone()
                .oneshot(request)
                .await
                .expect("dispatch is infallible");
            let status = response.status();
            let headers = response
                .headers()
                .iter()
                .map(|(name, value)| {
                    (
                        name.as_str().to_owned(),
                        String::from_utf8_lossy(value.as_bytes()).into_owned(),
                    )
                })
                .collect();
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("body");
            (status, headers, String::from_utf8_lossy(&body).into_owned())
        })
    }

    #[test]
    fn a_registered_route_is_served() {
        let (status, _, body) = call(&build_edge_router(routes()), "/greet/ada");
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "hello ada");
    }

    #[test]
    fn an_unmatched_path_answers_with_the_fallthrough_sentinel() {
        let (status, headers, body) = call(&build_edge_router(routes()), "/nope");
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(
            headers.contains(&(FALLTHROUGH_SENTINEL.to_owned(), "unknown_route".to_owned())),
            "{headers:?}"
        );
        assert!(body.contains("/nope"), "{body}");
    }

    #[test]
    fn the_probe_reports_declared_capabilities() {
        let probe = CapabilityProbe::new(&routes());
        assert_eq!(probe.needs("/greet/ada"), Some(vec![EdgeCapability::Kv]));
        assert_eq!(probe.needs("/stats"), Some(vec![]));
        assert_eq!(probe.needs("/stats?x=1"), Some(vec![]));
    }

    #[test]
    fn the_probe_reports_none_for_an_unknown_route() {
        let probe = CapabilityProbe::new(&routes());
        assert_eq!(probe.needs("/nope"), None);
        assert_eq!(probe.needs("/greet/ada/extra"), None);
    }

    #[test]
    fn needs_encoding_round_trips() {
        assert_eq!(encode_needs(&[]), "");
        assert_eq!(encode_needs(&[EdgeCapability::Kv]), "kv");
        assert_eq!(decode_needs(""), vec![]);
        assert_eq!(decode_needs("kv"), vec![EdgeCapability::Kv]);
    }
}
