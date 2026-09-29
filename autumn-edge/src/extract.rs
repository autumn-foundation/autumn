//! The author-facing handle on the mediated KV seam.

use std::fmt;
use std::sync::Arc;

use axum::extract::FromRequestParts;
use axum::response::{IntoResponse, Response};
use http::StatusCode;
use http::request::Parts;

use crate::kv::EdgeKv;
use crate::wire::{FALLTHROUGH_SENTINEL, FallthroughReason};

/// The message the rejection renders. Also asserted by the native
/// integration test in `autumn-web`, so it lives in one place.
const NOT_CONFIGURED: &str = "EdgeKv not configured; call `.with_edge_kv(...)` on the AppBuilder \
     or provide the `kv` capability at the edge host";

/// A read handle on the host's non-authoritative KV replica.
///
/// This is the one extractor that makes AC-4 true: the *same* handler source
/// compiles for both substrates because `EdgeCache` arrives through a request
/// extension rather than through router state.
///
/// ```rust
/// use autumn_edge::prelude::*;
///
/// async fn note(Path(key): Path<String>, cache: EdgeCache) -> String {
///     cache
///         .get_string(&key)
///         .unwrap_or_else(|| "not cached here".to_owned())
/// }
/// ```
///
/// | Substrate | Who injects it |
/// | --- | --- |
/// | Origin | `AppBuilder::with_edge_kv(...)` (the `edge` feature of `autumn-web`) |
/// | Edge | the capsule runtime, when the host provides [`EdgeCapability::Kv`](crate::route::EdgeCapability::Kv) |
///
/// # When it is missing
///
/// Extraction fails with [`EdgeCacheUnavailable`], whose response carries the
/// fallthrough sentinel. At the edge that means the request transparently
/// falls through to the origin; at the origin it means a `500` with an
/// actionable message, because an unconfigured seam is a wiring bug the author
/// must see. The pre-dispatch capability check normally makes the edge case
/// unreachable — this is the belt to that suspenders.
#[derive(Clone)]
pub struct EdgeCache(pub(crate) Arc<dyn EdgeKv>);

impl EdgeCache {
    /// Wrap a store in a handle.
    #[must_use]
    pub fn new(kv: Arc<dyn EdgeKv>) -> Self {
        Self(kv)
    }

    /// Read `key`. `None` is a miss and always a legal answer.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<Vec<u8>> {
        self.0.get(key)
    }

    /// Read `key` as UTF-8. `None` on a miss *or* on invalid UTF-8 — the bytes
    /// are never lossily patched up, because a replacement character rendered
    /// at the edge and not at the origin would be a silent divergence.
    #[must_use]
    pub fn get_string(&self, key: &str) -> Option<String> {
        String::from_utf8(self.0.get(key)?).ok()
    }

    /// The extension value `autumn-web` installs app-wide from
    /// `AppBuilder::with_edge_kv`, and the capsule runtime installs per
    /// request.
    ///
    /// Handing out a ready-made [`axum::Extension`] keeps the injection type an
    /// implementation detail of this crate: callers never name it.
    pub fn layer(kv: Arc<dyn EdgeKv>) -> axum::Extension<Self> {
        axum::Extension(Self::new(kv))
    }
}

impl fmt::Debug for EdgeCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("EdgeCache(..)")
    }
}

/// Rejection returned when no [`EdgeKv`] is installed for the request.
///
/// The response carries the [`FALLTHROUGH_SENTINEL`] header so the capsule
/// runtime converts it into
/// [`FallthroughReason::MissingCapability`] instead of serving a broken page.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct EdgeCacheUnavailable;

impl fmt::Display for EdgeCacheUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(NOT_CONFIGURED)
    }
}

impl std::error::Error for EdgeCacheUnavailable {}

impl IntoResponse for EdgeCacheUnavailable {
    fn into_response(self) -> Response {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            [(
                FALLTHROUGH_SENTINEL,
                FallthroughReason::MissingCapability.as_str(),
            )],
            NOT_CONFIGURED,
        )
            .into_response()
    }
}

impl<S: Send + Sync> FromRequestParts<S> for EdgeCache {
    type Rejection = EdgeCacheUnavailable;

    fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> impl Future<Output = Result<Self, Self::Rejection>> {
        // Not `async fn`: the body only reads a request extension, so there is
        // nothing to await and clippy's `unused_async_trait_impl` rightly
        // objects to the generated state machine.
        std::future::ready(
            parts
                .extensions
                .get::<Self>()
                .cloned()
                .ok_or(EdgeCacheUnavailable),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kv::InMemoryEdgeKv;

    fn cache() -> EdgeCache {
        EdgeCache::new(Arc::new(
            InMemoryEdgeKv::new()
                .with("greeting", "hello")
                .with("binary", vec![0xff, 0xfe]),
        ))
    }

    #[test]
    fn get_and_get_string_agree_on_hits() {
        let cache = cache();
        assert_eq!(cache.get("greeting"), Some(b"hello".to_vec()));
        assert_eq!(cache.get_string("greeting"), Some("hello".to_owned()));
    }

    #[test]
    fn get_string_is_none_on_invalid_utf8_rather_than_lossy() {
        let cache = cache();
        assert_eq!(cache.get("binary"), Some(vec![0xff, 0xfe]));
        assert_eq!(cache.get_string("binary"), None);
    }

    #[test]
    fn miss_is_none() {
        assert_eq!(cache().get("absent"), None);
        assert_eq!(cache().get_string("absent"), None);
    }

    #[test]
    fn layer_builds_an_extension_the_extractor_finds() {
        let axum::Extension(handle) =
            EdgeCache::layer(Arc::new(InMemoryEdgeKv::new().with("k", "v")));
        assert_eq!(handle.get("k"), Some(b"v".to_vec()));
    }

    #[test]
    fn extraction_succeeds_when_the_extension_is_present() {
        let mut parts = http::Request::builder()
            .uri("/")
            .extension(cache())
            .body(())
            .expect("request")
            .into_parts()
            .0;

        let extracted = futures::executor::block_on(EdgeCache::from_request_parts(&mut parts, &()))
            .expect("extension present");
        assert_eq!(extracted.get_string("greeting"), Some("hello".to_owned()));
    }

    #[test]
    fn extraction_fails_with_the_sentinel_and_an_actionable_message() {
        let mut parts = http::Request::builder()
            .uri("/")
            .body(())
            .expect("request")
            .into_parts()
            .0;

        let rejection = futures::executor::block_on(EdgeCache::from_request_parts(&mut parts, &()))
            .map(|_| ())
            .expect_err("no extension installed");
        let response = rejection.into_response();

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            response
                .headers()
                .get(FALLTHROUGH_SENTINEL)
                .and_then(|value| value.to_str().ok()),
            Some("missing_capability")
        );

        let body =
            futures::executor::block_on(axum::body::to_bytes(response.into_body(), usize::MAX))
                .expect("body");
        let body = String::from_utf8_lossy(&body);
        assert!(body.contains("with_edge_kv"), "{body}");
        assert!(body.contains("kv"), "{body}");
    }
}
