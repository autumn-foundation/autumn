//! Redaction of everything a capsule copies off the wire.
//!
//! A capsule holds a real production request, so it is only safe to write once
//! the sensitive parts are gone. This module masks headers, query parameters
//! and structured bodies through the same
//! [`ParameterFilter`](crate::log::filter::ParameterFilter) the access log and
//! the dev error page use, so one `[log] filter_parameters` list governs every
//! place Autumn writes request data down.
//!
//! Masking also feeds forward: every value it removes is retained in a
//! [`RedactedValues`] set so [`mask_binds`] can blank any SQL bind parameter
//! that echoes one of them. A password redacted out of the request body must
//! not reappear in the `INSERT` that stored it.
//!
//! What is **not** masked: unstructured bodies (no keys to match on), URL path
//! segments, and database result rows. See `docs/guide/failure-capsules.md`.

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
    )
)]

use std::collections::BTreeSet;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;

use crate::capsule::schema::{BindValue, CapsuleBody, CapsuleRequest};
use crate::log::filter::{FILTERED_PLACEHOLDER, ParameterFilter};

/// The unredacted request *head* the capture layer snapshotted, held in memory
/// until the request either succeeds (and the snapshot is dropped) or fails
/// (and this is redacted into a [`CapsuleRequest`]).
///
/// The body is not part of the snapshot: it is copied while the handler reads
/// it (see [`CaptureScope::captured_body`](crate::capsule::CaptureScope::captured_body))
/// and composed with this head at persist time.
#[derive(Debug, Clone)]
pub struct RawRequest {
    /// HTTP method.
    pub method: String,
    /// Full request target, query string included.
    pub uri: axum::http::Uri,
    /// Negotiated HTTP version.
    pub version: axum::http::Version,
    /// Request headers, verbatim.
    pub headers: axum::http::HeaderMap,
    /// Matched route template, when routing had already resolved one.
    pub route: Option<String>,
}

/// The request body as the capture layer obtained it.
#[derive(Debug, Clone)]
pub enum CapturedBody {
    /// No body was present, or none of it had been read when the request
    /// failed.
    Absent,
    /// The body was copied as the handler read it.
    Buffered(Bytes),
    /// The body was over the cap and deliberately not copied.
    Skipped {
        /// `Content-Length` the client declared, when it declared one.
        declared_len: Option<usize>,
    },
}

/// Values redaction removed, kept so bind parameters echoing them can be
/// masked too.
///
/// Compared byte-wise: a bind carrying exactly the pre-mask bytes is masked,
/// anything else (a hash, a truncation, a re-encoding) is not. This is a
/// best-effort echo check, not a general secret scanner.
#[derive(Debug, Clone, Default)]
pub struct RedactedValues(BTreeSet<Vec<u8>>);

impl RedactedValues {
    /// Record a value that was masked out of the request.
    pub fn insert(&mut self, value: &[u8]) {
        // A one- or two-byte "secret" would mask half the binds in the tape for
        // no security benefit, so only substantial values participate.
        if value.len() >= MIN_ECHO_LEN {
            self.0.insert(value.to_vec());
        }
    }

    /// Whether these bytes were masked out of the request.
    #[must_use]
    pub fn contains(&self, value: &[u8]) -> bool {
        self.0.contains(value)
    }

    /// Whether anything was masked.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The recorded values, longest first.
    ///
    /// Longest-first matters for substring masking: masking `"hunter2"` before
    /// `"hunter2secret"` would leave the tail of the longer secret behind.
    fn longest_first(&self) -> Vec<&[u8]> {
        let mut values: Vec<&[u8]> = self.0.iter().map(Vec::as_slice).collect();
        values.sort_by_key(|value| std::cmp::Reverse(value.len()));
        values
    }
}

/// Shortest value worth echo-matching against bind parameters.
const MIN_ECHO_LEN: usize = 4;

/// Build the capsule's request record, masking every sensitive value.
///
/// Takes the request head and the body separately, because the capture layer
/// obtains them at different times: the head up front, the body as the handler
/// streams it.
///
/// Returns the redacted record plus the set of pre-mask values, for
/// [`mask_binds`] and [`mask_echoes`].
#[must_use]
pub fn redact_request(
    raw: &RawRequest,
    raw_body: &CapturedBody,
    filter: &ParameterFilter,
) -> (CapsuleRequest, RedactedValues) {
    let mut values = RedactedValues::default();
    let mut keys = BTreeSet::new();

    let headers = redact_headers(&raw.headers, filter, &mut values, &mut keys);
    let uri = redact_uri(&raw.uri, filter, &mut values, &mut keys);
    let body = redact_body(&raw.headers, raw_body, filter, &mut values, &mut keys);

    let request = CapsuleRequest {
        method: raw.method.clone(),
        uri,
        route: raw.route.clone(),
        http_version: format!("{:?}", raw.version),
        headers,
        body,
        redacted_keys: keys.into_iter().collect(),
    };
    (request, values)
}

/// Replace every occurrence of a redacted request value inside `text` with the
/// filtered placeholder.
///
/// Where [`mask_binds`] compares whole values, this looks for them *inside* a
/// string: a handler that fails with `"could not store password=hunter2"` — or
/// panics with the submitted value in its payload — would otherwise write back
/// out, in the capsule's outcome, exactly what redaction removed from the
/// request. Same minimum-length rule as the rest of the echo set, so short
/// values cannot shred unrelated prose.
#[must_use]
pub fn mask_echoes(text: &str, redacted: &RedactedValues) -> String {
    if redacted.is_empty() || text.is_empty() {
        return text.to_owned();
    }
    let mut masked = text.to_owned();
    for value in redacted.longest_first() {
        // Only values that were UTF-8 to begin with can appear in a message;
        // binary secrets are handled by `mask_binds`.
        let Ok(needle) = std::str::from_utf8(value) else {
            continue;
        };
        if needle.len() >= MIN_ECHO_LEN && masked.contains(needle) {
            masked = masked.replace(needle, FILTERED_PLACEHOLDER);
        }
    }
    masked
}

/// Mask any bind parameter whose bytes exactly echo a redacted request value.
pub fn mask_binds(binds: &mut [BindValue], redacted: &RedactedValues) {
    if redacted.is_empty() {
        return;
    }
    for bind in binds {
        if let BindValue::Value(bytes) = bind
            && redacted.contains(bytes)
        {
            *bind = BindValue::Masked;
        }
    }
}

/// Copy the headers in wire order, replacing sensitive values.
fn redact_headers(
    headers: &axum::http::HeaderMap,
    filter: &ParameterFilter,
    values: &mut RedactedValues,
    keys: &mut BTreeSet<String>,
) -> Vec<(String, String)> {
    let mut out = Vec::with_capacity(headers.len());
    for (name, value) in headers {
        let name = name.as_str().to_owned();
        if filter.matches_key(&name) {
            values.insert(value.as_bytes());
            keys.insert(format!("header:{name}"));
            out.push((name, FILTERED_PLACEHOLDER.to_owned()));
        } else {
            let value = value.to_str().unwrap_or(NON_UTF8_PLACEHOLDER).to_owned();
            out.push((name, value));
        }
    }
    out
}

/// Placeholder for a header value that is not valid UTF-8, mirroring the dev
/// error page's rendering of the same case.
const NON_UTF8_PLACEHOLDER: &str = "<non-utf8>";

/// Re-serialize the request target with sensitive query parameters masked.
fn redact_uri(
    uri: &axum::http::Uri,
    filter: &ParameterFilter,
    values: &mut RedactedValues,
    keys: &mut BTreeSet<String>,
) -> String {
    let Some(query) = uri.query() else {
        return uri.to_string();
    };
    let pairs: Vec<(String, String)> = url::form_urlencoded::parse(query.as_bytes())
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    if pairs.is_empty() {
        return uri.to_string();
    }

    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in pairs {
        if key_is_sensitive(&key, filter) {
            values.insert(value.as_bytes());
            keys.insert(format!("query:{key}"));
            serializer.append_pair(&key, FILTERED_PLACEHOLDER);
        } else {
            serializer.append_pair(&key, &value);
        }
    }
    let redacted_query = serializer.finish();
    let path = uri.path();
    format!("{path}?{redacted_query}")
}

/// Whether a form/query key names a sensitive value.
///
/// Bracket and dot notation are expanded first so `user[password]` matches on
/// its `password` leaf, the same expansion the dev error page applies before
/// scrubbing a form body.
fn key_is_sensitive(key: &str, filter: &ParameterFilter) -> bool {
    key_segments(key)
        .iter()
        .any(|segment| filter.matches_key(segment))
}

/// Split a form key on bracket or dot notation into path segments.
fn key_segments(key: &str) -> Vec<String> {
    if let Some((head, rest)) = key.split_once('[') {
        let mut parts = vec![head.to_owned()];
        for segment in rest.split('[') {
            let segment = segment.trim_end_matches(']');
            if !segment.is_empty() {
                parts.push(segment.to_owned());
            }
        }
        parts
    } else if key.contains('.') {
        key.split('.')
            .filter(|segment| !segment.is_empty())
            .map(str::to_owned)
            .collect()
    } else {
        vec![key.to_owned()]
    }
}

/// Redact the request body according to its content type.
fn redact_body(
    headers: &axum::http::HeaderMap,
    raw_body: &CapturedBody,
    filter: &ParameterFilter,
    values: &mut RedactedValues,
    keys: &mut BTreeSet<String>,
) -> CapsuleBody {
    let bytes = match raw_body {
        CapturedBody::Absent => return CapsuleBody::Absent,
        CapturedBody::Skipped { declared_len } => {
            return CapsuleBody::Skipped {
                declared_len: *declared_len,
            };
        }
        CapturedBody::Buffered(bytes) => bytes,
    };
    if bytes.is_empty() {
        return CapsuleBody::Absent;
    }

    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();

    if content_type.contains("json")
        && let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(bytes)
    {
        let scrubbed = scrub_value(&parsed, filter, "body", values, keys);
        return serde_json::to_string(&scrubbed).map_or(CapsuleBody::Absent, CapsuleBody::Text);
    }

    if content_type.contains("application/x-www-form-urlencoded") {
        let mut serializer = url::form_urlencoded::Serializer::new(String::new());
        for (key, value) in url::form_urlencoded::parse(bytes) {
            if key_is_sensitive(&key, filter) {
                values.insert(value.as_bytes());
                keys.insert(format!("body:{key}"));
                serializer.append_pair(&key, FILTERED_PLACEHOLDER);
            } else {
                serializer.append_pair(&key, &value);
            }
        }
        return CapsuleBody::Text(serializer.finish());
    }

    // Anything else has no key structure to match on, so it is copied
    // verbatim — see the module docs on what redaction does not cover.
    std::str::from_utf8(bytes).map_or_else(
        |_| CapsuleBody::Base64(STANDARD.encode(bytes)),
        |text| CapsuleBody::Text(text.to_owned()),
    )
}

/// Recursively mask sensitive leaves of a JSON body, recording what was hit.
fn scrub_value(
    value: &serde_json::Value,
    filter: &ParameterFilter,
    path: &str,
    values: &mut RedactedValues,
    keys: &mut BTreeSet<String>,
) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (key, child) in map {
                let child_path = format!("{path}.{key}");
                if filter.matches_key(key) {
                    record_masked_value(child, values);
                    keys.insert(child_path);
                    out.insert(
                        key.clone(),
                        serde_json::Value::String(FILTERED_PLACEHOLDER.to_owned()),
                    );
                } else {
                    out.insert(
                        key.clone(),
                        scrub_value(child, filter, &child_path, values, keys),
                    );
                }
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(items) => serde_json::Value::Array(
            items
                .iter()
                .map(|item| scrub_value(item, filter, path, values, keys))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Retain the bytes of a masked JSON leaf so a bind echoing it is masked too.
fn record_masked_value(value: &serde_json::Value, values: &mut RedactedValues) {
    match value {
        serde_json::Value::String(text) => values.insert(text.as_bytes()),
        serde_json::Value::Null => {}
        other => values.insert(other.to_string().as_bytes()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{Request, header};

    fn filter_with(extra: &[&str]) -> ParameterFilter {
        let extra: Vec<String> = extra.iter().map(|k| (*k).to_owned()).collect();
        ParameterFilter::new(&extra, &[])
    }

    #[allow(
        clippy::needless_pass_by_value,
        reason = "fixture bodies read better handed over than borrowed at every call site"
    )]
    fn redact(
        builder: axum::http::request::Builder,
        body: CapturedBody,
        filter: &ParameterFilter,
    ) -> (CapsuleRequest, RedactedValues) {
        let request = builder.body(()).expect("test request builds");
        let (parts, ()) = request.into_parts();
        let raw = RawRequest {
            method: parts.method.as_str().to_owned(),
            uri: parts.uri,
            version: parts.version,
            headers: parts.headers,
            route: None,
        };
        redact_request(&raw, &body, filter)
    }

    fn header_value<'a>(request: &'a CapsuleRequest, name: &str) -> &'a str {
        request
            .headers
            .iter()
            .find(|(key, _)| key == name)
            .map_or_else(
                || panic!("header {name} must be present in the capsule"),
                |(_, value)| value.as_str(),
            )
    }

    #[test]
    fn authorization_and_cookie_headers_are_masked() {
        let (request, values) = redact(
            Request::get("/x")
                .header(header::AUTHORIZATION, "Bearer super-secret-token")
                .header(header::COOKIE, "session=abcdef123456")
                .header(header::ACCEPT, "application/json"),
            CapturedBody::Absent,
            &filter_with(&[]),
        );

        assert_eq!(
            header_value(&request, "authorization"),
            FILTERED_PLACEHOLDER
        );
        assert_eq!(header_value(&request, "cookie"), FILTERED_PLACEHOLDER);
        assert_eq!(
            header_value(&request, "accept"),
            "application/json",
            "non-sensitive headers must survive verbatim"
        );
        assert!(
            request
                .redacted_keys
                .contains(&"header:authorization".to_owned()),
            "redacted_keys must name what was masked, got {:?}",
            request.redacted_keys
        );
        assert!(
            values.contains(b"Bearer super-secret-token"),
            "the pre-mask header value must be retained for bind masking"
        );
    }

    #[test]
    fn set_cookie_and_configured_sensitive_params_are_masked() {
        let (request, _) = redact(
            Request::get("/x")
                .header("set-cookie", "a=b")
                .header("x-tenant-pin", "4321"),
            CapturedBody::Absent,
            &filter_with(&["x-tenant-pin"]),
        );

        assert_eq!(header_value(&request, "set-cookie"), FILTERED_PLACEHOLDER);
        assert_eq!(
            header_value(&request, "x-tenant-pin"),
            FILTERED_PLACEHOLDER,
            "a key added via [log] filter_parameters must be masked here too"
        );
    }

    #[test]
    fn encrypted_column_names_are_masked() {
        // `router.rs` folds `registered_encrypted_column_names()` into the
        // filter it hands the capture layer; simulate that composition.
        let filter = filter_with(&["ssn_encrypted"]);
        let (request, _) = redact(
            Request::post("/x")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-ignored", "1"),
            CapturedBody::Buffered(Bytes::from_static(
                br#"{"ssn_encrypted":"123-45-6789","name":"Ada"}"#,
            )),
            &filter,
        );

        let CapsuleBody::Text(body) = &request.body else {
            panic!(
                "a JSON body must be captured as text, got {:?}",
                request.body
            );
        };
        assert!(
            body.contains(FILTERED_PLACEHOLDER),
            "an encrypted column name must be masked in the body, got {body}"
        );
        assert!(!body.contains("123-45-6789"), "the value must be gone");
        assert!(body.contains("Ada"), "non-sensitive fields must survive");
    }

    #[test]
    fn query_string_params_are_masked() {
        let (request, values) = redact(
            Request::get("/search?q=cats&api_key=abcdef123456&page=2"),
            CapturedBody::Absent,
            &filter_with(&[]),
        );

        assert!(
            request.uri.contains("q=cats") && request.uri.contains("page=2"),
            "harmless query parameters must survive, got {}",
            request.uri
        );
        assert!(
            !request.uri.contains("abcdef123456"),
            "a sensitive query value must never be written to the capsule, got {}",
            request.uri
        );
        assert!(
            request.uri.contains("api_key=%5BFILTERED%5D")
                || request.uri.contains("api_key=[FILTERED]"),
            "the masked parameter must remain present as a placeholder, got {}",
            request.uri
        );
        assert!(values.contains(b"abcdef123456"));
    }

    #[test]
    fn form_body_bracket_keys_are_masked() {
        let (request, values) = redact(
            Request::post("/users")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded"),
            CapturedBody::Buffered(Bytes::from_static(
                b"user%5Bemail%5D=ada%40example.com&user%5Bpassword%5D=hunter2secret",
            )),
            &filter_with(&[]),
        );

        let CapsuleBody::Text(body) = &request.body else {
            panic!(
                "a form body must be captured as text, got {:?}",
                request.body
            );
        };
        assert!(
            !body.contains("hunter2secret"),
            "a bracket-notation password must be masked, got {body}"
        );
        assert!(
            body.contains("ada%40example.com") || body.contains("ada@example.com"),
            "non-sensitive form fields must survive, got {body}"
        );
        assert!(
            request
                .redacted_keys
                .iter()
                .any(|key| key.contains("password")),
            "redacted_keys must name the masked form key, got {:?}",
            request.redacted_keys
        );
        assert!(values.contains(b"hunter2secret"));
    }

    #[test]
    fn json_body_is_masked_recursively() {
        let (request, values) = redact(
            Request::post("/users").header(header::CONTENT_TYPE, "application/json"),
            CapturedBody::Buffered(Bytes::from_static(
                br#"{"a":{"b":[{"token":"deep-secret-value"}]},"keep":"visible"}"#,
            )),
            &filter_with(&[]),
        );

        let CapsuleBody::Text(body) = &request.body else {
            panic!(
                "a JSON body must be captured as text, got {:?}",
                request.body
            );
        };
        assert!(
            !body.contains("deep-secret-value"),
            "redaction must recurse through objects and arrays, got {body}"
        );
        assert!(body.contains("visible"));
        assert!(values.contains(b"deep-secret-value"));
    }

    #[test]
    fn oversized_body_is_skipped_not_consumed() {
        let (request, _) = redact(
            Request::post("/upload").header(header::CONTENT_TYPE, "application/json"),
            CapturedBody::Skipped {
                declared_len: Some(9_000_000),
            },
            &filter_with(&[]),
        );

        assert_eq!(
            request.body,
            CapsuleBody::Skipped {
                declared_len: Some(9_000_000)
            },
            "an oversized body must be recorded as skipped, never partially copied"
        );
    }

    #[test]
    fn binary_body_is_base64_encoded() {
        let (request, _) = redact(
            Request::post("/upload").header(header::CONTENT_TYPE, "application/octet-stream"),
            CapturedBody::Buffered(Bytes::from_static(&[0xFF, 0xFE, 0x00, 0x01])),
            &filter_with(&[]),
        );

        let CapsuleBody::Base64(encoded) = &request.body else {
            panic!(
                "a non-UTF-8 body must be base64-encoded, got {:?}",
                request.body
            );
        };
        assert_eq!(encoded, "//4AAQ==");
    }

    #[test]
    fn outcome_text_echoing_a_redacted_value_is_masked() {
        // A handler that fails while talking about what it was given hands the
        // secret straight back: `redact_request` masked it out of the body, so
        // the outcome must not smuggle it into the capsule.
        let (_, values) = redact(
            Request::post("/users")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded"),
            CapturedBody::Buffered(Bytes::from_static(b"email=ada&password=hunter2secret")),
            &filter_with(&[]),
        );

        let masked = mask_echoes("could not store password=hunter2secret for ada", &values);

        assert!(
            !masked.contains("hunter2secret"),
            "a value redaction removed must not reappear in the outcome, got {masked}"
        );
        assert!(
            masked.contains(FILTERED_PLACEHOLDER),
            "the echo must be replaced by the placeholder, got {masked}"
        );
        assert!(
            masked.contains("for ada"),
            "the rest of the message must survive, got {masked}"
        );
    }

    #[test]
    fn mask_echoes_prefers_the_longest_match_and_ignores_short_values() {
        let mut values = RedactedValues::default();
        values.insert(b"hunter2secret");
        values.insert(b"hunter2");
        // Below `MIN_ECHO_LEN`: never recorded, so it cannot shred prose.
        values.insert(b"ada");

        let masked = mask_echoes("ada tried hunter2secret twice", &values);

        assert_eq!(masked, format!("ada tried {FILTERED_PLACEHOLDER} twice"));
    }

    #[test]
    fn mask_echoes_is_a_no_op_without_redactions() {
        let values = RedactedValues::default();
        assert_eq!(mask_echoes("nothing to hide", &values), "nothing to hide");
    }

    #[test]
    fn bind_matching_a_redacted_value_is_masked() {
        let mut values = RedactedValues::default();
        values.insert(b"hunter2secret");

        let mut binds = vec![
            BindValue::Value(b"hunter2secret".to_vec()),
            BindValue::Value(b"ada@example.com".to_vec()),
            BindValue::Null,
        ];
        mask_binds(&mut binds, &values);

        assert_eq!(
            binds,
            vec![
                BindValue::Masked,
                BindValue::Value(b"ada@example.com".to_vec()),
                BindValue::Null,
            ],
            "a bind echoing a redacted value must be masked; others must survive"
        );
    }
}
