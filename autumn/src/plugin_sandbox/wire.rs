//! The sandbox wire protocol (version 1).
//!
//! The host and the guest talk NDJSON over the guest's stdio: one JSON object
//! per line, `\n`-terminated, `op`-tagged. That is the entire ABI. It is
//! deliberately boring, so a plugin can be written in any language that can
//! target `wasm32-wasip1` — with no bindgen, no vendor SDK and no shared
//! header — and so an author debugging a plugin can read the traffic.
//!
//! ```text
//! host  → guest  {"op":"request","wire_version":1,"granted":["http-request"],
//!                 "method":"GET","route":"/hello/{id}","path":"/hello/7","query":"a=b",
//!                 "path_params":[["id","7"]],"headers":[["accept","text/html"]],"body_b64":""}
//! guest → host   {"op":"response","status":200,"headers":[["content-type","text/plain"]],"body_b64":"aGk="}
//! guest → host   {"op":"error","detail":"no handler for that route"}
//! ```
//!
//! The dialogue is strictly one frame in, one frame out. There is no seam the
//! guest can use to ask the host for anything, because in this slice the guest
//! is granted nothing to ask for.
//!
//! # What never crosses, in either direction
//!
//! [`SENSITIVE_REQUEST_HEADERS`] are stripped before the request frame is
//! built. The sandbox grants no session, auth or credential capability, so a
//! cookie or bearer token reaching a plugin could only ever be a liability —
//! and a plugin that echoed request headers would otherwise leak one.
//!
//! [`DENIED_RESPONSE_HEADERS`] are stripped from whatever the guest answers.
//! `Set-Cookie` is the load-bearing one: a plugin that could set a cookie could
//! forge a session in the host application's own origin, which is precisely the
//! authority the sandbox exists to withhold. The rest are framing headers
//! (`Content-Length`, `Transfer-Encoding`, hop-by-hop) that belong to the host's
//! HTTP stack, not to a guest.
//!
//! Header names and values are validated before they reach a response: a value
//! carrying `\r\n` would otherwise let a guest inject headers of its own into
//! the host's response, which is a `Set-Cookie` by another route.

use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize};

use super::manifest::{SandboxCapability, WIRE_VERSION};

/// Request headers that never reach a sandboxed plugin.
pub const SENSITIVE_REQUEST_HEADERS: &[&str] = &[
    "cookie",
    "authorization",
    "proxy-authorization",
    "www-authenticate",
    "proxy-authenticate",
];

/// Response headers a sandboxed plugin may not set.
pub const DENIED_RESPONSE_HEADERS: &[&str] = &[
    // Authority the sandbox exists to withhold.
    "set-cookie",
    "set-cookie2",
    // Framing and transport: the host's HTTP stack owns these.
    "content-length",
    "content-encoding",
    "transfer-encoding",
    "connection",
    "keep-alive",
    "upgrade",
    "te",
    "trailer",
    "proxy-authenticate",
    "www-authenticate",
];

/// Something that could not be parsed out of, or encoded onto, the wire.
#[derive(Debug)]
#[non_exhaustive]
pub enum WireError {
    /// The line was not a well-formed frame.
    Json(serde_json::Error),
    /// A response carried a status outside the HTTP range.
    InvalidStatus(u16),
    /// A response carried a header name or value HTTP cannot represent.
    InvalidHeader {
        /// The offending name.
        name: String,
        /// Why it was refused.
        reason: &'static str,
    },
    /// The response frame was larger than the plugin's declared ceiling.
    ResponseTooLarge {
        /// The response's size in bytes.
        found: usize,
        /// The plugin's ceiling.
        max: usize,
    },
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(err) => write!(f, "malformed sandbox wire frame: {err}"),
            Self::InvalidStatus(status) => {
                write!(f, "the plugin answered with status {status}, which is not a valid HTTP status")
            }
            Self::InvalidHeader { name, reason } => {
                write!(f, "the plugin answered with an invalid header {name:?}: {reason}")
            }
            Self::ResponseTooLarge { found, max } => write!(
                f,
                "the plugin answered with {found} bytes, over its declared {max}-byte ceiling"
            ),
        }
    }
}

impl std::error::Error for WireError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Json(err) => Some(err),
            _ => None,
        }
    }
}

impl From<serde_json::Error> for WireError {
    fn from(err: serde_json::Error) -> Self {
        Self::Json(err)
    }
}

/// Serialize a frame as one newline-terminated NDJSON line.
///
/// # Errors
///
/// Returns [`WireError::Json`] if the frame cannot be serialized.
pub fn to_line<T: Serialize>(frame: &T) -> Result<String, WireError> {
    let mut line = serde_json::to_string(frame)?;
    line.push('\n');
    Ok(line)
}

/// Parse one NDJSON line into a frame.
///
/// # Errors
///
/// Returns [`WireError::Json`] if the line is not a well-formed frame of the
/// expected shape — including an `op` this version does not know, which is
/// refused rather than ignored.
pub fn from_line<T: serde::de::DeserializeOwned>(line: &str) -> Result<T, WireError> {
    Ok(serde_json::from_str(line)?)
}

mod body_b64 {
    use super::BASE64;
    use base64::Engine as _;
    use serde::{Deserialize as _, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(bytes: &[u8], ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&BASE64.encode(bytes))
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Vec<u8>, D::Error> {
        let raw = String::deserialize(de)?;
        BASE64.decode(&raw).map_err(serde::de::Error::custom)
    }
}

/// One HTTP request, as the host hands it to a plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxRequest {
    /// The request method, upper-case.
    pub method: String,
    /// The declared route pattern that matched (e.g. `/hello/{id}`), so a guest
    /// can dispatch without re-implementing a router.
    pub route: String,
    /// The concrete request path.
    pub path: String,
    /// The raw query string, without the leading `?`.
    pub query: String,
    /// Captured path parameters, in the router's order.
    pub path_params: Vec<(String, String)>,
    /// Request headers, as received (canonicalized on the way out).
    pub headers: Vec<(String, String)>,
    /// The request body.
    pub body: Vec<u8>,
}

/// One HTTP response, as a plugin hands it back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxResponse {
    /// HTTP status code.
    pub status: u16,
    /// Response headers.
    pub headers: Vec<(String, String)>,
    /// Response body.
    #[serde(rename = "body_b64", with = "body_b64")]
    pub body: Vec<u8>,
}

impl SandboxResponse {
    /// Strip every header a plugin may not set.
    ///
    /// Returns the cleaned response and the lower-cased names that were
    /// removed, so the caller can record one denial per strip: a plugin
    /// reaching for `Set-Cookie` is a thing an operator wants in their log,
    /// not something to silently drop.
    #[must_use]
    pub fn sanitize(mut self) -> (Self, Vec<String>) {
        let mut denied = Vec::new();
        self.headers.retain(|(name, _)| {
            let lower = name.to_ascii_lowercase();
            if DENIED_RESPONSE_HEADERS.contains(&lower.as_str()) {
                denied.push(lower);
                false
            } else {
                true
            }
        });
        (self, denied)
    }

    /// Check the status and every header against what HTTP can represent.
    ///
    /// # Errors
    ///
    /// Returns [`WireError::InvalidStatus`] for a status outside `100..=599`
    /// and [`WireError::InvalidHeader`] for a name or value that is empty, not
    /// a legal HTTP token, or carries a control character — a value containing
    /// `\r\n` would otherwise be response splitting.
    pub fn validate(&self) -> Result<(), WireError> {
        if !(100..=599).contains(&self.status) {
            return Err(WireError::InvalidStatus(self.status));
        }
        for (name, value) in &self.headers {
            if http::HeaderName::try_from(name.as_str()).is_err() {
                return Err(WireError::InvalidHeader {
                    name: name.clone(),
                    reason: "not a legal HTTP header name",
                });
            }
            if http::HeaderValue::try_from(value.as_str()).is_err() {
                return Err(WireError::InvalidHeader {
                    name: name.clone(),
                    reason: "not a legal HTTP header value (control characters are refused, \
                             because a `\\r\\n` here would be response splitting)",
                });
            }
        }
        Ok(())
    }

    /// Refuse a response larger than the plugin's declared ceiling.
    ///
    /// # Errors
    ///
    /// Returns [`WireError::ResponseTooLarge`] when the body plus headers
    /// exceed `max`.
    pub fn check_size(&self, max: usize) -> Result<(), WireError> {
        let header_bytes: usize = self
            .headers
            .iter()
            .map(|(name, value)| name.len().saturating_add(value.len()).saturating_add(4))
            .sum();
        let found = self.body.len().saturating_add(header_bytes);
        if found > max {
            return Err(WireError::ResponseTooLarge { found, max });
        }
        Ok(())
    }
}

/// A frame the host sends to the guest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum HostFrame {
    /// One HTTP request to serve.
    Request {
        /// The protocol version this frame speaks.
        wire_version: u32,
        /// The capabilities this host is honouring for the request. A guest
        /// should refuse to run if something it needs is missing rather than
        /// assuming.
        granted: Vec<SandboxCapability>,
        /// Request method.
        method: String,
        /// The matched route pattern.
        route: String,
        /// The concrete path.
        path: String,
        /// Raw query string.
        query: String,
        /// Captured path parameters.
        path_params: Vec<(String, String)>,
        /// Canonicalized request headers, credentials already removed.
        headers: Vec<(String, String)>,
        /// The request body.
        #[serde(rename = "body_b64", with = "body_b64")]
        body: Vec<u8>,
    },
}

impl HostFrame {
    /// Build the request frame for `request`, stripping credentials and
    /// canonicalizing headers.
    #[must_use]
    pub fn request(request: &SandboxRequest, granted: &[SandboxCapability]) -> Self {
        Self::Request {
            wire_version: WIRE_VERSION,
            granted: granted.to_vec(),
            method: request.method.clone(),
            route: request.route.clone(),
            path: request.path.clone(),
            query: request.query.clone(),
            path_params: request.path_params.clone(),
            headers: canonicalize_headers(&request.headers),
            body: request.body.clone(),
        }
    }
}

/// A frame the guest sends back. Either one ends the exchange.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum GuestFrame {
    /// The plugin's answer.
    Response(SandboxResponse),
    /// The plugin reporting that it could not answer. The host turns this into
    /// a 5xx on the plugin's prefix; it is never a host failure.
    Error {
        /// What went wrong, for the log.
        detail: String,
    },
}

/// Lower-case header names, drop the ones that never cross, and sort by name
/// with insertion order preserved within a name.
///
/// Sorting makes the frame a deterministic function of the request, which is
/// what lets an author diff two runs of the same request and see only what
/// changed.
#[must_use]
pub fn canonicalize_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = headers
        .iter()
        .map(|(name, value)| (name.to_ascii_lowercase(), value.clone()))
        .filter(|(name, _)| !SENSITIVE_REQUEST_HEADERS.contains(&name.as_str()))
        .collect();
    out.sort_by(|(left, _), (right, _)| left.cmp(right));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> SandboxRequest {
        SandboxRequest {
            method: "GET".to_owned(),
            route: "/hello/{id}".to_owned(),
            path: "/hello/7".to_owned(),
            query: "a=b".to_owned(),
            path_params: vec![("id".to_owned(), "7".to_owned())],
            headers: vec![
                ("Accept".to_owned(), "text/html".to_owned()),
                ("Cookie".to_owned(), "session=secret".to_owned()),
                ("Authorization".to_owned(), "Bearer secret".to_owned()),
                ("X-Trace".to_owned(), "abc".to_owned()),
            ],
            body: b"hi".to_vec(),
        }
    }

    #[test]
    fn a_request_frame_round_trips() {
        let line = to_line(&HostFrame::request(&request(), &[SandboxCapability::HttpRequest]))
            .expect("serializes");
        assert!(line.ends_with('\n'), "frames are newline terminated");
        let back: HostFrame = from_line(line.trim_end()).expect("parses");
        let HostFrame::Request {
            wire_version,
            granted,
            method,
            path,
            body,
            ..
        } = back;
        assert_eq!(wire_version, WIRE_VERSION);
        assert_eq!(granted, vec![SandboxCapability::HttpRequest]);
        assert_eq!(method, "GET");
        assert_eq!(path, "/hello/7");
        assert_eq!(body, b"hi");
    }

    #[test]
    fn credentials_never_cross_the_boundary() {
        let frame = HostFrame::request(&request(), &[SandboxCapability::HttpRequest]);
        let json = to_line(&frame).expect("serializes");
        assert!(!json.contains("session=secret"), "{json}");
        assert!(!json.contains("Bearer secret"), "{json}");
        assert!(json.contains("x-trace"), "{json}");
        let HostFrame::Request { headers, .. } = frame;
        let names: Vec<&str> = headers.iter().map(|(name, _)| name.as_str()).collect();
        assert!(!names.contains(&"cookie"), "{names:?}");
        assert!(!names.contains(&"authorization"), "{names:?}");
    }

    #[test]
    fn request_headers_are_lower_cased_and_sorted_with_stable_duplicates() {
        let mut req = request();
        req.headers = vec![
            ("X-B".to_owned(), "2".to_owned()),
            ("X-A".to_owned(), "first".to_owned()),
            ("x-a".to_owned(), "second".to_owned()),
        ];
        let HostFrame::Request { headers, .. } =
            HostFrame::request(&req, &[SandboxCapability::HttpRequest]);
        assert_eq!(
            headers,
            vec![
                ("x-a".to_owned(), "first".to_owned()),
                ("x-a".to_owned(), "second".to_owned()),
                ("x-b".to_owned(), "2".to_owned()),
            ]
        );
    }

    #[test]
    fn a_response_frame_parses() {
        let line = r#"{"op":"response","status":201,"headers":[["content-type","text/plain"]],"body_b64":"aGk="}"#;
        let frame: GuestFrame = from_line(line).expect("parses");
        let GuestFrame::Response(response) = frame else {
            panic!("expected a response frame");
        };
        assert_eq!(response.status, 201);
        assert_eq!(response.body, b"hi");
        assert_eq!(
            response.headers,
            vec![("content-type".to_owned(), "text/plain".to_owned())]
        );
    }

    #[test]
    fn an_error_frame_parses() {
        let frame: GuestFrame =
            from_line(r#"{"op":"error","detail":"no handler"}"#).expect("parses");
        assert!(matches!(frame, GuestFrame::Error { .. }), "{frame:?}");
    }

    #[test]
    fn an_unknown_op_is_refused() {
        let err = from_line::<GuestFrame>(r#"{"op":"exec","cmd":"sh"}"#)
            .expect_err("an unknown op must be refused");
        assert!(matches!(err, WireError::Json(_)), "{err}");
    }

    #[test]
    fn malformed_json_is_refused() {
        assert!(from_line::<GuestFrame>("not json").is_err());
        assert!(from_line::<GuestFrame>("").is_err());
    }

    #[test]
    fn a_body_that_is_not_base64_is_refused() {
        let line = r#"{"op":"response","status":200,"headers":[],"body_b64":"!!!!"}"#;
        assert!(from_line::<GuestFrame>(line).is_err());
    }

    #[test]
    fn a_status_outside_http_range_is_refused() {
        for status in [0u16, 99, 600, 1000] {
            let response = SandboxResponse {
                status,
                headers: vec![],
                body: vec![],
            };
            assert!(
                response.validate().is_err(),
                "status {status} must be refused"
            );
        }
        for status in [100u16, 200, 404, 500, 599] {
            let response = SandboxResponse {
                status,
                headers: vec![],
                body: vec![],
            };
            assert!(response.validate().is_ok(), "status {status} must be allowed");
        }
    }

    #[test]
    fn a_plugin_cannot_mint_a_session_cookie() {
        let response = SandboxResponse {
            status: 200,
            headers: vec![
                ("Set-Cookie".to_owned(), "session=forged".to_owned()),
                ("content-type".to_owned(), "text/plain".to_owned()),
            ],
            body: vec![],
        };
        let (clean, denied) = response.sanitize();
        assert_eq!(denied, vec!["set-cookie".to_owned()]);
        assert_eq!(
            clean.headers,
            vec![("content-type".to_owned(), "text/plain".to_owned())]
        );
    }

    #[test]
    fn a_plugin_cannot_control_response_framing() {
        for name in [
            "content-length",
            "transfer-encoding",
            "connection",
            "upgrade",
            "keep-alive",
            "te",
            "trailer",
            "proxy-authenticate",
            "content-encoding",
        ] {
            let response = SandboxResponse {
                status: 200,
                headers: vec![(name.to_owned(), "x".to_owned())],
                body: vec![],
            };
            let (clean, denied) = response.sanitize();
            assert_eq!(denied, vec![name.to_owned()], "{name} must be stripped");
            assert!(clean.headers.is_empty());
        }
    }

    #[test]
    fn a_header_that_would_split_the_response_is_refused() {
        for (name, value) in [
            ("x-evil", "a\r\nSet-Cookie: forged=1"),
            ("x-evil\r\nSet-Cookie: forged=1", "a"),
            ("x-evil", "a\nb"),
            ("", "a"),
            ("x evil", "a"),
        ] {
            let response = SandboxResponse {
                status: 200,
                headers: vec![(name.to_owned(), value.to_owned())],
                body: vec![],
            };
            assert!(
                response.validate().is_err(),
                "header {name:?}: {value:?} must be refused"
            );
        }
    }

    #[test]
    fn a_response_over_the_ceiling_is_refused() {
        let response = SandboxResponse {
            status: 200,
            headers: vec![],
            body: vec![0u8; 128],
        };
        assert!(response.check_size(64).is_err());
        assert!(response.check_size(4096).is_ok());
    }
}
