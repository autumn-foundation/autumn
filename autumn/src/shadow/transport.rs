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
/// `GET`/`HEAD` mirror does not carry.
///
/// `accept-encoding` is deliberately **not** in this list. The primary body is
/// teed *inside* the compression layer, so it is the handler's plain bytes —
/// but stripping the request header is not the way to make the candidate match:
/// a handler or user layer may vary its body on `Accept-Encoding` (serving a
/// precompressed representation), and then the two stacks would be answering
/// different logical requests. The header travels, and the candidate's response
/// is decoded on arrival instead — see `decode_body`.
const STRIPPED_HEADERS: [&str; 16] = [
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
    // Describes a body a GET/HEAD mirror does not carry.
    "content-length",
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
///
/// **So is `Host`.** The candidate is dialed at the operator's target address
/// (`127.0.0.1:9091`, say), but the request's *logical* authority is the one
/// the live build accepted. Letting the client re-derive `Host` from the dial
/// address breaks any route whose behaviour depends on it: a candidate that
/// clones production's `[security.trusted_hosts]` rejects every mirror with a
/// `400`, and a subdomain-keyed multi-tenant app resolves the wrong tenant —
/// either way manufacturing a divergence on every single request. The dial
/// address and the authority are separate things, and only the address comes
/// from the target.
///
/// `resolved` is what the trusted-proxy layer accepted, when it ran, and it is
/// re-stamped over the stripped forwarding family: the candidate is told the
/// *validated* host, client address, and scheme rather than the client's own
/// claims. Behind a proxy the raw `Host` is the internal address while the
/// public one arrives in `X-Forwarded-Host` — forwarding that header would let
/// a client forge it, and dropping it silently would have the candidate resolve
/// the wrong tenant or reject the request. Preferring the resolved value
/// mirrors `tenancy::extract_tenant_from_parts`, which makes the same choice
/// for the same reason.
#[must_use]
pub fn forwarded_headers(
    source: &HeaderMap,
    resolved: Option<&crate::security::ResolvedClientIdentity>,
) -> HeaderMap {
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
    // Re-stamp the forwarding family from the VALIDATED identity — never from
    // the client's raw claims, which were stripped above. Dropping them
    // entirely left the candidate resolving this process as the client: its
    // per-IP rate limiter would bucket every mirror together (and answer `429`,
    // a divergence), and `ClientScheme` would fall back to `http`, diverging on
    // any route that behaves differently under TLS. A candidate that does not
    // trust this process as a proxy simply ignores these, which is exactly
    // where it was before.
    if let Some(identity) = resolved {
        if let Some(host) = identity.host.as_deref()
            && let Ok(value) = HeaderValue::from_str(host)
        {
            out.insert(axum::http::header::HOST, value);
        }
        if let Some(addr) = identity.addr
            && let Ok(value) = HeaderValue::from_str(&addr.to_string())
        {
            out.insert("x-forwarded-for", value);
        }
        if let Some(scheme) = identity.scheme.as_deref()
            && let Ok(value) = HeaderValue::from_str(scheme)
        {
            out.insert("x-forwarded-proto", value);
        }
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

/// Decode a candidate response body according to its `Content-Encoding`.
///
/// The mirrored request carries the client's own `Accept-Encoding` (see
/// [`STRIPPED_HEADERS`]), so the candidate may legitimately answer encoded
/// while the teed primary body is plain. Comparing those directly would report
/// every compressible route as divergent.
///
/// The output is capped at `max_body_bytes` just as the wire read is: a
/// decoder turns a small body into an arbitrarily large one, so bounding only
/// the compressed bytes would leave a decompression bomb able to grow this
/// process. An unknown encoding is passed through untouched — better a
/// byte-comparison the operator can see than a guess.
pub(crate) fn decode_body(
    content_encoding: Option<&str>,
    body: bytes::Bytes,
    max_body_bytes: usize,
) -> Result<bytes::Bytes, ShadowError> {
    // Nothing to decode. A `HEAD` response carries no body while keeping its
    // representation headers, so it arrives here as zero bytes still declaring
    // `Content-Encoding: gzip` — and a decoder handed zero bytes fails with an
    // unexpected EOF. Reporting that as a transport error would have two
    // identical builds counted as errors and never compared at all, on every
    // precompressed route that answers `HEAD`.
    if body.is_empty() {
        return Ok(body);
    }

    let Some(encoding) = content_encoding else {
        return Ok(body);
    };

    // `Content-Encoding` is a LIST: `gzip, br` means brotli was applied to the
    // gzip output, so it unwinds in reverse. Matching the header as a single
    // string left a stacked value unrecognised and passed encoded bytes through
    // to be compared against a plain body — a divergence produced by a header,
    // which the contract says is not compared. `identity` contributes nothing
    // and drops out of the chain.
    let codings: Vec<&str> = encoding
        .split(',')
        .map(str::trim)
        .filter(|coding| !coding.is_empty() && *coding != "identity")
        .collect();
    if codings.is_empty() {
        return Ok(body);
    }
    // An unrecognised coding anywhere makes the whole chain unsafe to unwind:
    // decoding the layers around it would produce bytes that were never a
    // representation of anything. Pass the body through untouched — exactly
    // what a single unknown coding does — and let the byte comparison show it.
    if !codings.iter().all(|coding| is_known_coding(coding)) {
        return Ok(body);
    }

    let mut current = body;
    for coding in codings.iter().rev() {
        current = decode_one(coding, &current, max_body_bytes)?;
    }
    Ok(current)
}

/// Whether this content coding can be unwound.
fn is_known_coding(coding: &str) -> bool {
    matches!(coding, "gzip" | "x-gzip" | "deflate" | "br")
}

/// Unwind one content coding, bounding the output at `max_body_bytes`.
fn decode_one(
    coding: &str,
    body: &bytes::Bytes,
    max_body_bytes: usize,
) -> Result<bytes::Bytes, ShadowError> {
    use std::io::Read as _;

    // One byte past the budget, so an over-budget body is detectable rather
    // than silently truncated into a false divergence.
    let limit = u64::try_from(max_body_bytes)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let mut decoded = Vec::new();
    let read = match coding {
        "gzip" | "x-gzip" => flate2::read::GzDecoder::new(body.as_ref())
            .take(limit)
            .read_to_end(&mut decoded),
        "deflate" => flate2::read::ZlibDecoder::new(body.as_ref())
            .take(limit)
            .read_to_end(&mut decoded),
        "br" => brotli::Decompressor::new(body.as_ref(), 4096)
            .take(limit)
            .read_to_end(&mut decoded),
        _ => return Ok(body.clone()),
    };
    read.map_err(|error| {
        ShadowError::Transport(format!("could not decode a {coding} body: {error}"))
    })?;
    if decoded.len() > max_body_bytes {
        return Err(ShadowError::Oversize);
    }
    Ok(bytes::Bytes::from(decoded))
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
            let content_encoding = response
                .headers()
                .get(axum::http::header::CONTENT_ENCODING)
                .and_then(|value| value.to_str().ok())
                .map(|value| value.trim().to_ascii_lowercase());
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

            // The encoding travels WITH the facts rather than being consumed
            // here. Decoding only this side would report identical builds as
            // divergent whenever a handler serves a precompressed
            // representation, because the primary tee captures those encoded
            // bytes too. Both sides are decoded together in the mirror task.
            Ok(ResponseFacts::encoded(
                status,
                content_type,
                content_encoding,
                collected.freeze(),
            ))
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
        let forwarded = forwarded_headers(&HeaderMap::new(), None);
        assert_eq!(
            forwarded
                .get(crate::shadow::sample::SHADOW_HEADER)
                .and_then(|v| v.to_str().ok()),
            Some(crate::shadow::sample::SHADOW_HEADER_VALUE)
        );
    }

    #[test]
    fn hop_by_hop_headers_are_stripped() {
        let forwarded = forwarded_headers(
            &headers(&[
                ("connection", "keep-alive"),
                ("keep-alive", "timeout=5"),
                ("proxy-authorization", "secret"),
                ("te", "trailers"),
                ("trailer", "Expires"),
                ("transfer-encoding", "chunked"),
                ("upgrade", "websocket"),
            ]),
            None,
        );
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
    fn framing_headers_are_stripped() {
        let forwarded = forwarded_headers(&headers(&[("content-length", "0")]), None);
        assert!(!forwarded.contains_key("content-length"));
    }

    #[test]
    fn the_accepted_host_is_preserved() {
        // The candidate is dialed at the operator's target address, but the
        // request's logical authority is the one the live build accepted.
        // Re-deriving it from the dial address would make a candidate that
        // clones production's trusted-host policy reject every mirror with a
        // 400, and a subdomain-keyed tenant app resolve the wrong tenant.
        let forwarded = forwarded_headers(&headers(&[("host", "app.example.com")]), None);
        assert_eq!(
            forwarded.get("host").and_then(|v| v.to_str().ok()),
            Some("app.example.com")
        );
    }

    #[test]
    fn accept_encoding_travels_so_both_stacks_answer_the_same_request() {
        // A handler or user layer may vary its body on this header; stripping it
        // would have the two stacks answering different logical requests. The
        // candidate's response is decoded on arrival instead.
        let forwarded = forwarded_headers(&headers(&[("accept-encoding", "gzip, br")]), None);
        assert_eq!(
            forwarded
                .get("accept-encoding")
                .and_then(|v| v.to_str().ok()),
            Some("gzip, br")
        );
    }

    #[test]
    fn an_encoded_candidate_body_is_decoded_before_comparison() {
        use std::io::Write as _;
        let plain = b"{\"ok\":true}";
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(plain).expect("encode");
        let gzipped = bytes::Bytes::from(encoder.finish().expect("finish"));
        assert_ne!(
            gzipped.as_ref(),
            plain,
            "the fixture must really be encoded"
        );

        let decoded = decode_body(Some("gzip"), gzipped, 4096).expect("decode");
        assert_eq!(decoded.as_ref(), plain);
    }

    #[test]
    fn a_stacked_content_encoding_is_unwound_in_reverse() {
        // `gzip, br` means brotli was applied to the gzip output, so it unwinds
        // br-then-gzip. Treating the header as one opaque string left it
        // unrecognised and passed encoded bytes through to be compared against
        // a plain body.
        use std::io::Write as _;
        let plain = br#"{"ok":true}"#;

        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(plain).expect("gzip");
        let gzipped = gz.finish().expect("finish");

        let mut brotlied = Vec::new();
        {
            let mut writer = brotli::CompressorWriter::new(&mut brotlied, 4096, 5, 22);
            writer.write_all(&gzipped).expect("br");
        }

        let decoded = decode_body(Some("gzip, br"), bytes::Bytes::from(brotlied), 4096)
            .expect("decode chain");
        assert_eq!(decoded.as_ref(), plain);
    }

    #[test]
    fn identity_inside_a_chain_is_ignored() {
        use std::io::Write as _;
        let plain = b"hello";
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(plain).expect("gzip");
        let gzipped = bytes::Bytes::from(gz.finish().expect("finish"));

        let decoded = decode_body(Some("identity, gzip"), gzipped, 4096).expect("decode");
        assert_eq!(decoded.as_ref(), plain);
    }

    #[test]
    fn an_unknown_coding_anywhere_leaves_the_whole_chain_alone() {
        // Unwinding the layers around an unknown coding would produce bytes
        // that were never a representation of anything.
        let body = bytes::Bytes::from_static(b"opaque");
        for encoding in ["exotic-v2", "gzip, exotic-v2", "exotic-v2, gzip"] {
            assert_eq!(
                decode_body(Some(encoding), body.clone(), 4096).expect("pass through"),
                body,
                "{encoding}"
            );
        }
    }

    #[test]
    fn an_empty_body_needs_no_decoding_whatever_it_declares() {
        // A `HEAD` keeps its representation headers and sends no body, so this
        // is the shape that reaches the decoder for a precompressed route.
        for encoding in [
            Some("gzip"),
            Some("br"),
            Some("deflate"),
            Some("identity"),
            None,
        ] {
            assert_eq!(
                decode_body(encoding, bytes::Bytes::new(), 4096),
                Ok(bytes::Bytes::new()),
                "{encoding:?}"
            );
        }
    }

    #[test]
    fn an_unencoded_or_unknown_body_passes_through() {
        let body = bytes::Bytes::from_static(b"plain");
        for encoding in [None, Some("identity"), Some("exotic-v2")] {
            let out = decode_body(encoding, body.clone(), 4096).expect("pass through");
            assert_eq!(out, body, "{encoding:?}");
        }
    }

    #[test]
    fn a_decompression_bomb_is_refused_by_the_output_budget() {
        // The wire read is capped, but a decoder turns a small body into an
        // arbitrarily large one — so the OUTPUT is capped too.
        use std::io::Write as _;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        encoder.write_all(&vec![b'a'; 512 * 1024]).expect("encode");
        let bomb = bytes::Bytes::from(encoder.finish().expect("finish"));
        assert!(bomb.len() < 4096, "the fixture must be small on the wire");

        assert_eq!(
            decode_body(Some("gzip"), bomb, 4096),
            Err(ShadowError::Oversize)
        );
    }

    #[test]
    fn the_forwarding_family_is_stripped() {
        // This layer runs outside TrustedProxiesLayer, so these still carry
        // whatever the client sent. Forwarding them would launder a spoofed
        // client IP or host into a candidate that trusts this process's
        // address as a proxy.
        let forwarded = forwarded_headers(
            &headers(&[
                ("x-forwarded-for", "10.0.0.1"),
                ("x-forwarded-host", "evil.example"),
                ("x-forwarded-proto", "https"),
                ("x-forwarded-port", "443"),
                ("forwarded", "for=10.0.0.1;host=evil.example"),
                ("x-real-ip", "10.0.0.1"),
            ]),
            None,
        );
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
        let forwarded = forwarded_headers(
            &headers(&[
                ("connection", "X-Internal-Auth, Keep-Alive"),
                ("x-internal-auth", "hop-scoped"),
                ("proxy-connection", "keep-alive"),
                ("accept", "application/json"),
            ]),
            None,
        );
        assert!(!forwarded.contains_key("x-internal-auth"));
        assert!(!forwarded.contains_key("proxy-connection"));
        assert!(
            forwarded.contains_key("accept"),
            "unrelated headers survive"
        );
    }

    fn identity(host: &str, addr: &str, scheme: &str) -> crate::security::ResolvedClientIdentity {
        crate::security::ResolvedClientIdentity {
            addr: Some(addr.parse().expect("valid ip")),
            host: Some(host.to_owned()),
            scheme: Some(scheme.to_owned()),
        }
    }

    #[test]
    fn the_validated_identity_replaces_the_clients_own_claims() {
        // Behind a proxy the raw `Host` is the INTERNAL address and the public
        // one arrives in `X-Forwarded-Host` — which is stripped here, because a
        // client can forge it. What travels instead is what the trusted-proxy
        // layer ACCEPTED: host, client address, and scheme. Dropping the last
        // two left the candidate resolving this process as the client, so its
        // per-IP limiter bucketed every mirror together and `ClientScheme` fell
        // back to `http`.
        let forwarded = forwarded_headers(
            &headers(&[
                ("host", "10.0.0.7:3000"),
                ("x-forwarded-host", "evil.example"),
                ("x-forwarded-for", "203.0.113.9"),
                ("x-forwarded-proto", "http"),
            ]),
            Some(&identity("app.example.com", "198.51.100.4", "https")),
        );
        assert_eq!(
            forwarded.get("host").and_then(|v| v.to_str().ok()),
            Some("app.example.com")
        );
        assert_eq!(
            forwarded
                .get("x-forwarded-for")
                .and_then(|v| v.to_str().ok()),
            Some("198.51.100.4"),
            "the resolved client address, not the one the client claimed"
        );
        assert_eq!(
            forwarded
                .get("x-forwarded-proto")
                .and_then(|v| v.to_str().ok()),
            Some("https"),
            "the resolved scheme, not the one the client claimed"
        );
        assert!(
            !forwarded.contains_key("x-forwarded-host"),
            "the unvalidated header itself must never travel"
        );
    }

    #[test]
    fn no_forwarding_headers_are_synthesized_without_a_resolved_identity() {
        // Nothing validated the client's claims, so nothing is asserted to the
        // candidate on its behalf.
        let forwarded = forwarded_headers(
            &headers(&[("x-forwarded-for", "203.0.113.9"), ("host", "app.local")]),
            None,
        );
        assert!(!forwarded.contains_key("x-forwarded-for"));
        assert_eq!(
            forwarded.get("host").and_then(|v| v.to_str().ok()),
            Some("app.local")
        );
    }

    #[test]
    fn without_a_resolved_host_the_raw_one_is_used() {
        let forwarded = forwarded_headers(&headers(&[("host", "app.example.com")]), None);
        assert_eq!(
            forwarded.get("host").and_then(|v| v.to_str().ok()),
            Some("app.example.com")
        );
    }

    #[test]
    fn an_inbound_loop_guard_is_replaced_not_duplicated() {
        let forwarded = forwarded_headers(
            &headers(&[(crate::shadow::sample::SHADOW_HEADER, "spoofed")]),
            None,
        );
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
        let forwarded = forwarded_headers(
            &headers(&[
                ("cookie", "session=abc"),
                ("authorization", "Bearer t"),
                ("accept", "application/json"),
                ("x-request-id", "r-1"),
            ]),
            None,
        );
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
