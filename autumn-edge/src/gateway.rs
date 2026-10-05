//! The reference gateway: the capsule in front of the origin (AC-3).
//!
//! [`EdgeGateway`] does what a CDN shim does. It offers each request to the
//! capsule. When the capsule serves it, the gateway returns those bytes. When
//! the capsule declines, the gateway sends the *original* request to the
//! origin and returns the origin's answer. The app needs no extra code for
//! this.
//!
//! The origin is any `tower::Service`, for example the app's `axum::Router`.
//! The lane that answered is in the response extensions as [`Lane`], not in a
//! header, so the bytes on the wire stay identical to the origin's.
//!
//! # Rules
//!
//! - **The capsule never gets a body.** An edge handler cannot read one (no
//!   body extractor is on the `EdgeHandler` list). The gateway does not buffer
//!   the body; it keeps it for the origin.
//! - **Credentials go to the origin only.** [`EdgeArtifact::run`] strips
//!   [`SENSITIVE_HEADERS`] from the capsule's copy. The forwarded request
//!   keeps them.
//! - **Writes skip the capsule.** A method other than `GET`/`HEAD` goes
//!   directly to the origin, with the same reason the capsule would give.
//! - **A request the wire cannot carry skips the capsule.** A header value
//!   that is not UTF-8 goes to the origin as [`Lane::OriginOnly`].
//! - **The gateway does not trust the capsule.** A status outside 200-599, an
//!   invalid header, `set-cookie`, [`FALLTHROUGH_SENTINEL`], a hop-by-hop
//!   header, a body on 204/205/304, or a `content-length` that does not match
//!   the body becomes a `capsule_error` fallthrough.
//! - **No identity.** The gateway attaches no `EdgeIdentity`. A
//!   `needs(identity)` route falls through with `missing_capability`.
//! - **Fallthrough detail stays in the gateway.** It never reaches the client.
//!
//! The capsule runs on the calling task, before [`EdgeGateway::handle`]
//! returns. Thus a `tower::timeout` layer around the gateway cannot stop it;
//! only the fuel budget does. This is a reference host. A production shim
//! runs the capsule off the request thread and sets a time limit.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::body::Body;
use http::{HeaderName, HeaderValue, Request, Response, StatusCode};
use tower::{Service, ServiceExt};

use crate::host::EdgeArtifact;
use crate::kv::{EdgeKv, EmptyEdgeKv};
use crate::route::EdgeCapability;
use crate::wire::{
    EdgeOutcome, EdgeRequest, EdgeResponse, FALLTHROUGH_SENTINEL, FallthroughReason,
    SENSITIVE_HEADERS,
};

/// The lane that answered a request. Stored in the response extensions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Lane {
    /// The capsule served the response.
    Edge,
    /// The capsule declined, for this reason. The origin served the response.
    Fallthrough(FallthroughReason),
    /// The request cannot cross the wire. The origin served it and the
    /// capsule was not asked.
    OriginOnly,
}

/// A capsule in front of an origin service.
pub struct EdgeGateway<O> {
    artifact: Arc<EdgeArtifact>,
    kv: Arc<dyn EdgeKv>,
    capabilities: Vec<EdgeCapability>,
    response_headers: Vec<(HeaderName, HeaderValue)>,
    origin: O,
}

impl<O: Clone> Clone for EdgeGateway<O> {
    fn clone(&self) -> Self {
        Self {
            artifact: Arc::clone(&self.artifact),
            kv: Arc::clone(&self.kv),
            capabilities: self.capabilities.clone(),
            response_headers: self.response_headers.clone(),
            origin: self.origin.clone(),
        }
    }
}

impl<O> std::fmt::Debug for EdgeGateway<O> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EdgeGateway")
            .field("capabilities", &self.capabilities)
            .finish_non_exhaustive()
    }
}

impl<O> EdgeGateway<O>
where
    O: Service<Request<Body>, Response = Response<Body>, Error = Infallible>
        + Clone
        + Send
        + 'static,
    O::Future: Send,
{
    /// A gateway with no capabilities: a route that needs `kv` falls through.
    pub fn new(artifact: Arc<EdgeArtifact>, origin: O) -> Self {
        Self {
            artifact,
            kv: Arc::new(EmptyEdgeKv),
            capabilities: Vec::new(),
            response_headers: Vec::new(),
            origin,
        }
    }

    /// Provide the `kv` capability from this store.
    #[must_use]
    pub fn with_kv(mut self, kv: Arc<dyn EdgeKv>) -> Self {
        self.kv = kv;
        if !self.capabilities.contains(&EdgeCapability::Kv) {
            self.capabilities.push(EdgeCapability::Kv);
        }
        self
    }

    /// Set these headers on every edge response, as the origin's security
    /// middleware does. They replace a header of the same name from the
    /// capsule. A name given more than once keeps every value.
    ///
    /// Give the static headers the origin sends: `x-frame-options`,
    /// `x-content-type-options`, and `content-security-policy` when the CSP
    /// nonce is off. Then the client gets the same headers from both lanes.
    #[must_use]
    pub fn with_response_headers(
        mut self,
        headers: impl IntoIterator<Item = (HeaderName, HeaderValue)>,
    ) -> Self {
        self.response_headers.extend(headers);
        self
    }

    /// Serve one request: from the capsule if it can, else from the origin.
    ///
    /// The capsule runs before this returns. The future only waits for the
    /// origin, and it does not borrow the gateway.
    pub fn handle(
        &self,
        request: Request<Body>,
    ) -> impl Future<Output = Response<Body>> + Send + 'static {
        let lane = if is_edge_method(request.method().as_str()) {
            edge_request(&request).map_or(Err(Lane::OriginOnly), |edge| {
                self.ask_capsule(&edge).map_err(Lane::Fallthrough)
            })
        } else {
            Err(Lane::Fallthrough(FallthroughReason::MethodNotEdgeEligible))
        };
        let origin = self.origin.clone();
        async move {
            match lane {
                Ok(response) => with_lane(response, Lane::Edge),
                Err(lane) => {
                    let response = origin
                        .oneshot(request)
                        .await
                        .unwrap_or_else(|never| match never {});
                    with_lane(response, lane)
                }
            }
        }
    }

    /// Run the capsule. `Err` is the reason to fall through.
    fn ask_capsule(&self, request: &EdgeRequest) -> Result<Response<Body>, FallthroughReason> {
        let head = request.method == "HEAD";
        match self
            .artifact
            .run(request, &self.capabilities, self.kv.as_ref())
        {
            Ok(EdgeOutcome::Served(response)) => {
                let mut response =
                    into_http(response, head).ok_or(FallthroughReason::CapsuleError)?;
                let headers = response.headers_mut();
                for (name, _) in &self.response_headers {
                    headers.remove(name);
                }
                for (name, value) in &self.response_headers {
                    headers.append(name.clone(), value.clone());
                }
                Ok(response)
            }
            Ok(EdgeOutcome::Fallthrough { reason, .. }) => Err(reason),
            Err(_) => Err(FallthroughReason::CapsuleError),
        }
    }
}

impl<O> Service<Request<Body>> for EdgeGateway<O>
where
    O: Service<Request<Body>, Response = Response<Body>, Error = Infallible>
        + Clone
        + Send
        + 'static,
    O::Future: Send,
{
    type Response = Response<Body>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Infallible>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: Request<Body>) -> Self::Future {
        let response = self.handle(request);
        Box::pin(async move { Ok(response.await) })
    }
}

/// The same rule as the capsule's own method check.
fn is_edge_method(method: &str) -> bool {
    method == "GET" || method == "HEAD"
}

/// The capsule's view of `request`: no body and no credentials. `None` when a
/// header value is not UTF-8 and so cannot cross the wire.
fn edge_request<B>(request: &Request<B>) -> Option<EdgeRequest> {
    let mut headers = Vec::with_capacity(request.headers().len());
    for (name, value) in request.headers() {
        if SENSITIVE_HEADERS.contains(&name.as_str()) {
            continue;
        }
        let value = std::str::from_utf8(value.as_bytes()).ok()?;
        headers.push((name.as_str().to_owned(), value.to_owned()));
    }
    let uri = request
        .uri()
        .path_and_query()
        .map_or("/", http::uri::PathAndQuery::as_str);
    Some(EdgeRequest {
        method: request.method().as_str().to_owned(),
        uri: uri.to_owned(),
        headers,
        body: Vec::new(),
        identity: None,
    })
}

/// Headers that describe one connection or hop, not the response (RFC 9110
/// section 7.6.1, plus the legacy `keep-alive` and `proxy-connection`). A
/// capsule must not set them.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Statuses that have no body in HTTP.
const NO_BODY_STATUSES: &[u16] = &[204, 205, 304];

/// The capsule's answer as an HTTP response. `None` when the edge runtime
/// would refuse it, or when it would break on the wire: a status outside
/// 200-599, an invalid header, `set-cookie`, the fallthrough sentinel, a
/// hop-by-hop header, a body on 204/205/304, or a `content-length` that does
/// not match the body. A
/// `HEAD` answer has no body and keeps the length of the `GET` body. A
/// capsule that Autumn did not build can send any of these, so the gateway
/// checks again.
fn into_http(response: EdgeResponse, head: bool) -> Option<Response<Body>> {
    let no_body = head || NO_BODY_STATUSES.contains(&response.status);
    if !(200..=599).contains(&response.status) || (no_body && !response.body.is_empty()) {
        return None;
    }
    let body_len = response.body.len();
    let mut http = Response::new(Body::from(response.body));
    *http.status_mut() = StatusCode::from_u16(response.status).ok()?;
    for (name, value) in response.headers {
        let name = HeaderName::from_bytes(name.as_bytes()).ok()?;
        if name == http::header::SET_COOKIE
            || name.as_str() == FALLTHROUGH_SENTINEL
            || HOP_BY_HOP.contains(&name.as_str())
        {
            return None;
        }
        if name == http::header::CONTENT_LENGTH {
            let declared: usize = value.trim().parse().ok()?;
            if !content_length_fits(response.status, head, declared, body_len) {
                return None;
            }
        }
        http.headers_mut()
            .append(name, HeaderValue::from_str(&value).ok()?);
    }
    Some(http)
}

/// Whether a declared `content-length` is valid for this answer (RFC 9110
/// section 8.6). A `HEAD` or 304 answer may give the length of the 200
/// representation. A 204 must not send the field. A 205 may only send 0.
/// Any other answer must give the length of its body.
const fn content_length_fits(status: u16, head: bool, declared: usize, body_len: usize) -> bool {
    match status {
        204 => false,
        205 => declared == 0,
        304 => true,
        _ => head || declared == body_len,
    }
}

fn with_lane(mut response: Response<Body>, lane: Lane) -> Response<Body> {
    response.extensions_mut().insert(lane);
    response
}
