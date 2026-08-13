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
//! segments, and database result rows. A body that *declares* structure but
//! does not parse as it — malformed JSON, or the prefix teed before a handler
//! abandoned the read — is not copied at all: with no keys to match on there
//! is nothing to mask, so it is recorded as skipped with a note. See
//! `docs/guide/failure-capsules.md`.

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
    ///
    /// Every masked value is kept, however short. The length floor lives in
    /// [`mask_echoes`] alone, where it earns its keep: masking a three-letter
    /// value *inside* free-form prose would shred unrelated words. Bind masking
    /// compares whole values, so there is nothing to shred — and a three-digit
    /// CVV or a short PIN the filter removed from the request must not travel
    /// on in the tape simply for being short.
    pub fn insert(&mut self, value: &[u8]) {
        if value.is_empty() {
            return;
        }
        self.0.insert(value.to_vec());
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

/// Shortest value worth looking for *inside* free-form text (see
/// [`mask_echoes`]); whole-value bind masking has no such floor.
const MIN_ECHO_LEN: usize = 4;

/// Build the capsule's request record, masking every sensitive value.
///
/// Takes the request head and the body separately, because the capture layer
/// obtains them at different times: the head up front, the body as the handler
/// streams it.
///
/// Returns the redacted record, the set of pre-mask values for [`mask_binds`]
/// and [`mask_echoes`], and any notes about a body redaction refused to copy.
#[must_use]
pub fn redact_request(
    raw: &RawRequest,
    raw_body: &CapturedBody,
    filter: &ParameterFilter,
) -> (CapsuleRequest, RedactedValues, Vec<String>) {
    let mut values = RedactedValues::default();
    let mut keys = BTreeSet::new();
    let mut notes = Vec::new();

    let headers = redact_headers(&raw.headers, filter, &mut values, &mut keys);
    let uri = redact_uri(&raw.uri, filter, &mut values, &mut keys);
    let body = redact_body(
        &raw.headers,
        raw_body,
        filter,
        &mut values,
        &mut keys,
        &mut notes,
    );

    let request = CapsuleRequest {
        method: raw.method.clone(),
        uri,
        route: raw.route.clone(),
        http_version: format!("{:?}", raw.version),
        headers,
        body,
        redacted_keys: keys.into_iter().collect(),
        // Filled in by persist from the capture scope; redaction only sees
        // the request head.
        client_addr: None,
    };
    (request, values, notes)
}

/// Replace every occurrence of a redacted request value inside `text` with the
/// filtered placeholder.
///
/// Where [`mask_binds`] compares whole values, this looks for them *inside* a
/// string: a handler that fails with `"could not store password=hunter2"` — or
/// panics with the submitted value in its payload — would otherwise write back
/// out, in the capsule's outcome, exactly what redaction removed from the
/// request. Same minimum-length rule as the rest of the echo set, so short
/// values cannot shred unrelated prose — a floor that applies here and nowhere
/// else.
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

/// Headers the capsule masks unconditionally, over and above the filter.
///
/// These carry credentials by construction, but their names do not normalize
/// onto any `DEFAULT_FILTER_KEYS` entry (`proxy-authorization` →
/// `proxyauthorization` ≠ `authorization`), so an exact-match filter would
/// copy them verbatim. A capsule is a production-data artifact; a standard
/// credential header must never depend on app configuration to be masked.
const ALWAYS_SENSITIVE_HEADERS: &[&str] = &["proxy-authorization"];

/// Whether a header must be masked out of the capsule.
fn header_is_sensitive(name: &str, filter: &ParameterFilter) -> bool {
    filter.matches_key(name) || ALWAYS_SENSITIVE_HEADERS.contains(&name)
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
        if header_is_sensitive(&name, filter) {
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

/// `redacted_keys` entry recording that a body was masked wholesale because it
/// did not parse as the structure it declared.
const UNPARSEABLE_BODY_KEY: &str = "body:<unparseable>";

/// Note recorded when a body declared JSON but did not parse as JSON.
const UNPARSEABLE_JSON_NOTE: &str = "request body declared a JSON content type but did not parse as JSON; it was masked out of \
     the capsule rather than copied verbatim, because there are no keys to redact on";

/// Note recorded when a body declared a multipart form.
const MULTIPART_BODY_NOTE: &str = "request body declared a multipart content type, which this slice does not parse; it was \
     masked out of the capsule rather than copied verbatim, because its fields (and any uploaded \
     file) cannot be redacted without parsing them";

/// Note recorded when a body declared a urlencoded form but did not parse as
/// one.
const UNPARSEABLE_FORM_NOTE: &str = "request body declared a urlencoded form but did not parse as one; it was masked out of the \
     capsule rather than copied verbatim, because there are no keys to redact on";

/// Redact the request body according to its content type.
fn redact_body(
    headers: &axum::http::HeaderMap,
    raw_body: &CapturedBody,
    filter: &ParameterFilter,
    values: &mut RedactedValues,
    keys: &mut BTreeSet<String>,
    notes: &mut Vec<String>,
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

    if content_type.contains("json") {
        // A body that claims to be JSON but is not one — malformed, or the
        // prefix the tap copied before the handler abandoned the read — has no
        // keys for the filter to match, so copying it verbatim would write
        // `{"password":"secret",` straight into the capsule. Mask it instead:
        // an unparsed body also contributes nothing to `values`, so nothing
        // downstream could catch the echo either.
        let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(bytes) else {
            return unparseable_body(bytes, UNPARSEABLE_JSON_NOTE, keys, notes);
        };
        let scrubbed = scrub_value(&parsed, filter, "body", values, keys);
        return serde_json::to_string(&scrubbed).map_or(CapsuleBody::Absent, CapsuleBody::Text);
    }

    if content_type.contains("application/x-www-form-urlencoded") {
        // Same conservatism, one step earlier: the urlencoded parser is lossy
        // and accepts anything, so a JSON document sent under a form content
        // type would come back as one giant key that matches no filter and is
        // then re-serialized verbatim. `form_pairs` says whether this really
        // is a form before any of it is copied.
        let Some(pairs) = form_pairs(bytes) else {
            return unparseable_body(bytes, UNPARSEABLE_FORM_NOTE, keys, notes);
        };
        let mut serializer = url::form_urlencoded::Serializer::new(String::new());
        for (key, value) in pairs {
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

    // A multipart body *has* key structure — a file upload's form fields are
    // exactly the kind of thing `filter_parameters` names — but this slice does
    // not parse multipart, and copying it verbatim would write every part
    // through unredacted, password field and uploaded file alike. Skip it: the
    // capsule records the length and says why, rather than becoming the one
    // place a submitted secret survives in the clear.
    if content_type.starts_with("multipart/") || content_type.contains("multipart/form-data") {
        return unparseable_body(bytes, MULTIPART_BODY_NOTE, keys, notes);
    }

    // Anything else has no key structure to match on, so it is copied
    // verbatim — see the module docs on what redaction does not cover.
    std::str::from_utf8(bytes).map_or_else(
        |_| CapsuleBody::Base64(STANDARD.encode(bytes)),
        |text| CapsuleBody::Text(text.to_owned()),
    )
}

/// Record a body that declared a structure it does not have, without copying
/// any of it.
///
/// Reusing [`CapsuleBody::Skipped`] says the one thing a reader needs to know —
/// the bytes were deliberately not carried, only their length — and replay
/// already handles it by sending an empty body with a warning. The note says
/// *why* this one was skipped.
fn unparseable_body(
    bytes: &[u8],
    note: &'static str,
    keys: &mut BTreeSet<String>,
    notes: &mut Vec<String>,
) -> CapsuleBody {
    keys.insert(UNPARSEABLE_BODY_KEY.to_owned());
    notes.push(note.to_owned());
    CapsuleBody::Skipped {
        declared_len: Some(bytes.len()),
    }
}

/// Decode a urlencoded form body, or `None` if it is not one.
///
/// `url::form_urlencoded::parse` never fails, so this validates the shape
/// first: UTF-8, every `&`-separated segment a `key=value` pair, and every key
/// made of characters a client would not have had to percent-encode. Rejecting
/// costs a captured body; accepting a non-form costs an unredactable copy of
/// it, so the doubt is resolved towards rejecting.
fn form_pairs(bytes: &[u8]) -> Option<Vec<(String, String)>> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut pairs = 0usize;
    for segment in text.split('&') {
        // A trailing or doubled separator is sloppy but still a form.
        if segment.is_empty() {
            continue;
        }
        let (key, _) = segment.split_once('=')?;
        if key.is_empty() || key.contains(is_not_form_key_char) {
            return None;
        }
        pairs += 1;
    }
    if pairs == 0 {
        return None;
    }
    Some(
        url::form_urlencoded::parse(bytes)
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect(),
    )
}

/// Whether a character disqualifies the string it appears in from being a
/// urlencoded form key.
///
/// Deliberately narrow: it names the characters a form key cannot carry
/// unencoded (structural JSON punctuation, quotes, whitespace, controls)
/// rather than trying to enumerate the ones it can, so unencoded-but-harmless
/// keys such as `user[email]` still pass.
const fn is_not_form_key_char(c: char) -> bool {
    c.is_ascii_control()
        || c.is_whitespace()
        || matches!(c, '{' | '}' | '"' | '\'' | '<' | '>' | '\\')
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
        let (request, values, _) = redact_with_notes(builder, body, filter);
        (request, values)
    }

    #[allow(
        clippy::needless_pass_by_value,
        reason = "fixture bodies read better handed over than borrowed at every call site"
    )]
    fn redact_with_notes(
        builder: axum::http::request::Builder,
        body: CapturedBody,
        filter: &ParameterFilter,
    ) -> (CapsuleRequest, RedactedValues, Vec<String>) {
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

    /// A multipart body is the one shape that carries both form fields and
    /// file contents, and this slice cannot parse it — so it must never be
    /// copied. Copying it verbatim made the capsule the one place a submitted
    /// password survived unmasked.
    #[test]
    fn a_multipart_body_is_never_copied_into_the_capsule() {
        let body = "--X\r\nContent-Disposition: form-data; name=\"password\"\r\n\r\n\
                    hunter2-in-the-clear\r\n--X--\r\n";
        let (request, _values, notes) = redact_with_notes(
            Request::post("/upload")
                .header(header::CONTENT_TYPE, "multipart/form-data; boundary=X"),
            CapturedBody::Buffered(Bytes::from_static(body.as_bytes())),
            &filter_with(&[]),
        );

        match &request.body {
            CapsuleBody::Skipped { declared_len } => {
                assert_eq!(*declared_len, Some(body.len()));
            }
            other => panic!("a multipart body must be skipped, got {other:?}"),
        }
        let rendered = serde_json::to_string(&request).expect("request serializes");
        assert!(
            !rendered.contains("hunter2-in-the-clear") && !rendered.contains("password"),
            "no part of a multipart body may reach the capsule: {rendered}"
        );
        assert!(
            notes.iter().any(|note| note.contains("multipart")),
            "the capsule must say why the body is missing, got {notes:?}"
        );
    }

    /// The echo set has no length floor: a three-character CVV the filter
    /// removed from the request must not travel on in the tape. The floor
    /// belongs to substring masking of prose, and to nothing else.
    #[test]
    fn a_short_masked_value_still_masks_its_bind() {
        let (_request, values) = redact(
            Request::post("/pay").header(header::CONTENT_TYPE, "application/json"),
            CapturedBody::Buffered(Bytes::from_static(b"{\"cvv\":\"123\",\"amount\":10}")),
            &filter_with(&["cvv"]),
        );

        let mut binds = vec![
            BindValue::Value(b"123".to_vec()),
            BindValue::Value(b"10".to_vec()),
        ];
        mask_binds(&mut binds, &values);
        assert_eq!(
            binds.first(),
            Some(&BindValue::Masked),
            "a short value the filter removed must still be masked out of the binds"
        );
        assert_eq!(
            binds.get(1),
            Some(&BindValue::Value(b"10".to_vec())),
            "unrelated binds are untouched"
        );

        // …but short values are still not hunted for inside free-form prose,
        // where they would shred unrelated words.
        assert_eq!(
            mask_echoes("the 123rd attempt", &values),
            "the 123rd attempt",
            "substring masking keeps its length floor"
        );
    }

    #[test]
    fn proxy_authorization_is_masked_unconditionally() {
        let (request, values) = redact(
            Request::get("/x").header("proxy-authorization", "Basic cHJveHk6c2VjcmV0cGFzcw=="),
            CapturedBody::Absent,
            &filter_with(&[]),
        );

        assert_eq!(
            header_value(&request, "proxy-authorization"),
            FILTERED_PLACEHOLDER,
            "a standard credential header must not depend on app config to be masked"
        );
        assert!(
            values.contains(b"Basic cHJveHk6c2VjcmV0cGFzcw=="),
            "the masked value must join the echo set"
        );
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
    fn malformed_json_body_is_masked_not_copied_verbatim() {
        let (request, values, notes) = redact_with_notes(
            Request::post("/users").header(header::CONTENT_TYPE, "application/json"),
            CapturedBody::Buffered(Bytes::from_static(br#"{"password":"hunter2secret", oops"#)),
            &filter_with(&[]),
        );

        assert_eq!(
            request.body,
            CapsuleBody::Skipped {
                declared_len: Some(33)
            },
            "a body that declared JSON but did not parse must never be copied verbatim, got {:?}",
            request.body
        );
        assert!(
            values.is_empty(),
            "an unparsed body yields no keys, so it contributes nothing to the echo set"
        );
        assert!(
            request
                .redacted_keys
                .contains(&UNPARSEABLE_BODY_KEY.to_owned()),
            "redacted_keys must record that the body was masked, got {:?}",
            request.redacted_keys
        );
        assert_eq!(notes, vec![UNPARSEABLE_JSON_NOTE.to_owned()]);
    }

    #[test]
    fn truncated_json_prefix_is_masked() {
        // The body tap copies bytes as the handler reads them, so a request
        // that fails mid-stream leaves a prefix that cannot parse.
        let (request, _, notes) = redact_with_notes(
            Request::post("/users").header(header::CONTENT_TYPE, "application/json; charset=utf-8"),
            CapturedBody::Buffered(Bytes::from_static(br#"{"token":"deep-secret-value","#)),
            &filter_with(&[]),
        );

        assert!(
            matches!(request.body, CapsuleBody::Skipped { .. }),
            "a truncated JSON prefix must be masked, got {:?}",
            request.body
        );
        assert_eq!(notes, vec![UNPARSEABLE_JSON_NOTE.to_owned()]);
    }

    #[test]
    fn body_that_is_not_a_form_under_a_form_content_type_is_masked() {
        // The urlencoded parser is lossy: it accepts anything, turning a JSON
        // document into one giant key that no filter can match.
        let (request, _, notes) = redact_with_notes(
            Request::post("/users")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded"),
            CapturedBody::Buffered(Bytes::from_static(br#"{"password":"hunter2secret"}"#)),
            &filter_with(&[]),
        );

        assert!(
            matches!(request.body, CapsuleBody::Skipped { .. }),
            "a body that declared a form but did not parse as one must be masked, got {:?}",
            request.body
        );
        assert_eq!(notes, vec![UNPARSEABLE_FORM_NOTE.to_owned()]);
    }

    #[test]
    fn valid_structured_bodies_still_parse_without_a_note() {
        let (request, values, notes) = redact_with_notes(
            Request::post("/users").header(header::CONTENT_TYPE, "application/json"),
            CapturedBody::Buffered(Bytes::from_static(
                br#"{"password":"hunter2secret","keep":"visible"}"#,
            )),
            &filter_with(&[]),
        );
        let CapsuleBody::Text(body) = &request.body else {
            panic!("valid JSON must still be parsed and scrubbed, got {request:?}");
        };
        assert!(!body.contains("hunter2secret") && body.contains("visible"));
        assert!(values.contains(b"hunter2secret"));
        assert!(
            notes.is_empty(),
            "a parsed body needs no note, got {notes:?}"
        );

        let (request, _, notes) = redact_with_notes(
            Request::post("/users")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded"),
            CapturedBody::Buffered(Bytes::from_static(b"email=ada%40example.com&flag=&page=2")),
            &filter_with(&[]),
        );
        assert!(
            matches!(&request.body, CapsuleBody::Text(body) if body.contains("page=2")),
            "an ordinary form body — empty values included — must still parse, got {:?}",
            request.body
        );
        assert!(
            notes.is_empty(),
            "a parsed body needs no note, got {notes:?}"
        );
    }

    #[test]
    fn unstructured_bodies_are_unaffected() {
        // No declared structure means nothing claimed to parse, so the existing
        // verbatim copy stands.
        let (request, _, notes) = redact_with_notes(
            Request::post("/notes").header(header::CONTENT_TYPE, "text/plain"),
            CapturedBody::Buffered(Bytes::from_static(b"{not json, not a form")),
            &filter_with(&[]),
        );

        assert_eq!(
            request.body,
            CapsuleBody::Text("{not json, not a form".to_owned())
        );
        assert!(notes.is_empty());
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
