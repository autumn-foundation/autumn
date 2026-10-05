//! Request ID middleware -- gives every request a [`RequestId`].
//!
//! Each request gets a [`RequestId`] that is:
//!
//! 1. Inserted into request extensions (accessible to handlers via
//!    `Extension<RequestId>`).
//! 2. Added as an `X-Request-Id` response header for correlation in
//!    logs and downstream services.
//!
//! The id is a new UUID v4, with one exception. When the peer is a trusted
//! proxy (see `[security.trusted_proxies]`) and it sends one well-formed
//! `X-Request-Id`, the layer keeps that id. Well-formed means a UUID in the
//! hyphenated form (36 characters) or the simple form (32 hex digits). The
//! layer keeps the text as sent. It ignores all other values, and it ignores
//! a request with more than one `X-Request-Id` header.
//!
//! The [`RequestIdLayer`] is applied automatically by the framework.
//! You do not need to register it manually.
//!
//! # Examples
//!
//! ```rust,no_run
//! use autumn_web::prelude::*;
//! use autumn_web::middleware::RequestId;
//! use axum::extract::Extension;
//!
//! #[get("/whoami")]
//! async fn whoami(Extension(req_id): Extension<RequestId>) -> String {
//!     format!("Your request ID is {req_id}")
//! }
//! ```

// autumn-determinism-gate: production code in this module must read time and
// mint identifiers through the framework's injected seams (ClockSource /
// Entropy), never `Instant::now()` / `Utc::now()` / `SystemTime::now()` /
// `Uuid::new_v4()` directly. See CONTRIBUTING.md "Determinism seam gate"
// (issue #1797). Justify exceptions with
// #[allow(clippy::disallowed_methods, reason = "…")] at the narrowest scope.
#![cfg_attr(not(test), deny(clippy::disallowed_methods))]
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

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use std::sync::Arc;

use axum::http::{HeaderValue, Request, Response};
use http::header::HeaderName;
use pin_project_lite::pin_project;
use tower::{Layer, Service};

use crate::entropy::{Entropy, OsEntropy};
use crate::security::ProxyResolver;
use uuid::Uuid;

/// Header name for the request ID, added to every response.
static X_REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");

/// A unique identifier assigned to each incoming HTTP request.
///
/// Wraps a [`Uuid`] v4 and is inserted into request extensions so handlers
/// can access it via `Extension<RequestId>`. It is also added to the
/// response as an `X-Request-Id` header for correlation in logs and
/// downstream services.
///
/// # Examples
///
/// ```rust,no_run
/// use autumn_web::prelude::*;
/// use autumn_web::middleware::RequestId;
/// use axum::extract::Extension;
///
/// #[get("/trace")]
/// async fn trace(Extension(req_id): Extension<RequestId>) -> String {
///     format!("request={}", req_id.as_uuid())
/// }
/// ```
#[derive(Clone, Debug)]
pub struct RequestId {
    uuid: Uuid,
    /// The inbound value, as the trusted proxy sent it. `None` for a new id.
    inbound: Option<HeaderValue>,
}

impl RequestId {
    /// Returns the underlying [`Uuid`] value.
    #[must_use]
    pub const fn as_uuid(&self) -> Uuid {
        self.uuid
    }

    /// Parse an inbound `X-Request-Id` value.
    ///
    /// Returns `None` unless `value` is a UUID in the hyphenated form
    /// (36 characters) or the simple form (32 hex digits). The id keeps
    /// `value` as its text, so logs match the logs of the sender.
    #[must_use]
    pub fn parse_inbound(value: &str) -> Option<Self> {
        if !matches!(value.len(), 32 | 36) {
            return None;
        }
        let uuid = Uuid::try_parse(value).ok()?;
        Some(Self {
            uuid,
            inbound: Some(HeaderValue::from_str(value).ok()?),
        })
    }

    /// Returns `true` when a trusted proxy supplied this id. An inbound id is
    /// not unique: a proxy retry sends the same id again.
    #[must_use]
    pub const fn is_inbound(&self) -> bool {
        self.inbound.is_some()
    }

    const fn minted(uuid: Uuid) -> Self {
        Self {
            uuid,
            inbound: None,
        }
    }

    fn header_value(&self) -> Option<HeaderValue> {
        if let Some(value) = &self.inbound {
            return Some(value.clone());
        }
        // Format the UUID into a stack buffer to avoid a String allocation.
        let mut buf = [0u8; uuid::fmt::Hyphenated::LENGTH];
        let s = self.uuid.as_hyphenated().encode_lower(&mut buf);
        HeaderValue::from_bytes(s.as_bytes()).ok()
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `parse_inbound` accepts only ASCII, so `to_str` cannot fail.
        match self.inbound.as_ref().and_then(|value| value.to_str().ok()) {
            Some(text) => f.write_str(text),
            None => write!(f, "{}", self.uuid),
        }
    }
}

/// Tower [`Layer`] that wraps a service with `RequestIdService`.
///
/// Applied automatically by [`AppBuilder::run`](crate::app::AppBuilder::run).
/// If you are building a custom Axum router, you can add it manually:
///
/// ```rust,no_run
/// use autumn_web::middleware::RequestIdLayer;
///
/// let app = axum::Router::<()>::new()
///     .route("/", axum::routing::get(|| async { "ok" }))
///     .layer(RequestIdLayer::default());
/// ```
#[derive(Clone, Debug)]
pub struct RequestIdLayer {
    /// Injected entropy source for minting request ids. Defaults to
    /// [`OsEntropy`]; the framework threads the app's seeded source here so
    /// request ids replay deterministically under a fixed simulation seed.
    entropy: Arc<dyn Entropy>,
    /// Decides if the peer may set the id. `None` ignores every inbound id.
    trust: Option<Arc<ProxyResolver>>,
}

impl Default for RequestIdLayer {
    fn default() -> Self {
        Self {
            entropy: Arc::new(OsEntropy),
            trust: None,
        }
    }
}

impl RequestIdLayer {
    /// Build a layer that mints request ids from the given entropy source.
    #[must_use]
    pub fn with_entropy(entropy: Arc<dyn Entropy>) -> Self {
        Self {
            entropy,
            trust: None,
        }
    }

    /// Keep a well-formed inbound `X-Request-Id` when `resolver` trusts the
    /// peer. Without this call, the layer ignores every inbound id.
    #[must_use]
    pub fn with_inbound_trust(mut self, resolver: Arc<ProxyResolver>) -> Self {
        self.trust = Some(resolver);
        self
    }
}

impl<S> Layer<S> for RequestIdLayer {
    type Service = RequestIdService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RequestIdService {
            inner,
            entropy: self.entropy.clone(),
            trust: self.trust.clone(),
        }
    }
}

/// Tower [`Service`] produced by [`RequestIdLayer`].
///
/// Gives each request a [`RequestId`], inserts it into request
/// extensions, and adds it as an `X-Request-Id` response header. You
/// do not construct this type directly -- it is created by
/// [`RequestIdLayer`].
#[derive(Clone, Debug)]
pub struct RequestIdService<S> {
    inner: S,
    entropy: Arc<dyn Entropy>,
    trust: Option<Arc<ProxyResolver>>,
}

impl<S> RequestIdService<S> {
    /// The trusted inbound id, if the peer is trusted and the id is valid.
    fn inbound_id<B>(&self, req: &Request<B>) -> Option<RequestId> {
        let trust = self.trust.as_ref()?;
        let mut values = req.headers().get_all(&X_REQUEST_ID).iter();
        let value = values.next()?.to_str().ok()?;
        // A proxy that appends, not replaces, leaves the client's value too.
        if values.next().is_some() || !trust.is_trusted_peer(req) {
            return None;
        }
        RequestId::parse_inbound(value)
    }
}

impl<S, ReqBody, ResBody> Service<Request<ReqBody>> for RequestIdService<S>
where
    S: Service<Request<ReqBody>, Response = Response<ResBody>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = RequestIdFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request<ReqBody>) -> Self::Future {
        let id = self
            .inbound_id(&req)
            .unwrap_or_else(|| RequestId::minted(self.entropy.uuid_v4()));
        req.extensions_mut().insert(id.clone());

        RequestIdFuture {
            inner: self.inner.call(req),
            request_id: Some(id),
        }
    }
}

pin_project! {
    /// Future that adds the `X-Request-Id` header to the response.
    pub struct RequestIdFuture<F> {
        #[pin]
        inner: F,
        request_id: Option<RequestId>,
    }
}

impl<F, ResBody, E> Future for RequestIdFuture<F>
where
    F: Future<Output = Result<Response<ResBody>, E>>,
{
    type Output = Result<Response<ResBody>, E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        match this.inner.poll(cx) {
            Poll::Ready(Ok(mut response)) => {
                if let Some(value) = this.request_id.take().and_then(|id| id.header_value()) {
                    response.headers_mut().insert(X_REQUEST_ID.clone(), value);
                }
                Poll::Ready(Ok(response))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::extract::Extension;
    use axum::routing::get;
    use tower::ServiceExt; // for oneshot

    #[tokio::test]
    async fn response_has_request_id_header() {
        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(RequestIdLayer::default());

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert!(response.headers().contains_key("x-request-id"));
        // Verify it's a valid UUID
        let id_str = response.headers()["x-request-id"].to_str().unwrap();
        assert!(Uuid::parse_str(id_str).is_ok());
    }

    #[tokio::test]
    async fn each_request_gets_unique_id() {
        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(RequestIdLayer::default());

        let r1 = app
            .clone()
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let r2 = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        let id1 = r1.headers()["x-request-id"].to_str().unwrap();
        let id2 = r2.headers()["x-request-id"].to_str().unwrap();
        assert_ne!(id1, id2);
    }

    #[tokio::test]
    async fn request_id_available_in_extensions() {
        async fn handler(Extension(id): Extension<RequestId>) -> String {
            id.to_string()
        }

        let app = Router::new()
            .route("/", get(handler))
            .layer(RequestIdLayer::default());

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), axum::http::StatusCode::OK);

        // The response body should contain a valid UUID
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_str = String::from_utf8(body.to_vec()).unwrap();
        assert!(Uuid::parse_str(&body_str).is_ok());
    }

    #[test]
    fn request_id_display() {
        let id = RequestId::minted(Uuid::nil());
        assert_eq!(id.to_string(), "00000000-0000-0000-0000-000000000000");
    }

    #[test]
    fn parse_inbound_accepts_only_hyphenated_and_simple_uuids() {
        let hyphenated = "0F8FAD5B-D9CB-469F-A165-70867728950E";
        let id = RequestId::parse_inbound(hyphenated).unwrap();
        assert_eq!(id.to_string(), hyphenated, "text is kept as sent");
        assert_eq!(id.as_uuid(), Uuid::parse_str(hyphenated).unwrap());

        let simple = "0f8fad5bd9cb469fa16570867728950e";
        assert_eq!(
            RequestId::parse_inbound(simple).unwrap().to_string(),
            simple
        );

        for bad in [
            "",
            "not-a-uuid",
            "0f8fad5b-d9cb-469f-a165-70867728950",
            "{0f8fad5b-d9cb-469f-a165-70867728950e}",
            "urn:uuid:0f8fad5b-d9cb-469f-a165-70867728950e",
            "0f8fad5b-d9cb-469f-a165-70867728950g",
        ] {
            assert!(RequestId::parse_inbound(bad).is_none(), "{bad}");
        }
    }

    fn trusting_layer(config: &crate::security::config::TrustedProxiesConfig) -> RequestIdLayer {
        RequestIdLayer::default().with_inbound_trust(Arc::new(ProxyResolver::from_config(config)))
    }

    async fn response_id(layer: RequestIdLayer, peer: Option<&str>, inbound: &str) -> String {
        response_id_with(layer, peer, &[("x-request-id", inbound)]).await
    }

    async fn response_id_with(
        layer: RequestIdLayer,
        peer: Option<&str>,
        headers: &[(&str, &str)],
    ) -> String {
        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(layer);
        let mut builder = Request::builder().uri("/");
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let mut request = builder.body(Body::empty()).unwrap();
        if let Some(peer) = peer {
            let addr: std::net::SocketAddr = peer.parse().unwrap();
            request
                .extensions_mut()
                .insert(axum::extract::ConnectInfo(addr));
        }
        let response = app.oneshot(request).await.unwrap();
        response.headers()["x-request-id"]
            .to_str()
            .unwrap()
            .to_owned()
    }

    #[tokio::test]
    async fn inbound_id_needs_a_trusted_peer() {
        let inbound = "0f8fad5b-d9cb-469f-a165-70867728950e";
        let ranges = crate::security::config::TrustedProxiesConfig {
            ranges: vec!["10.0.0.0/8".to_owned()],
            trusted_hops: None,
            trust_forwarded_headers: true,
        };

        // No trust configured on the layer.
        let id = response_id(RequestIdLayer::default(), None, inbound).await;
        assert_ne!(id, inbound);

        // Forwarded headers not trusted.
        let off = crate::security::config::TrustedProxiesConfig {
            ranges: Vec::new(),
            trusted_hops: None,
            trust_forwarded_headers: false,
        };
        let id = response_id(trusting_layer(&off), Some("10.1.2.3:4000"), inbound).await;
        assert_ne!(id, inbound);

        // Peer outside the trusted range.
        let id = response_id(trusting_layer(&ranges), Some("203.0.113.9:4000"), inbound).await;
        assert_ne!(id, inbound);

        // Peer inside the trusted range.
        let id = response_id(trusting_layer(&ranges), Some("10.1.2.3:4000"), inbound).await;
        assert_eq!(id, inbound);

        // No ranges and no hops: every peer is trusted (documented).
        let open = crate::security::config::TrustedProxiesConfig {
            ranges: Vec::new(),
            trusted_hops: None,
            trust_forwarded_headers: true,
        };
        let id = response_id(trusting_layer(&open), Some("203.0.113.9:4000"), inbound).await;
        assert_eq!(id, inbound);

        // Ranges that all fail to parse: no peer is trusted.
        let broken = crate::security::config::TrustedProxiesConfig {
            ranges: vec!["not-a-range".to_owned()],
            trusted_hops: None,
            trust_forwarded_headers: true,
        };
        let id = response_id(trusting_layer(&broken), Some("10.1.2.3:4000"), inbound).await;
        assert_ne!(id, inbound);
    }

    #[tokio::test]
    async fn hop_mode_needs_a_long_enough_forwarded_chain() {
        let inbound = "0f8fad5b-d9cb-469f-a165-70867728950e";
        let hops = crate::security::config::TrustedProxiesConfig {
            ranges: Vec::new(),
            trusted_hops: Some(1),
            trust_forwarded_headers: true,
        };
        let peer = Some("203.0.113.9:4000");

        // A direct client sends no X-Forwarded-For.
        let id = response_id(trusting_layer(&hops), peer, inbound).await;
        assert_ne!(id, inbound);

        // The proxy appended the client: the chain has two entries.
        let headers = [
            ("x-request-id", inbound),
            ("x-forwarded-for", "198.51.100.7, 10.0.0.2"),
        ];
        let id = response_id_with(trusting_layer(&hops), peer, &headers).await;
        assert_eq!(id, inbound);
    }

    #[tokio::test]
    async fn repeated_inbound_header_is_ignored() {
        let open = crate::security::config::TrustedProxiesConfig {
            ranges: Vec::new(),
            trusted_hops: None,
            trust_forwarded_headers: true,
        };
        let first = "0f8fad5b-d9cb-469f-a165-70867728950e";
        let headers = [
            ("x-request-id", first),
            ("x-request-id", "1f8fad5b-d9cb-469f-a165-70867728950e"),
        ];
        let id = response_id_with(trusting_layer(&open), None, &headers).await;
        assert_ne!(id, first);
        assert!(Uuid::parse_str(&id).is_ok());
    }

    #[test]
    fn inbound_flag_marks_only_proxy_ids() {
        assert!(!RequestId::minted(Uuid::nil()).is_inbound());
        let inbound = RequestId::parse_inbound("0f8fad5bd9cb469fa16570867728950e").unwrap();
        assert!(inbound.is_inbound());
    }
}
