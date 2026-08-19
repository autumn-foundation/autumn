//! The edge capsule wire protocol (version 1).
//!
//! The host and the guest capsule talk NDJSON over the capsule's stdio: one
//! JSON object per line, `\n`-terminated, `op`-tagged. The protocol is
//! deliberately boring — a CDN shim in any language can implement it from this
//! module's documentation alone, which is the whole point: Autumn ships the
//! artifact and the protocol, not a vendor binding.
//!
//! ```text
//! host → guest  {"op":"request","wire_version":1,"provided_capabilities":["kv"],
//!                "method":"GET","uri":"/x?y=z","headers":[["accept","text/html"]],"body_b64":""}
//! guest → host  {"op":"kv_get","key":"greeting"}
//! host → guest  {"op":"kv_value","value_b64":"aGk="}
//! guest → host  {"op":"response","status":200,"headers":[["content-type","text/plain"]],"body_b64":"aGk="}
//! guest → host  {"op":"fallthrough","reason":"unknown_route","detail":"no edge route matches /nope"}
//! ```
//!
//! The dialogue is strictly request-scoped and half-duplex: the guest reads one
//! [`HostFrame::Request`], may interleave any number of
//! [`GuestFrame::KvGet`]/[`HostFrame::KvValue`] exchanges, and closes the
//! exchange with exactly one [`GuestFrame::Response`] or
//! [`GuestFrame::Fallthrough`]. The loop then repeats until stdin reaches EOF,
//! so one process can serve many requests.
//!
//! # Header contract
//!
//! Header names cross the wire lowercased and [canonicalized](canonicalize_headers):
//! sorted by name, insertion order preserved within a name. Bodies cross the
//! wire base64-encoded so a binary response survives a line-oriented,
//! text-shaped protocol unharmed.
//!
//! The host strips [`SENSITIVE_HEADERS`] before sending, and the guest strips
//! them again on receipt. The edge lane has no session, no auth, and no
//! credential store, so a credential reaching a capsule could only ever be a
//! liability.

use std::fmt;
use std::str::FromStr;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize};

use crate::route::EdgeCapability;

/// The wire protocol version this crate speaks.
///
/// A capsule refuses a request frame carrying any other value with
/// [`FallthroughReason::CapsuleError`], so a host and an artifact built from
/// different Autumn versions degrade to origin-serving instead of guessing.
pub const WIRE_VERSION: u32 = 1;

/// Response header a handler (or the router's 404 fallback) sets to say
/// "I cannot serve this at the edge — forward it to the origin".
///
/// The value is a [`FallthroughReason`] string. The runtime converts any
/// response carrying this header into [`EdgeOutcome::Fallthrough`] and never
/// lets the header reach the wire; see [`strip_sentinel`].
pub const FALLTHROUGH_SENTINEL: &str = "x-autumn-edge-fallthrough";

/// Request headers that never reach a capsule.
///
/// Stripped by the host before the request frame is sent and again by the
/// guest on receipt — a defensive double-strip, because the guest cannot audit
/// the host it is running under.
pub const SENSITIVE_HEADERS: &[&str] = &["cookie", "authorization", "proxy-authorization"];

// ── Errors ───────────────────────────────────────────────────────────

/// Something that could not be parsed out of, or encoded onto, the wire.
#[derive(Debug)]
#[non_exhaustive]
pub enum WireError {
    /// The line was not a well-formed frame.
    Json(serde_json::Error),
    /// A `body_b64` / `value_b64` field was not valid base64.
    Base64(base64::DecodeError),
    /// A `reason` field named a fallthrough reason this version does not know.
    UnknownFallthroughReason(String),
    /// A `provided_capabilities` entry named a capability this version does not know.
    UnknownCapability(String),
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(err) => write!(f, "malformed edge wire frame: {err}"),
            Self::Base64(err) => write!(f, "malformed base64 payload on the edge wire: {err}"),
            Self::UnknownFallthroughReason(raw) => {
                write!(f, "unknown edge fallthrough reason `{raw}`")
            }
            Self::UnknownCapability(raw) => write!(f, "unknown edge capability `{raw}`"),
        }
    }
}

impl std::error::Error for WireError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Json(err) => Some(err),
            Self::Base64(err) => Some(err),
            Self::UnknownFallthroughReason(_) | Self::UnknownCapability(_) => None,
        }
    }
}

impl From<serde_json::Error> for WireError {
    fn from(err: serde_json::Error) -> Self {
        Self::Json(err)
    }
}

impl From<base64::DecodeError> for WireError {
    fn from(err: base64::DecodeError) -> Self {
        Self::Base64(err)
    }
}

// ── Base64 body codec ────────────────────────────────────────────────

/// Encode a body for the `body_b64` / `value_b64` wire fields.
#[must_use]
pub fn encode_body(bytes: &[u8]) -> String {
    BASE64.encode(bytes)
}

/// Decode a `body_b64` / `value_b64` wire field.
///
/// # Errors
///
/// Returns [`WireError::Base64`] when `encoded` is not valid standard base64.
pub fn decode_body(encoded: &str) -> Result<Vec<u8>, WireError> {
    Ok(BASE64.decode(encoded)?)
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

mod opt_body_b64 {
    use super::BASE64;
    use base64::Engine as _;
    use serde::{Deserialize as _, Deserializer, Serializer};

    #[allow(
        clippy::ref_option,
        reason = "serde's `with` module contract fixes this signature"
    )]
    pub(super) fn serialize<S: Serializer>(
        bytes: &Option<Vec<u8>>,
        ser: S,
    ) -> Result<S::Ok, S::Error> {
        match bytes {
            Some(bytes) => ser.serialize_some(&BASE64.encode(bytes)),
            None => ser.serialize_none(),
        }
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        de: D,
    ) -> Result<Option<Vec<u8>>, D::Error> {
        let raw = Option::<String>::deserialize(de)?;
        raw.map(|raw| BASE64.decode(&raw).map_err(serde::de::Error::custom))
            .transpose()
    }
}

// ── Header helpers ───────────────────────────────────────────────────

/// Put headers into canonical wire form: names lowercased, sorted by name,
/// insertion order preserved within a name.
///
/// Both sides of the conformance comparison run through this function, so a
/// `HeaderMap` iteration order that depends on hashing can never be mistaken
/// for a real divergence.
#[must_use]
pub fn canonicalize_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = headers
        .iter()
        .map(|(name, value)| (name.to_ascii_lowercase(), value.clone()))
        .collect();
    // `sort_by` is guaranteed stable, which is what preserves the relative
    // order of repeated header names (e.g. two `set-cookie`s).
    out.sort_by(|left, right| left.0.cmp(&right.0));
    out
}

/// Drop every header named in [`SENSITIVE_HEADERS`], case-insensitively,
/// preserving the order of everything else.
#[must_use]
pub fn strip_sensitive_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    headers
        .iter()
        .filter(|(name, _)| {
            let name = name.to_ascii_lowercase();
            !SENSITIVE_HEADERS.contains(&name.as_str())
        })
        .cloned()
        .collect()
}

/// Remove the [`FALLTHROUGH_SENTINEL`] header, returning its first value.
///
/// The runtime calls this on every outgoing response so the sentinel — an
/// internal control channel between a handler and the capsule runtime — can
/// never leak onto the wire and out to a browser.
pub fn strip_sentinel(headers: &mut Vec<(String, String)>) -> Option<String> {
    let mut found = None;
    headers.retain(|(name, value)| {
        if name.eq_ignore_ascii_case(FALLTHROUGH_SENTINEL) {
            if found.is_none() {
                found = Some(value.clone());
            }
            false
        } else {
            true
        }
    });
    found
}

// ── Payloads ─────────────────────────────────────────────────────────

/// A request as it crosses the edge wire.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgeRequest {
    /// HTTP method, uppercased (`GET` / `HEAD` are the only edge-eligible ones).
    pub method: String,
    /// Origin-form request target, including the query string.
    pub uri: String,
    /// Canonicalized request headers.
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    /// Request body. Always empty for the read-path methods slice 1 accepts.
    #[serde(default, rename = "body_b64", with = "body_b64")]
    pub body: Vec<u8>,
}

impl EdgeRequest {
    /// A bodyless `GET` for `uri` with no headers — the shape most conformance
    /// cases need.
    #[must_use]
    pub fn get(uri: impl Into<String>) -> Self {
        Self {
            method: "GET".to_owned(),
            uri: uri.into(),
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    /// Add a request header.
    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Wrap this request into the frame a host sends to a capsule, stripping
    /// [`SENSITIVE_HEADERS`] and canonicalizing what is left.
    #[must_use]
    pub fn into_host_frame(self, provided_capabilities: &[EdgeCapability]) -> HostFrame {
        let headers = canonicalize_headers(&strip_sensitive_headers(&self.headers));
        HostFrame::Request {
            wire_version: WIRE_VERSION,
            provided_capabilities: provided_capabilities.to_vec(),
            request: Self { headers, ..self },
        }
    }
}

/// A response as it crosses the edge wire.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgeResponse {
    /// HTTP status code.
    pub status: u16,
    /// Canonicalized response headers, sentinel already stripped.
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    /// Response body bytes.
    #[serde(default, rename = "body_b64", with = "body_b64")]
    pub body: Vec<u8>,
}

/// Why a request the edge was offered must be served by the origin instead.
///
/// Every variant is a *transparent* outcome: the host forwards the original
/// request upstream and the author writes no glue for any of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FallthroughReason {
    /// No edge route matches the request path.
    UnknownRoute,
    /// The method is not read-path (anything but `GET`/`HEAD`).
    MethodNotEdgeEligible,
    /// The matched route declares a capability this host did not provide.
    MissingCapability,
    /// The capsule could not produce an answer: unsupported wire version,
    /// malformed frame, a trap, or a handler that opted out explicitly.
    CapsuleError,
}

impl FallthroughReason {
    /// The wire spelling of this reason.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnknownRoute => "unknown_route",
            Self::MethodNotEdgeEligible => "method_not_edge_eligible",
            Self::MissingCapability => "missing_capability",
            Self::CapsuleError => "capsule_error",
        }
    }
}

impl fmt::Display for FallthroughReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for FallthroughReason {
    type Err = WireError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw {
            "unknown_route" => Ok(Self::UnknownRoute),
            "method_not_edge_eligible" => Ok(Self::MethodNotEdgeEligible),
            "missing_capability" => Ok(Self::MissingCapability),
            "capsule_error" => Ok(Self::CapsuleError),
            other => Err(WireError::UnknownFallthroughReason(other.to_owned())),
        }
    }
}

/// What the edge lane decided about one request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EdgeOutcome {
    /// The edge answered; these bytes are the answer.
    Served(EdgeResponse),
    /// The edge declined; the host must forward to the origin.
    Fallthrough {
        /// Why the edge declined.
        reason: FallthroughReason,
        /// Human-readable detail, for logs and test failure messages. Never
        /// part of the served response.
        detail: String,
    },
}

impl EdgeOutcome {
    /// Build a [`EdgeOutcome::Fallthrough`].
    #[must_use]
    pub fn fallthrough(reason: FallthroughReason, detail: impl Into<String>) -> Self {
        Self::Fallthrough {
            reason,
            detail: detail.into(),
        }
    }

    /// The response, when the edge served this request.
    #[must_use]
    pub const fn served(&self) -> Option<&EdgeResponse> {
        match self {
            Self::Served(response) => Some(response),
            Self::Fallthrough { .. } => None,
        }
    }

    /// The reason, when the edge declined this request.
    #[must_use]
    pub const fn fallthrough_reason(&self) -> Option<FallthroughReason> {
        match self {
            Self::Served(_) => None,
            Self::Fallthrough { reason, .. } => Some(*reason),
        }
    }
}

// ── Frames ───────────────────────────────────────────────────────────

/// A frame sent by the host to the capsule.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum HostFrame {
    /// Serve this request.
    Request {
        /// Protocol version the host speaks; see [`WIRE_VERSION`].
        wire_version: u32,
        /// Platform seams this host can satisfy for the duration of the request.
        #[serde(default)]
        provided_capabilities: Vec<EdgeCapability>,
        /// The request itself.
        #[serde(flatten)]
        request: EdgeRequest,
    },
    /// The answer to a [`GuestFrame::KvGet`]; `None` is a miss.
    KvValue {
        /// The stored bytes, or `None` on a miss.
        #[serde(rename = "value_b64", with = "opt_body_b64")]
        value: Option<Vec<u8>>,
    },
}

/// A frame sent by the capsule to the host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum GuestFrame {
    /// Read one key from the host's non-authoritative KV replica.
    KvGet {
        /// The key to read.
        key: String,
    },
    /// The edge served the request.
    Response(EdgeResponse),
    /// The edge declined; forward the original request to the origin.
    Fallthrough {
        /// Why the edge declined.
        reason: FallthroughReason,
        /// Human-readable detail.
        detail: String,
    },
}

impl GuestFrame {
    /// The terminal frame for an [`EdgeOutcome`].
    #[must_use]
    pub fn from_outcome(outcome: EdgeOutcome) -> Self {
        match outcome {
            EdgeOutcome::Served(response) => Self::Response(response),
            EdgeOutcome::Fallthrough { reason, detail } => Self::Fallthrough { reason, detail },
        }
    }

    /// The [`EdgeOutcome`] this frame carries, if it is a terminal frame.
    #[must_use]
    pub fn into_outcome(self) -> Option<EdgeOutcome> {
        match self {
            Self::Response(response) => Some(EdgeOutcome::Served(response)),
            Self::Fallthrough { reason, detail } => {
                Some(EdgeOutcome::Fallthrough { reason, detail })
            }
            Self::KvGet { .. } => None,
        }
    }
}

/// Serialize a frame as one NDJSON line, `\n` included.
///
/// # Errors
///
/// Returns [`WireError::Json`] if the frame cannot be serialized, which for
/// these types can only happen on an allocation failure inside `serde_json`.
pub fn to_line<T: Serialize>(frame: &T) -> Result<String, WireError> {
    let mut line = serde_json::to_string(frame)?;
    line.push('\n');
    Ok(line)
}

/// Parse one NDJSON line into a frame.
///
/// # Errors
///
/// Returns [`WireError::Json`] when the line is not a well-formed frame of
/// type `T`.
pub fn from_line<T: serde::de::DeserializeOwned>(line: &str) -> Result<T, WireError> {
    Ok(serde_json::from_str(line.trim_end_matches(['\r', '\n']))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn canonicalize_lowercases_sorts_and_keeps_multi_value_order() {
        let input = owned(&[
            ("X-Foo", "1"),
            ("accept", "a"),
            ("X-FOO", "2"),
            ("Accept", "b"),
        ]);

        assert_eq!(
            canonicalize_headers(&input),
            owned(&[
                ("accept", "a"),
                ("accept", "b"),
                ("x-foo", "1"),
                ("x-foo", "2"),
            ])
        );
    }

    #[test]
    fn canonicalize_is_idempotent() {
        let once = canonicalize_headers(&owned(&[("B", "2"), ("a", "1")]));
        assert_eq!(canonicalize_headers(&once), once);
    }

    #[test]
    fn strip_sensitive_removes_credentials_case_insensitively() {
        let input = owned(&[
            ("Cookie", "sid=1"),
            ("accept", "text/html"),
            ("AUTHORIZATION", "Bearer x"),
            ("Proxy-Authorization", "Basic y"),
            ("x-keep", "yes"),
        ]);

        assert_eq!(
            strip_sensitive_headers(&input),
            owned(&[("accept", "text/html"), ("x-keep", "yes")])
        );
    }

    #[test]
    fn strip_sentinel_removes_every_copy_and_returns_the_first() {
        let mut headers = owned(&[
            ("content-type", "text/plain"),
            (FALLTHROUGH_SENTINEL, "unknown_route"),
            ("X-Autumn-Edge-Fallthrough", "capsule_error"),
        ]);

        assert_eq!(
            strip_sentinel(&mut headers),
            Some("unknown_route".to_owned())
        );
        assert_eq!(headers, owned(&[("content-type", "text/plain")]));
        assert_eq!(strip_sentinel(&mut headers), None);
    }

    #[test]
    fn body_codec_round_trips_binary() {
        let bytes = vec![0u8, 255, 10, 13, 0x7f, b'a'];
        let encoded = encode_body(&bytes);
        assert_eq!(decode_body(&encoded).expect("decodes"), bytes);
        assert_eq!(encode_body(&[]), "");
        assert_eq!(decode_body("").expect("decodes"), Vec::<u8>::new());
    }

    #[test]
    fn body_codec_rejects_garbage() {
        let err = decode_body("!!!not base64!!!").expect_err("must reject");
        assert!(matches!(err, WireError::Base64(_)), "got {err:?}");
    }

    #[test]
    fn request_frame_matches_the_documented_json() {
        let frame = EdgeRequest::get("/x?y=z")
            .with_header("Accept", "text/html")
            .into_host_frame(&[EdgeCapability::Kv]);

        assert_eq!(
            serde_json::to_string(&frame).expect("serializes"),
            r#"{"op":"request","wire_version":1,"provided_capabilities":["kv"],"method":"GET","uri":"/x?y=z","headers":[["accept","text/html"]],"body_b64":""}"#
        );
        assert_eq!(
            from_line::<HostFrame>(&to_line(&frame).expect("line")).expect("round trips"),
            frame
        );
    }

    #[test]
    fn request_frame_strips_credentials_on_the_way_out() {
        let frame = EdgeRequest::get("/x")
            .with_header("Cookie", "sid=1")
            .with_header("Accept", "text/html")
            .into_host_frame(&[]);

        let HostFrame::Request { request, .. } = frame else {
            panic!("expected a request frame");
        };
        assert_eq!(request.headers, owned(&[("accept", "text/html")]));
    }

    #[test]
    fn kv_value_frame_matches_the_documented_json() {
        let hit = HostFrame::KvValue {
            value: Some(b"hi".to_vec()),
        };
        let miss = HostFrame::KvValue { value: None };

        assert_eq!(
            serde_json::to_string(&hit).expect("serializes"),
            r#"{"op":"kv_value","value_b64":"aGk="}"#
        );
        assert_eq!(
            serde_json::to_string(&miss).expect("serializes"),
            r#"{"op":"kv_value","value_b64":null}"#
        );
        assert_eq!(
            from_line::<HostFrame>(r#"{"op":"kv_value","value_b64":"aGk="}"#).expect("parses"),
            hit
        );
        assert_eq!(
            from_line::<HostFrame>(r#"{"op":"kv_value","value_b64":null}"#).expect("parses"),
            miss
        );
    }

    #[test]
    fn guest_frames_match_the_documented_json() {
        let kv_get = GuestFrame::KvGet {
            key: "greeting".to_owned(),
        };
        let response = GuestFrame::Response(EdgeResponse {
            status: 200,
            headers: owned(&[("content-type", "text/plain")]),
            body: b"hi".to_vec(),
        });
        let fallthrough = GuestFrame::Fallthrough {
            reason: FallthroughReason::UnknownRoute,
            detail: "no edge route matches /nope".to_owned(),
        };

        assert_eq!(
            serde_json::to_string(&kv_get).expect("serializes"),
            r#"{"op":"kv_get","key":"greeting"}"#
        );
        assert_eq!(
            serde_json::to_string(&response).expect("serializes"),
            r#"{"op":"response","status":200,"headers":[["content-type","text/plain"]],"body_b64":"aGk="}"#
        );
        assert_eq!(
            serde_json::to_string(&fallthrough).expect("serializes"),
            r#"{"op":"fallthrough","reason":"unknown_route","detail":"no edge route matches /nope"}"#
        );

        for frame in [kv_get, response, fallthrough] {
            assert_eq!(
                from_line::<GuestFrame>(&to_line(&frame).expect("line")).expect("round trips"),
                frame
            );
        }
    }

    #[test]
    fn to_line_terminates_with_a_newline_and_nothing_else() {
        let line = to_line(&GuestFrame::KvGet {
            key: "k".to_owned(),
        })
        .expect("line");
        assert!(line.ends_with('\n'), "{line:?}");
        assert_eq!(line.matches('\n').count(), 1, "{line:?}");
    }

    #[test]
    fn fallthrough_reason_round_trips_through_its_wire_spelling() {
        for reason in [
            FallthroughReason::UnknownRoute,
            FallthroughReason::MethodNotEdgeEligible,
            FallthroughReason::MissingCapability,
            FallthroughReason::CapsuleError,
        ] {
            assert_eq!(
                reason
                    .as_str()
                    .parse::<FallthroughReason>()
                    .expect("parses"),
                reason
            );
            assert_eq!(reason.to_string(), reason.as_str());
        }
        assert!(matches!(
            "nope".parse::<FallthroughReason>(),
            Err(WireError::UnknownFallthroughReason(_))
        ));
    }

    #[test]
    fn outcome_accessors_discriminate() {
        let served = EdgeOutcome::Served(EdgeResponse::default());
        let declined = EdgeOutcome::fallthrough(FallthroughReason::CapsuleError, "boom");

        assert!(served.served().is_some());
        assert_eq!(served.fallthrough_reason(), None);
        assert!(declined.served().is_none());
        assert_eq!(
            declined.fallthrough_reason(),
            Some(FallthroughReason::CapsuleError)
        );
        assert_eq!(
            GuestFrame::from_outcome(declined.clone())
                .into_outcome()
                .as_ref(),
            Some(&declined)
        );
        assert_eq!(
            GuestFrame::KvGet {
                key: "k".to_owned()
            }
            .into_outcome(),
            None
        );
    }
}
