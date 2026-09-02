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
//! Both directions are **allowlists**. Only [`ALLOWED_REQUEST_HEADERS`] reach
//! the guest: the sandbox grants no session, auth or credential capability, so
//! a cookie or a bearer token reaching a plugin could only ever be a liability
//! — and there is no finished list of what a credential is called, because
//! every authenticating proxy invents its own header for one.
//!
//! On the way back, only [`ALLOWED_RESPONSE_HEADERS`] survive, and only the
//! content types in [`ALLOWED_RESPONSE_CONTENT_TYPES`] are served at all. Both
//! are allowlists for the same reason: the sandbox's whole job is to withhold
//! the host application's origin, and a response is served *from* that origin.
//! A `Set-Cookie` forges a session in it, a `Strict-Transport-Security`
//! rewrites its TLS posture, and an `application/javascript` body executes in
//! it. A deny-list would have to name each of those and the next one nobody has
//! standardised yet.
//!
//! Header names and values are validated before they reach a response: a value
//! carrying `\r\n` would otherwise let a guest inject headers of its own into
//! the host's response, which is a `Set-Cookie` by another route.
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

use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize};

use super::manifest::{SandboxCapability, WIRE_VERSION};

/// Request headers that may reach a sandboxed plugin.
///
/// An **allowlist**, for the same reason the response side is one. A denylist
/// of credential headers is a losing game: the RFCs name `Cookie` and
/// `Authorization`, but every authenticating proxy invents its own —
/// `Cf-Access-Jwt-Assertion`, `X-Forwarded-User`, `X-Amzn-Oidc-Data`,
/// `X-Ms-Client-Principal`, `X-Goog-Iap-Jwt-Assertion` — and each one is a
/// bearer credential the sandbox promised would not cross. There is no version
/// of that list that is finished.
///
/// What a plugin actually needs is content negotiation and conditional-request
/// metadata, which is short and does not grow. Everything else is dropped
/// silently: a request header is not something a plugin *asked* for, so a
/// denial record would be noise on every request rather than evidence.
pub const ALLOWED_REQUEST_HEADERS: &[&str] = &[
    "accept",
    "accept-charset",
    "accept-encoding",
    "accept-language",
    "cache-control",
    "content-length",
    "content-type",
    "if-match",
    "if-modified-since",
    "if-none-match",
    "if-range",
    "if-unmodified-since",
    "range",
    "user-agent",
];

/// Whether a request header of this name may cross into a guest.
///
/// Case-insensitive, and deliberately so: HTTP header names are, and a caller
/// that had to lower-case first would have to *allocate* first. That allocation
/// was the whole cost of deciding a header does not cross — a name arrives from
/// an untrusted `SandboxRequest`, is not bounded by the metadata ceiling once it
/// is dropped, and copying it in order to refuse it is work an attacker chooses
/// the size of. `eq_ignore_ascii_case` compares the borrowed name in place and
/// rejects on length before it looks at a byte.
#[must_use]
pub fn request_header_allowed(name: &str) -> bool {
    ALLOWED_REQUEST_HEADERS
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(name))
}

/// Response headers a sandboxed plugin may set.
///
/// A closed allowlist, with no `x-` escape hatch. An allowlist that ends in
/// "…and anything starting with `x-`" is not one: `X-Accel-Redirect` (nginx)
/// and `X-Sendfile` (Apache) make a *reverse proxy* serve an internal URI or a
/// local file, so that hatch would hand a filesystem-free plugin the
/// filesystem, one hop upstream. `X-Accel-Buffering` alone can change the
/// proxy's streaming behaviour for the whole connection.
///
/// The names here are the ones a response genuinely needs, and the list is
/// meant to grow by review rather than by pattern. Anything outside it —
/// including the host's own `x-autumn-sandboxed` and `x-content-type-options`,
/// which a guest must not be able to forge — is stripped and recorded as a
/// denial.
pub const ALLOWED_RESPONSE_HEADERS: &[&str] = &[
    "accept-ranges",
    "age",
    "cache-control",
    "content-disposition",
    "content-language",
    "content-range",
    "content-type",
    "etag",
    "expires",
    "last-modified",
    "location",
    "retry-after",
    "vary",
];

/// May a guest redirect a client to `target`?
///
/// Only to a path inside its own declared prefix. Everything else is refused:
/// an absolute URL (`https://evil.example/`), a protocol-relative one
/// (`//evil.example/`), a path elsewhere in the host's origin (`/admin`), and a
/// path that *starts* inside the prefix but climbs out of it
/// (`/hello/../admin`) — the last is why this walks segments rather than
/// checking a string prefix, and why a prefix match alone would have been a
/// hole rather than a fix.
///
/// A bare prefix match would also accept `/hellofoo` for prefix `/hello`, so
/// the character after the prefix has to be a separator or nothing at all.
///
/// Two of the refusals below exist because the client normalises the target
/// before following it, and a check that reads the bytes as sent is answering a
/// different question than the one that matters: percent-encoded double-dot
/// segments (`%2e%2e`) and ASCII tabs or newlines, which the client removes
/// outright.
fn redirect_target_allowed(target: &str, prefix: &str) -> bool {
    // A backslash is a path separator to some clients but not to this check,
    // so a target carrying one is refused rather than guessed at.
    if target.contains('\\') {
        return false;
    }
    // A client *deletes* every ASCII tab and newline from a URL before it
    // parses it — the URL Standard says so in as many words ("Remove all ASCII
    // tab or newline from input") — so a target carrying one does not say here
    // what it will say there. `/hello/<TAB>../admin` walks below as the
    // ordinary segment `<TAB>..`, which climbs nothing; the client removes the
    // tab, resolves `/hello/../admin`, and arrives at `/admin`. HTTP permits a
    // tab in a field value, so the header validator does not stop it either,
    // and a 307/308 then carries the client's credentials to a path this
    // plugin was never mounted on.
    //
    // Refused rather than stripped and re-checked: two normalisations that have
    // to agree is the shape of the next bypass, and no legitimate redirect
    // needs a control character. The whole C0 range goes, not just the three
    // that are deleted — the parser also trims leading and trailing controls,
    // so none of them mean here what they mean at the client.
    //
    // And the space with them, which the C0 range does not cover and the parser
    // trims for the same reason: `Location: /hello/.. ` walks below as the
    // segment `.. `, which climbs nothing, and reaches the client as
    // `/hello/..`, which resolves to `/`. A legitimate target percent-encodes a
    // space, so refusing the raw byte costs nothing a redirect actually needs —
    // and refusing it anywhere rather than only at the ends keeps this one rule
    // instead of a second normalisation that has to agree with the client's.
    if target.chars().any(|ch| ch.is_ascii_control() || ch == ' ') {
        return false;
    }
    let Some(rest) = target.strip_prefix(prefix) else {
        return false;
    };
    if !(rest.is_empty() || rest.starts_with('/') || rest.starts_with('?') || rest.starts_with('#'))
    {
        return false;
    }
    // No segment may climb out of the prefix. The query and fragment are not
    // path, so stop before them.
    let path = rest.split(['?', '#']).next().unwrap_or_default();
    !path.split('/').any(is_double_dot_segment)
}

/// Is this path segment a "double-dot segment" — one that climbs a level?
///
/// Comparing against `".."` alone is not enough, and the difference is a real
/// bypass rather than a nicety: a client normalises the URL *before* it
/// follows the redirect, and the URL Standard defines a double-dot segment as
/// `..`, `.%2e`, `%2e.` or `%2e%2e`, ASCII case-insensitively. So
/// `/hello/%2e%2e/admin` is not literally `..` anywhere, and a browser still
/// resolves it to `/admin`.
///
/// One level of decoding is the right amount: a client decodes percent-escapes
/// once during normalisation, so `%252e%252e` becomes the literal text
/// `%2e%2e` and stays a segment name rather than climbing.
fn is_double_dot_segment(segment: &str) -> bool {
    // Compared against the borrowed segment. Lower-casing it first copied a
    // string the guest chose the length of — a `Location` with no `/` after the
    // prefix is one segment holding the whole value — to answer a question four
    // fixed spellings can answer in place, while `run` still holds the parsed
    // response and its clone.
    ["..", ".%2e", "%2e.", "%2e%2e"]
        .iter()
        .any(|spelling| spelling.eq_ignore_ascii_case(segment))
}

/// Content types a sandboxed plugin's response may declare.
///
/// The sandbox withholds the host application's origin. A response served from
/// the host's own origin gives it back, so the type matters as much as the
/// bytes: `application/javascript` under `script-src 'self'` is script
/// execution in the host's origin, and `text/html` is a document in it. Neither
/// is a capability this slice grants, and neither can be made safe by a header
/// the application's own security middleware is entitled to overwrite.
///
/// So the first slice serves data, not documents. Every entry here is a type a
/// browser will neither execute nor render as a same-origin document —
/// `image/svg+xml` is absent for exactly that reason, and so are HTML, CSS,
/// JavaScript, XML and PDF. Widening this list is a later slice's job, and the
/// honest way to do it is to serve a plugin's documents from an origin of their
/// own.
///
/// Matching is on the type/subtype only; parameters (`; charset=utf-8`) are
/// ignored.
pub const ALLOWED_RESPONSE_CONTENT_TYPES: &[&str] = &[
    "text/plain",
    "text/csv",
    "application/json",
    "application/octet-stream",
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
    "image/avif",
];

/// Whether a plugin may set a response header of this (lower-cased) name.
#[must_use]
pub fn response_header_allowed(name: &str) -> bool {
    ALLOWED_RESPONSE_HEADERS
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(name))
}

/// Is this content type's essence one the slice serves?
///
/// Answered against the borrowed essence, for the same reason
/// [`response_header_allowed`] is: the caller must not have to copy a
/// guest-sized string to ask.
#[must_use]
fn content_type_allowed(essence: &str) -> bool {
    ALLOWED_RESPONSE_CONTENT_TYPES
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(essence))
}

/// What a denial records of a guest-chosen value the allowlist refused.
///
/// A response header name is the guest's, and bounded only by the stdout line
/// ceiling — roughly twice the response ceiling. Lower-casing it to decide
/// whether it is allowed copied all of that, and pushing it into the denial
/// list *kept* the copy until the caller logged it, beside the parsed response
/// and the clone `run` holds. `guest_text` bounds it at the log, which is the
/// printing rather than the copy: the same place the import list used to bound
/// its names, and for the same reason it no longer does.
///
/// The name still has to identify the header — a plugin reaching for
/// `Set-Cookie` is what an operator wants to see — so this keeps enough to name
/// it and says when it kept less.
const DENIED_NAME_EXCERPT: usize = 128;

fn denied_excerpt(name: &str) -> String {
    let mut out = String::with_capacity(name.len().min(DENIED_NAME_EXCERPT));
    for (kept, ch) in name.chars().enumerate() {
        if kept == DENIED_NAME_EXCERPT {
            out.push_str(super::host::TRUNCATION_MARKER);
            break;
        }
        out.push(ch.to_ascii_lowercase());
    }
    out
}

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
    /// The response declared a content type this slice does not serve.
    UnsupportedContentType(String),
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
                write!(
                    f,
                    "the plugin answered with status {status}, which is not a valid HTTP status"
                )
            }
            Self::InvalidHeader { name, reason } => {
                write!(
                    f,
                    "the plugin answered with an invalid header {name:?}: {reason}"
                )
            }
            Self::UnsupportedContentType(essence) => write!(
                f,
                "the plugin answered with content type `{essence}`, which a sandboxed plugin may \
                 not serve: a document or a script from the host's own origin would carry the \
                 host's authority. Allowed: {allowed}",
                allowed = ALLOWED_RESPONSE_CONTENT_TYPES.join(", ")
            ),
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
pub(crate) fn to_line<T: Serialize>(frame: &T) -> Result<String, WireError> {
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
pub(crate) fn from_line<T: serde::de::DeserializeOwned>(line: &str) -> Result<T, WireError> {
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
        // `Cow`, not `String`: base64's alphabet contains nothing JSON escapes,
        // so an honest frame borrows straight out of the line the guest wrote
        // and the decode is the only allocation. A guest that writes `\u0041`
        // forces the owned copy — which is why the footprint still budgets for
        // it — but it cannot make that the ordinary case.
        let raw = std::borrow::Cow::<'de, str>::deserialize(de)?;
        BASE64
            .decode(raw.as_ref())
            .map_err(serde::de::Error::custom)
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

/// The most headers a guest's response frame may carry.
///
/// This is a *structural* bound, enforced while the frame is deserialized
/// rather than after. The response ceiling bounds the frame's bytes, not its
/// shape, and the two amplify very differently: a line of `["",""]` pairs is
/// eight bytes each on the wire and a 48-byte `(String, String)` in the vector,
/// so a frame at the stdout budget expands by roughly six before a single
/// length check runs. `sanitize` then strips those headers, so `check_size`
/// never sees the bytes they cost — the footprint a manifest declares, and that
/// an operator sizes a host against, would silently not be the bound.
///
/// Sixty-four is far past what the response-header allowlist can usefully
/// carry; a guest against this limit is malfunctioning or hostile.
pub const MAX_RESPONSE_HEADERS: usize = 64;

/// Deserialize a header list, refusing one longer than
/// [`MAX_RESPONSE_HEADERS`] as it streams.
mod bounded_headers {
    use serde::de::{Deserializer, Error as _, SeqAccess, Visitor};
    use std::fmt;

    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<Vec<(String, String)>, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct Bounded;

        impl<'de> Visitor<'de> for Bounded {
            type Value = Vec<(String, String)>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(
                    formatter,
                    "at most {} response header pairs",
                    super::MAX_RESPONSE_HEADERS
                )
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                // Deliberately not `size_hint`-driven: the hint is computed from
                // the guest's own framing, so reserving against it hands the
                // allocation straight back to the thing this cap refuses.
                let mut headers = Vec::new();
                while let Some(pair) = seq.next_element::<(String, String)>()? {
                    if headers.len() >= super::MAX_RESPONSE_HEADERS {
                        return Err(A::Error::custom(format!(
                            "a response may carry at most {} headers",
                            super::MAX_RESPONSE_HEADERS
                        )));
                    }
                    headers.push(pair);
                }
                Ok(headers)
            }
        }

        deserializer.deserialize_seq(Bounded)
    }
}

/// One HTTP response, as a plugin hands it back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxResponse {
    /// HTTP status code.
    pub status: u16,
    /// Response headers, capped at [`MAX_RESPONSE_HEADERS`] on the way in.
    #[serde(deserialize_with = "bounded_headers::deserialize")]
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
    pub fn sanitize(mut self, prefix: &str) -> (Self, Vec<String>) {
        let mut denied = Vec::new();
        self.headers.retain(|(name, value)| {
            // Matched against the borrowed name. Lower-casing first copied every
            // name to answer a question the allowlist can answer without one,
            // and the copy of a *refused* name was then kept until it was
            // logged — so the longer the name a guest chose, the more the host
            // held to throw it away.
            if !response_header_allowed(name) {
                denied.push(denied_excerpt(name));
                return false;
            }
            // `Location` is the one allowed header that makes the *client* act,
            // and a client acts with the user's credentials. A guest answering
            // `307` or `308` with `Location: /admin/...` has a conforming
            // browser re-issue the request to the host's own origin with the
            // method, the body and the cookies intact — so a plugin holding no
            // session capability induces an authenticated request outside its
            // mount. Confining the target to the plugin's own prefix keeps the
            // redirect a plugin genuinely needs (its own routes) and takes away
            // the one it must not have. An absolute URL is refused by the same
            // rule, which closes the open-redirect half as well.
            if name.eq_ignore_ascii_case("location") && !redirect_target_allowed(value, prefix) {
                denied.push(denied_excerpt(name));
                return false;
            }
            true
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
        if !(200..=599).contains(&self.status) {
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

    /// The response's declared content type, if it is one this slice refuses.
    ///
    /// Returns the offending type so the caller can record a denial naming it.
    #[must_use]
    pub fn refused_content_type(&self) -> Option<String> {
        // *Every* declared type, not the first. Header values are appended to
        // the response, so a second `content-type` reaches the client too, and
        // a proxy or client that takes the last one sees whatever it says. A
        // check that read only the first would enforce the allowlist against
        // the value a guest least wanted to smuggle anything through.
        // The essence is borrowed, not lower-cased. With no `;` it is the whole
        // header value, which the guest sizes, and copying it was how the
        // allowlist was consulted — a third live copy beside the parsed
        // response and the clone `run` holds, to decide the type is not one
        // this slice serves.
        let mut declared = self
            .headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            .map(|(_, value)| value.split(';').next().unwrap_or_default().trim());

        let first = declared.next()?;

        // Two content-types is not a response any client agrees on the meaning
        // of, so it is refused whether or not both are allowed: "the allowlist
        // passed one of them" is not a property worth having. The refused type
        // reported is whichever one is actually off the list, so the denial
        // names the interesting header rather than the first one.
        if let Some(extra) = declared.next() {
            return Some(if content_type_allowed(extra) {
                denied_excerpt(first)
            } else {
                denied_excerpt(extra)
            });
        }

        if content_type_allowed(first) {
            None
        } else {
            Some(denied_excerpt(first))
        }
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
///
/// Crate-private: the framing is an implementation detail of
/// [`SandboxHost::run`](super::host::SandboxHost::run) on the host side, and on
/// the guest side it is a *protocol*, documented in this module and in
/// `docs/guide/sandboxed-plugins.md`, that a guest implements from the prose —
/// a `wasm32-wasip1` guest cannot link `autumn-web` at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub(crate) enum HostFrame {
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
pub(crate) enum GuestFrame {
    /// The plugin's answer.
    Response(SandboxResponse),
    /// The plugin reporting that it could not answer. The host turns this into
    /// a 5xx on the plugin's prefix; it is never a host failure.
    Error {
        /// What went wrong, for the log.
        detail: String,
    },
}

/// Lower-case header names, keep only the ones [`ALLOWED_REQUEST_HEADERS`]
/// names, and sort by name with insertion order preserved within a name.
///
/// Sorting makes the frame a deterministic function of the request, which is
/// what lets an author diff two runs of the same request and see only what
/// changed.
#[must_use]
pub(crate) fn canonicalize_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    // Filtered before anything is allocated, not after. Cloning first and
    // discarding on the next step duplicated every byte of headers that never
    // reach the guest — and once the metadata ceiling stopped charging for
    // those headers (rightly, since they are dropped) nothing else stood
    // between a direct `SandboxHost::run` caller and an arbitrarily large
    // credential being copied in full before being thrown away.
    //
    // The *name* is the same trap one step smaller: lower-casing it to look it
    // up allocates a copy of a string the caller chose the length of, in order
    // to decide it is not wanted. `request_header_allowed` compares the
    // borrowed name case-insensitively, so nothing is allocated until a header
    // has earned it.
    let mut out: Vec<(String, String)> = headers
        .iter()
        .filter(|(name, _)| request_header_allowed(name))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.clone()))
        .collect();
    out.sort_by(|(left, _), (right, _)| left.cmp(right));
    out
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_denied_header_name_is_not_copied_whole_to_be_thrown_away() {
        // `sanitize` lower-cased every name before deciding anything, so a name
        // the allowlist refuses was copied in full on the way to being dropped.
        // The guest chooses that length — it is bounded only by the stdout line
        // ceiling — and the copy is then *kept* in `denied` until the caller
        // logs it, beside the parsed response and the clone `run` holds. The
        // request side already answers this: `request_header_allowed` compares
        // the borrowed name case-insensitively and copies nothing.
        //
        // The name is still recorded, because a plugin reaching for
        // `Set-Cookie` is exactly what an operator wants to see — just as an
        // excerpt rather than as however many megabytes the guest chose.
        let huge = format!("set-cookie{}", "x".repeat(64 * 1024));
        let response = SandboxResponse {
            status: 200,
            headers: vec![(huge.clone(), "v".to_owned())],
            body: Vec::new(),
        };
        let (clean, denied) = response.sanitize("/hello");

        assert!(
            clean.headers.is_empty(),
            "the header must still be stripped"
        );
        assert_eq!(denied.len(), 1, "the strip must still be recorded");
        assert!(
            denied[0].len() < huge.len(),
            "the whole name was carried into the denial: {} bytes",
            denied[0].len(),
        );
        assert!(
            denied[0].starts_with("set-cookie"),
            "the excerpt must still name the header: {:?}",
            denied[0].get(..40),
        );

        // Case-insensitive without allocating to find out: the allowlist is
        // matched against the borrowed name now, so a shouted name is refused
        // exactly as a quiet one is.
        let shouted = SandboxResponse {
            status: 200,
            headers: vec![("SET-COOKIE".to_owned(), "v".to_owned())],
            body: Vec::new(),
        };
        let (clean, denied) = shouted.sanitize("/hello");
        assert!(
            clean.headers.is_empty(),
            "a shouted name must still be refused"
        );
        assert_eq!(
            denied,
            vec!["set-cookie".to_owned()],
            "recorded lower-cased"
        );

        // And an allowed header still survives, whatever case it arrives in —
        // the bound must not become a filter.
        let fine = SandboxResponse {
            status: 200,
            headers: vec![("Content-Type".to_owned(), "text/plain".to_owned())],
            body: Vec::new(),
        };
        let (clean, denied) = fine.sanitize("/hello");
        assert_eq!(clean.headers.len(), 1, "an allowed header must survive");
        assert!(denied.is_empty(), "nothing was denied");
    }

    #[test]
    fn a_redirect_cannot_hide_its_climb_behind_a_space_the_client_trims() {
        // The control-character refusal did not cover U+0020, and the URL
        // parser trims leading and trailing spaces for the same reason it
        // deletes tabs: `/hello/.. ` walks here as the segment `.. `, which
        // climbs nothing, and reaches the client as `/hello/..`, which resolves
        // to `/` — outside the prefix entirely, with a 307/308 replaying the
        // method and body there.
        //
        // Asserted as an equivalence, like the deleted-character cases: the
        // trimmed form and the plain form are the same redirect.
        for smuggled in ["/hello/.. ", "/hello/.. \t", " /hello/.. "] {
            let as_the_client_sees_it: String = smuggled
                .chars()
                .filter(|ch| !matches!(ch, '\t' | '\n' | '\r'))
                .collect();
            let as_the_client_sees_it = as_the_client_sees_it.trim();
            assert_eq!(
                as_the_client_sees_it, "/hello/..",
                "the fixture must be the same redirect once the client trims it",
            );
            assert!(
                !redirect_target_allowed(smuggled, "/hello"),
                "{smuggled:?} was accepted, and resolves outside the prefix at the client",
            );
        }

        // The plain form was already refused, so the assertions above cannot
        // pass vacuously.
        assert!(!redirect_target_allowed("/hello/..", "/hello"));

        // A space is refused anywhere rather than only at the ends, so there is
        // one rule here instead of a second normalisation that has to agree
        // with the client's. A legitimate target percent-encodes it.
        assert!(!redirect_target_allowed("/hello/a b", "/hello"));
        assert!(redirect_target_allowed("/hello/a%20b", "/hello"));
    }

    #[test]
    fn a_redirect_cannot_hide_its_climb_behind_characters_the_client_deletes() {
        // The cases above are refused because of what they say. This one is
        // refused because of what it will say *later*: the URL Standard has the
        // client remove every ASCII tab and newline from the input before
        // parsing it, so the target the guest sends and the target the client
        // resolves are not the same string.
        //
        // `<TAB>..` walks as an ordinary segment name — it climbs nothing, and
        // a check reading the bytes as sent has no reason to object. Delete the
        // tab, as every browser does, and it is `..`. HTTP allows a tab in a
        // field value, so nothing upstream objects either, and a 307/308
        // preserves the method, body and cookies on the way to `/admin`.
        //
        // Asserted as an equivalence rather than a bare refusal, so the test
        // says why: the smuggled form and the plain form are the same redirect.
        let plain = "/hello/../admin";
        for smuggled in [
            "/hello/\t../admin",
            "/hello/\n../admin",
            "/hello/\r../admin",
            "/hello/.\t./admin",
            "/hello\t/../admin",
        ] {
            let as_the_client_sees_it: String = smuggled
                .chars()
                .filter(|ch| !matches!(ch, '\t' | '\n' | '\r'))
                .collect();
            assert_eq!(
                as_the_client_sees_it, plain,
                "the fixture must be the same redirect once the client strips it",
            );
            assert!(
                !redirect_target_allowed(smuggled, "/hello"),
                "{smuggled:?} was accepted, and resolves to {plain} at the client",
            );
        }

        // The plain form was already refused; this is the floor the above is
        // measured against, so a change that stopped refusing it cannot make
        // the assertions above pass vacuously.
        assert!(!redirect_target_allowed(plain, "/hello"));

        // And a target with no control characters is unaffected.
        assert!(redirect_target_allowed("/hello/greet?page=2", "/hello"));
    }

    #[test]
    fn a_redirect_may_not_leave_the_plugin_s_own_prefix() {
        // `Location` is the one allowed header that makes the *client* act, and
        // it acts with the user's credentials. A 307/308 preserves the method,
        // the body and the cookies, so a guest with no session capability could
        // induce an authenticated request to the host's own admin surface.
        for target in [
            "/admin",                     // elsewhere in the host's origin
            "https://evil.example/admin", // another origin entirely
            "//evil.example/admin",       // protocol-relative, same effect
            "/hello/../admin",            // starts inside, climbs out
            "/hello/%2e%2e/admin",        // the same climb, percent-encoded
            "/hello/%2E%2E/admin",        // and in upper case
            "/hello/.%2e/admin",          // and half-encoded
            "/hello/%2e./admin",          // and the other half
            "/helloworld",                // prefix match that is not a segment
            "/hello\\..\\admin",          // backslash as a separator
            "/hello/\t../admin",          // the climb, hidden behind a tab
            "/hello/\n../admin",          // and a line feed
            "/hello/\r../admin",          // and a carriage return
            "/hello/.\t./admin",          // the tab inside the segment itself
            "/hello/.. ",                 // the climb, hidden behind a trailing space
            "/hello/.. \t",               // and behind both at once
        ] {
            let response = SandboxResponse {
                status: 307,
                headers: vec![("location".to_owned(), target.to_owned())],
                body: Vec::new(),
            };
            let (clean, denied) = response.sanitize("/hello");
            assert!(
                !clean
                    .headers
                    .iter()
                    .any(|(name, _)| name.eq_ignore_ascii_case("location")),
                "{target} survived sanitation"
            );
            assert!(
                denied.iter().any(|name| name == "location"),
                "{target} was dropped without being recorded as a denial"
            );
        }

        // And a redirect to its own routes still works — the plugin needs that,
        // and it can already serve whatever it points at.
        for target in [
            "/hello",
            "/hello/greet",
            "/hello/greet?page=2",
            "/hello#top",
        ] {
            let response = SandboxResponse {
                status: 303,
                headers: vec![("location".to_owned(), target.to_owned())],
                body: Vec::new(),
            };
            let (clean, _) = response.sanitize("/hello");
            assert!(
                clean
                    .headers
                    .iter()
                    .any(|(name, _)| name.eq_ignore_ascii_case("location")),
                "{target} is inside the prefix and must be allowed"
            );
        }
    }
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
        let line = to_line(&HostFrame::request(
            &request(),
            &[SandboxCapability::HttpRequest],
        ))
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
        // The list is closed, so an unrecognised header does not cross either —
        // which is the property that survives the next proxy vendor inventing
        // an identity header nobody has heard of.
        assert!(!json.contains("x-trace"), "{json}");
        let HostFrame::Request { headers, .. } = frame;
        assert_eq!(headers, vec![("accept".to_owned(), "text/html".to_owned())]);
    }

    #[test]
    fn request_headers_are_lower_cased_and_sorted_with_stable_duplicates() {
        let mut req = request();
        req.headers = vec![
            ("User-Agent".to_owned(), "curl".to_owned()),
            ("Accept".to_owned(), "first".to_owned()),
            ("accept".to_owned(), "second".to_owned()),
        ];
        let HostFrame::Request { headers, .. } =
            HostFrame::request(&req, &[SandboxCapability::HttpRequest]);
        assert_eq!(
            headers,
            vec![
                ("accept".to_owned(), "first".to_owned()),
                ("accept".to_owned(), "second".to_owned()),
                ("user-agent".to_owned(), "curl".to_owned()),
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
    fn an_unknown_field_on_a_response_frame_is_refused() {
        // The response is the one frame a guest controls, so it is the one that
        // most needs to refuse what it does not understand: a v2 guest sending
        // a field this version drops would be answered as if it had not.
        let line = r#"{"op":"response","status":200,"headers":[],"body_b64":"","stream":true}"#;
        let err = from_line::<GuestFrame>(line).expect_err("an unknown field must be refused");
        assert!(err.to_string().contains("stream"), "{err}");
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
        // 1xx is refused too: an informational response is the HTTP stack's to
        // send, and hyper treats one coming back from a service as an error.
        for status in [0u16, 99, 100, 101, 199, 600, 1000] {
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
        for status in [200u16, 204, 404, 500, 599] {
            let response = SandboxResponse {
                status,
                headers: vec![],
                body: vec![],
            };
            assert!(
                response.validate().is_ok(),
                "status {status} must be allowed"
            );
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
        let (clean, denied) = response.sanitize("/hello");
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
            let (clean, denied) = response.sanitize("/hello");
            assert_eq!(denied, vec![name.to_owned()], "{name} must be stripped");
            assert!(clean.headers.is_empty());
        }
    }

    #[test]
    fn a_proxy_injected_identity_header_never_reaches_the_guest() {
        // A denylist of credential headers is a losing game: an authenticating
        // proxy injects names nobody standardised — `cf-access-jwt-assertion`,
        // `x-forwarded-user`, `x-amzn-oidc-data`, `x-ms-client-principal` — and
        // each one is a credential the sandbox promised would not cross.
        let mut req = request();
        req.headers = vec![
            ("Accept".to_owned(), "text/plain".to_owned()),
            ("Cf-Access-Jwt-Assertion".to_owned(), "ey.J.hdr".to_owned()),
            ("X-Forwarded-User".to_owned(), "ada".to_owned()),
            ("X-Amzn-Oidc-Data".to_owned(), "ey.J.aws".to_owned()),
            ("X-Ms-Client-Principal".to_owned(), "ey.J.ms".to_owned()),
            ("X-Goog-Iap-Jwt-Assertion".to_owned(), "ey.J.g".to_owned()),
            ("X-Trace".to_owned(), "abc".to_owned()),
        ];
        let HostFrame::Request { headers, .. } =
            HostFrame::request(&req, &[SandboxCapability::HttpRequest]);
        assert_eq!(
            headers,
            vec![("accept".to_owned(), "text/plain".to_owned())],
            "only ordinary request metadata may cross"
        );
    }

    #[test]
    fn the_request_allowlist_carries_what_a_handler_needs() {
        let mut req = request();
        req.headers = vec![
            ("accept".to_owned(), "application/json".to_owned()),
            ("accept-language".to_owned(), "en".to_owned()),
            ("content-type".to_owned(), "application/json".to_owned()),
            ("if-none-match".to_owned(), "\"abc\"".to_owned()),
            ("range".to_owned(), "bytes=0-99".to_owned()),
            ("user-agent".to_owned(), "curl".to_owned()),
        ];
        let HostFrame::Request { headers, .. } =
            HostFrame::request(&req, &[SandboxCapability::HttpRequest]);
        assert_eq!(headers.len(), 6, "{headers:?}");
    }

    #[test]
    fn a_second_content_type_cannot_smuggle_a_document_past_the_allowlist() {
        // The allowlist exists because a response is served from the host's
        // own origin, so this slice serves data and not documents. Checking
        // only the *first* content-type does not enforce that: header values
        // are appended, so both reach the client, and a proxy or client that
        // takes the last one sees the document.
        let response = SandboxResponse {
            status: 200,
            headers: vec![
                ("content-type".to_owned(), "text/plain".to_owned()),
                ("content-type".to_owned(), "text/html".to_owned()),
            ],
            body: b"<script>alert(1)</script>".to_vec(),
        };
        assert!(
            response.refused_content_type().is_some(),
            "a second content-type crossed the boundary unchecked"
        );

        // And the reverse order, so this is not passing by luck of ordering.
        let response = SandboxResponse {
            status: 200,
            headers: vec![
                ("content-type".to_owned(), "text/html".to_owned()),
                ("content-type".to_owned(), "text/plain".to_owned()),
            ],
            body: Vec::new(),
        };
        assert!(response.refused_content_type().is_some());

        // One allowed type is still allowed.
        let response = SandboxResponse {
            status: 200,
            headers: vec![("content-type".to_owned(), "text/plain".to_owned())],
            body: Vec::new(),
        };
        assert_eq!(response.refused_content_type(), None);
    }

    #[test]
    fn a_plugin_cannot_reach_a_reverse_proxy_through_an_x_header() {
        // `X-Accel-Redirect` (nginx) and `X-Sendfile` (Apache) make the proxy
        // serve an internal URI or a local file. An allowlist that ends in "and
        // anything starting with x-" would hand a filesystem-free plugin the
        // filesystem, one hop upstream.
        for name in [
            "x-accel-redirect",
            "x-sendfile",
            "x-accel-buffering",
            "x-autumn-sandboxed",
            "x-content-type-options",
            "x-whatever",
        ] {
            assert!(
                !response_header_allowed(name),
                "{name} must not be settable by a plugin"
            );
        }
        for name in ["content-type", "cache-control", "etag", "location", "vary"] {
            assert!(response_header_allowed(name), "{name} must be settable");
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
    fn a_response_frame_carrying_more_headers_than_the_cap_is_refused_while_parsing() {
        // The refusal has to happen *during* deserialization: after it, the
        // vector this guards against already exists. `["",""]` is eight bytes
        // on the wire and a 48-byte `(String, String)` in memory, so a frame at
        // the stdout budget expands roughly sixfold — and `sanitize` then
        // strips these, so `check_size` never charges for a byte of it.
        let headers = std::iter::repeat_n(r#"["",""]"#, MAX_RESPONSE_HEADERS + 1)
            .collect::<Vec<_>>()
            .join(",");
        let line = format!(r#"{{"status":200,"headers":[{headers}],"body_b64":""}}"#);

        let parsed = from_line::<SandboxResponse>(&line);
        assert!(
            parsed.is_err(),
            "a frame past the header cap must not deserialize"
        );

        // And one at the cap still parses: the bound is a ceiling, not a
        // narrowing of what a working plugin may answer with.
        let headers = std::iter::repeat_n(r#"["x","y"]"#, MAX_RESPONSE_HEADERS)
            .collect::<Vec<_>>()
            .join(",");
        let line = format!(r#"{{"status":200,"headers":[{headers}],"body_b64":""}}"#);
        let parsed = from_line::<SandboxResponse>(&line).expect("a frame at the cap parses");
        assert_eq!(parsed.headers.len(), MAX_RESPONSE_HEADERS);
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
