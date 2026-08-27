//! Sending a mirrored request to the candidate build (issue #1653).
//!
//! Behind a trait, for two reasons. The obvious one is testability: the layer's
//! own tests drive a recording double instead of a socket. The load-bearing one
//! is that the shadow response must never become a `Response` the framework can
//! accidentally return — it is read into a [`ResponseFacts`] here and handed
//! straight to the differ, so there is no code path on which a candidate's bytes
//! could reach an end user.
//!
//! [`HttpShadowTransport`] also disables proxy autodetection and follows no
//! redirects — see [`HttpShadowTransport::new`].
//!
//! [`HttpShadowTransport`] deliberately does **not** use
//! [`crate::http_client::Client`]. That client carries a retry policy and a
//! process-global per-host circuit breaker, both of which are wrong here: a
//! retry would double the amplification a mirror already represents, and a
//! flaky candidate would trip a breaker shared with the application's own
//! outbound calls. Mirroring is strictly one attempt, one deadline, no
//! redirects.

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

use axum::http::{HeaderMap, HeaderName, HeaderValue, Method};

use crate::shadow::diff::ResponseFacts;
use crate::shadow::sample::{SHADOW_HEADER, SHADOW_HEADER_VALUE};

/// Headers never copied onto a mirrored request.
///
/// The hop-by-hop set (RFC 9110 §7.6.1) describes *this* connection, not the
/// message, so forwarding it is meaningless at best. `host` is re-derived from
/// the shadow target. `content-length`/`transfer-encoding` describe a body a
/// `GET`/`HEAD` mirror does not carry. `accept-encoding` is dropped so the
/// candidate answers uncompressed: the primary body is teed *inside* the
/// compression layer, so an encoded shadow body would diff against a plain
/// primary one and every route would look divergent.
const STRIPPED_HEADERS: [&str; 18] = [
    // Hop-by-hop (RFC 9110 §7.6.1) plus `proxy-connection`.
    "connection",
    "proxy-connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    // Re-derived from the shadow target, or describing a body a GET/HEAD
    // mirror does not carry.
    "host",
    "content-length",
    // Dropped so the candidate answers uncompressed — see the module note on
    // why an encoded shadow body would diff against a plain primary one.
    "accept-encoding",
    // The forwarding family. This layer runs OUTSIDE
    // `TrustedProxiesLayer`, so these still hold whatever the client put on
    // the wire: the primary is about to discard them (its peer is not a
    // trusted proxy), but the candidate would receive them from *this
    // process's* address, which a production-cloned `trusted_proxies` config
    // very likely does trust. Forwarding them would launder a header the
    // primary correctly rejects into one an internal build accepts — a
    // spoofed client IP past the candidate's own IP allowlists and per-IP
    // limits, or a poisoned absolute-URL host.
    "x-forwarded-for",
    "x-forwarded-host",
    "x-forwarded-proto",
    "x-forwarded-port",
    "forwarded",
    "x-real-ip",
];

/// A request to replay against the candidate build.
#[derive(Clone, Debug)]
pub struct ShadowRequest {
    /// Method — always `GET` or `HEAD`.
    pub method: Method,
    /// Absolute URL on the shadow target.
    pub url: String,
    /// Headers to send, already sanitized by [`forwarded_headers`].
    pub headers: HeaderMap,
}

/// Why a shadow request produced no comparable response.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ShadowError {
    /// The configured deadline elapsed first.
    #[error("shadow request timed out")]
    Timeout,
    /// The candidate's response body exceeded `shadow.max_body_bytes`.
    ///
    /// Reported by the transport rather than by the caller, because the caller
    /// can only measure a body that has *already been read into memory* — which
    /// is the very thing this bound exists to prevent. The read stops the
    /// moment the budget is passed and the rest of the body is never buffered.
    #[error("shadow response body exceeded the capture budget")]
    Oversize,
    /// Anything else: connection refused, DNS, TLS, a malformed response.
    #[error("shadow request failed: {0}")]
    Transport(String),
}

impl ShadowError {
    /// Stable metric label for this failure.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Oversize => "oversize",
            Self::Transport(_) => "error",
        }
    }
}

/// Boxed future a [`ShadowTransport`] returns.
pub type ShadowFuture =
    Pin<Box<dyn Future<Output = Result<ResponseFacts, ShadowError>> + Send + 'static>>;

/// How a mirrored request reaches the candidate build.
pub trait ShadowTransport: std::fmt::Debug + Send + Sync + 'static {
    /// Send `request` and read the response into comparable facts.
    ///
    /// Implementations must not retry: the caller already bounds this with a
    /// deadline, and a mirror that retries amplifies production traffic.
    fn send(&self, request: ShadowRequest) -> ShadowFuture;
}

/// Copy the headers that should travel with a mirrored request, and stamp the
/// loop guard.
///
/// **Credentials are forwarded on purpose.** A candidate build that cannot see
/// the session cookie or bearer token answers every authenticated route with a
/// redirect or a `401`, and the diff degenerates into noise. The consequence —
/// the shadow target receives live credentials and must be as trusted as the
/// primary — is stated in `docs/guide/staged-deploys.md`.
#[must_use]
pub fn forwarded_headers(source: &HeaderMap) -> HeaderMap {
    let connection_named = connection_named_headers(source);
    let mut out = HeaderMap::with_capacity(source.len().saturating_add(1));
    for (name, value) in source {
        let lower = name.as_str().to_ascii_lowercase();
        if STRIPPED_HEADERS.contains(&lower.as_str())
            || lower == SHADOW_HEADER
            || connection_named.iter().any(|named| named == &lower)
        {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    if let Ok(name) = HeaderName::from_bytes(SHADOW_HEADER.as_bytes()) {
        out.insert(name, HeaderValue::from_static(SHADOW_HEADER_VALUE));
    }
    out
}

/// The header names an inbound `Connection:` header declares hop-by-hop.
///
/// RFC 9110 lets a message nominate its own connection-scoped headers
/// (`Connection: X-Internal-Auth`). Those describe the hop the request arrived
/// on, so copying them onto a different hop is exactly the mistake the fixed
/// list above avoids for the well-known names.
fn connection_named_headers(source: &HeaderMap) -> Vec<String> {
    source
        .get_all(axum::http::header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|name| name.trim().to_ascii_lowercase())
        .filter(|name| !name.is_empty())
        .collect()
}

/// Join a shadow target base with a request target.
#[must_use]
pub fn shadow_url(target_base: &str, request_target: &str) -> String {
    let base = target_base.trim_end_matches('/');
    if request_target.starts_with('/') {
        format!("{base}{request_target}")
    } else {
        format!("{base}/{request_target}")
    }
}

/// The real transport: one `reqwest` request, one deadline, no retries, no
/// redirects, no circuit breaker.
#[cfg(feature = "http-client")]
#[derive(Debug, Clone)]
pub struct HttpShadowTransport {
    client: reqwest::Client,
    max_body_bytes: usize,
}

#[cfg(feature = "http-client")]
impl HttpShadowTransport {
    /// Build a transport whose requests are bounded by `timeout`.
    ///
    /// Redirects are not followed: a candidate that answers `302` where the
    /// live build answers `200` is exactly the divergence this feature exists
    /// to report, and following the hop would hide it.
    ///
    /// # Errors
    ///
    /// Returns the `reqwest` build error when the TLS backend cannot be
    /// initialised, so a misconfigured process reports it at startup rather
    /// than panicking on the request path.
    pub fn new(
        timeout: std::time::Duration,
        max_body_bytes: usize,
    ) -> Result<Self, reqwest::Error> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(timeout)
                // No ambient proxy. Mirrored requests carry the end user's
                // session cookie and `Authorization` header, and reqwest
                // honours `HTTP_PROXY`/`HTTPS_PROXY` by default with no
                // loopback bypass — so on a host that sets those (a corporate
                // egress proxy, a sidecar) every mirrored request would ship
                // live credentials to a third party the operator never chose
                // as a shadow target. The target is an address the operator
                // named; this client dials it directly or not at all.
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            max_body_bytes,
        })
    }
}

#[cfg(feature = "http-client")]
impl ShadowTransport for HttpShadowTransport {
    fn send(&self, request: ShadowRequest) -> ShadowFuture {
        let client = self.client.clone();
        let max_body_bytes = self.max_body_bytes;
        Box::pin(async move {
            let response = client
                .request(request.method, &request.url)
                .headers(request.headers)
                .send()
                .await
                .map_err(|error| {
                    if error.is_timeout() {
                        ShadowError::Timeout
                    } else {
                        ShadowError::Transport(error.to_string())
                    }
                })?;

            let status = response.status().as_u16();
            let content_type = response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .map(ToOwned::to_owned);
            // Streamed, not `.bytes()`: collecting the whole body first would
            // buffer an arbitrarily large candidate response into this
            // process's memory before anyone could check its size. A candidate
            // that streams a gigabyte must cost this replica the budget, not
            // the gigabyte.
            let mut collected = bytes::BytesMut::new();
            let mut response = response;
            loop {
                let chunk = response.chunk().await.map_err(|error| {
                    if error.is_timeout() {
                        ShadowError::Timeout
                    } else {
                        ShadowError::Transport(error.to_string())
                    }
                })?;
                let Some(chunk) = chunk else { break };
                if collected.len().saturating_add(chunk.len()) > max_body_bytes {
                    // Drop the response here: the connection is closed and the
                    // remaining bytes are never read.
                    return Err(ShadowError::Oversize);
                }
                collected.extend_from_slice(&chunk);
            }

            Ok(ResponseFacts::new(status, content_type, collected.freeze()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderName, HeaderValue};

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                HeaderName::from_bytes(name.as_bytes()).expect("valid header name"),
                HeaderValue::from_str(value).expect("valid header value"),
            );
        }
        map
    }

    #[test]
    fn the_loop_guard_header_is_always_added() {
        let forwarded = forwarded_headers(&HeaderMap::new());
        assert_eq!(
            forwarded
                .get(crate::shadow::sample::SHADOW_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some(crate::shadow::sample::SHADOW_HEADER_VALUE)
        );
    }

    #[test]
    fn hop_by_hop_headers_are_stripped() {
        let forwarded = forwarded_headers(&headers(&[
            ("connection", "keep-alive"),
            ("keep-alive", "timeout=5"),
            ("proxy-authorization", "secret"),
            ("te", "trailers"),
            ("trailer", "Expires"),
            ("transfer-encoding", "chunked"),
            ("upgrade", "websocket"),
        ]));
        for stripped in [
            "connection",
            "keep-alive",
            "proxy-authorization",
            "te",
            "trailer",
            "transfer-encoding",
            "upgrade",
        ] {
            assert!(
                !forwarded.contains_key(stripped),
                "{stripped} must not be forwarded"
            );
        }
    }

    #[test]
    fn host_and_framing_headers_are_stripped() {
        let forwarded = forwarded_headers(&headers(&[
            ("host", "app.example.com"),
            ("content-length", "0"),
        ]));
        assert!(!forwarded.contains_key("host"));
        assert!(!forwarded.contains_key("content-length"));
    }

    #[test]
    fn accept_encoding_is_stripped_so_both_sides_are_compared_uncompressed() {
        let forwarded = forwarded_headers(&headers(&[("accept-encoding", "gzip, br")]));
        assert!(!forwarded.contains_key("accept-encoding"));
    }

    #[test]
    fn the_forwarding_family_is_stripped() {
        // This layer runs outside TrustedProxiesLayer, so these still carry
        // whatever the client sent. Forwarding them would launder a spoofed
        // client IP or host into a candidate that trusts this process's
        // address as a proxy.
        let forwarded = forwarded_headers(&headers(&[
            ("x-forwarded-for", "10.0.0.1"),
            ("x-forwarded-host", "evil.example"),
            ("x-forwarded-proto", "https"),
            ("x-forwarded-port", "443"),
            ("forwarded", "for=10.0.0.1;host=evil.example"),
            ("x-real-ip", "10.0.0.1"),
        ]));
        for stripped in [
            "x-forwarded-for",
            "x-forwarded-host",
            "x-forwarded-proto",
            "x-forwarded-port",
            "forwarded",
            "x-real-ip",
        ] {
            assert!(
                !forwarded.contains_key(stripped),
                "{stripped} must not be forwarded to the candidate"
            );
        }
    }

    #[test]
    fn headers_named_by_connection_are_stripped() {
        let forwarded = forwarded_headers(&headers(&[
            ("connection", "X-Internal-Auth, Keep-Alive"),
            ("x-internal-auth", "hop-scoped"),
            ("proxy-connection", "keep-alive"),
            ("accept", "application/json"),
        ]));
        assert!(!forwarded.contains_key("x-internal-auth"));
        assert!(!forwarded.contains_key("proxy-connection"));
        assert!(
            forwarded.contains_key("accept"),
            "unrelated headers survive"
        );
    }

    #[test]
    fn an_inbound_loop_guard_is_replaced_not_duplicated() {
        let forwarded = forwarded_headers(&headers(&[(
            crate::shadow::sample::SHADOW_HEADER,
            "spoofed",
        )]));
        let values: Vec<_> = forwarded
            .get_all(crate::shadow::sample::SHADOW_HEADER)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect();
        assert_eq!(values, vec![crate::shadow::sample::SHADOW_HEADER_VALUE]);
    }

    #[test]
    fn application_headers_including_credentials_are_forwarded() {
        // Documented trust boundary: the candidate build must see the same
        // request the live build saw, or every authenticated route diverges.
        let forwarded = forwarded_headers(&headers(&[
            ("cookie", "session=abc"),
            ("authorization", "Bearer t"),
            ("accept", "application/json"),
            ("x-request-id", "r-1"),
        ]));
        for kept in ["cookie", "authorization", "accept", "x-request-id"] {
            assert!(forwarded.contains_key(kept), "{kept} must be forwarded");
        }
    }

    #[test]
    fn shadow_urls_join_without_doubling_slashes() {
        assert_eq!(
            shadow_url("http://127.0.0.1:9091", "/api/orders?page=2"),
            "http://127.0.0.1:9091/api/orders?page=2"
        );
        assert_eq!(
            shadow_url("http://127.0.0.1:9091", "api/orders"),
            "http://127.0.0.1:9091/api/orders"
        );
    }

    #[test]
    fn shadow_errors_have_stable_metric_labels() {
        assert_eq!(ShadowError::Timeout.as_str(), "timeout");
        assert_eq!(ShadowError::Oversize.as_str(), "oversize");
        assert_eq!(ShadowError::Transport(String::new()).as_str(), "error");
    }
}
