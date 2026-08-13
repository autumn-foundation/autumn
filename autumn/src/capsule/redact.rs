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
        clippy::string_slice,
        clippy::arithmetic_side_effects,
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
        client_host: None,
        client_scheme: None,
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

/// Rewrite the request target with sensitive query parameters masked, leaving
/// every other byte exactly as the client sent it.
fn redact_uri(
    uri: &axum::http::Uri,
    filter: &ParameterFilter,
    values: &mut RedactedValues,
    keys: &mut BTreeSet<String>,
) -> String {
    let Some(query) = uri.query() else {
        return uri.to_string();
    };
    let original = uri.to_string();
    let Some(masked) = mask_raw_urlencoded(query, "query", filter, values, keys) else {
        return original;
    };
    // Splice the masked query onto the original prefix rather than rebuilding
    // from `uri.path()`: an absolute-form target
    // (`https://api.example/items?...`) keeps its scheme and authority, so a
    // proxy-style route that reads the full request target sees the same
    // shape during replay.
    let prefix = original.split_once('?').map_or("", |(prefix, _)| prefix);
    format!("{prefix}?{masked}")
}

/// The on-the-wire spelling a masked pair's value is replaced with — the
/// percent-encoded [`FILTERED_PLACEHOLDER`], so the rewritten string is still
/// a well-formed urlencoded pair list and decodes to `[FILTERED]`.
const FILTERED_PLACEHOLDER_URLENCODED: &str = "%5BFILTERED%5D";

/// Mask the sensitive pairs of a raw `key=value&…` string in place, leaving
/// every byte of every other pair exactly as the client sent it.
///
/// Returns `None` when no pair matched the filter: the caller keeps the
/// original representation untouched, so a handler that inspects the raw
/// string — or verifies a signature computed over it — sees the same bytes
/// during replay that it saw in production. Decoding, and the spelling drift
/// it brings (`%2f` → `%2F`, space → `+`, bare `flag` → `flag=`), happens
/// only to *matched* pairs, which are being rewritten anyway.
///
/// A matched pair contributes **both** value spellings to the echo set: the
/// decoded one (what a handler error usually quotes) and the on-the-wire one
/// (what an error echoing the raw request target or body carries).
fn mask_raw_urlencoded(
    raw: &str,
    context: &str,
    filter: &ParameterFilter,
    values: &mut RedactedValues,
    keys: &mut BTreeSet<String>,
) -> Option<String> {
    let mut changed = false;
    let masked: Vec<String> = raw
        .split('&')
        .map(|raw_pair| {
            let Some((decoded_key, decoded_value)) =
                url::form_urlencoded::parse(raw_pair.as_bytes()).next()
            else {
                // An empty segment (`a=1&&b=2`): sloppy, but the client sent
                // it, so it stays.
                return raw_pair.to_owned();
            };
            if !key_is_sensitive(&decoded_key, filter) {
                return raw_pair.to_owned();
            }
            changed = true;
            keys.insert(format!("{context}:{decoded_key}"));
            if !decoded_value.is_empty() {
                values.insert(decoded_value.as_bytes());
            }
            let (raw_key, raw_value) = raw_pair
                .split_once('=')
                .map_or((raw_pair, ""), |(key, value)| (key, value));
            if !raw_value.is_empty() {
                values.insert(raw_value.as_bytes());
            }
            format!("{raw_key}={FILTERED_PLACEHOLDER_URLENCODED}")
        })
        .collect();
    changed.then(|| masked.join("&"))
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
        let keys_before = keys.len();
        let scrubbed = scrub_value(&parsed, filter, "body", values, keys);
        if keys.len() > keys_before {
            // The echo set holds the *decoded* masked values, but an error
            // that quotes the raw request body carries the on-wire spelling —
            // `"token"` does not contain `token`. Walk the raw text's
            // string literals and retain every spelling whose decoded form
            // was just masked, so `scrub_outcome` catches either.
            retain_raw_json_string_spellings(bytes, values);
        }
        if keys.len() == keys_before {
            // Nothing was masked: keep the exact bytes the client sent.
            // Re-serializing would drift whitespace, number spellings and key
            // order, and a handler that verifies a signature over the raw
            // body would reject the replay of a request it accepted in
            // production. (serde_json just proved the bytes are UTF-8.)
            return std::str::from_utf8(bytes).map_or(CapsuleBody::Absent, |text| {
                CapsuleBody::Text(text.to_owned())
            });
        }
        return serde_json::to_string(&scrubbed).map_or(CapsuleBody::Absent, CapsuleBody::Text);
    }

    if content_type.contains("application/x-www-form-urlencoded") {
        // Same conservatism, one step earlier: the urlencoded parser is lossy
        // and accepts anything, so a JSON document sent under a form content
        // type would come back as one giant key that matches no filter and is
        // then copied verbatim. `form_text` says whether this really is a
        // form before any of it is copied.
        let Some(text) = form_text(bytes) else {
            return unparseable_body(bytes, UNPARSEABLE_FORM_NOTE, keys, notes);
        };
        // The masking splices only the matched pairs, so an untouched form —
        // or the untouched neighbours of a masked field — keeps the exact
        // bytes the client sent, and a signature computed over the raw body
        // still verifies during replay.
        return CapsuleBody::Text(
            mask_raw_urlencoded(text, "body", filter, values, keys)
                .unwrap_or_else(|| text.to_owned()),
        );
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

/// Validate that a body really is a urlencoded form, returning its text.
///
/// `url::form_urlencoded::parse` never fails, so this validates the shape
/// first: UTF-8, every `&`-separated segment a `key=value` pair, and every key
/// made of characters a client would not have had to percent-encode. Rejecting
/// costs a captured body; accepting a non-form costs an unredactable copy of
/// it, so the doubt is resolved towards rejecting.
fn form_text(bytes: &[u8]) -> Option<&str> {
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
        pairs = pairs.saturating_add(1);
    }
    if pairs == 0 {
        return None;
    }
    Some(text)
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
        // A masked container's *leaves* are what a handler extracts, echoes,
        // or binds — `{"value":"hunter2"}` in the set would never match the
        // `hunter2` an error quotes or an INSERT binds. Keep the container's
        // serialization (an error may echo the whole thing) and recurse so
        // every scalar beneath the matched key is retained too. Depth is
        // bounded by serde_json's own recursion limit at parse time.
        serde_json::Value::Object(map) => {
            values.insert(value.to_string().as_bytes());
            for child in map.values() {
                record_masked_value(child, values);
            }
        }
        serde_json::Value::Array(items) => {
            values.insert(value.to_string().as_bytes());
            for child in items {
                record_masked_value(child, values);
            }
        }
        other => values.insert(other.to_string().as_bytes()),
    }
}

/// Retain the on-wire spelling of every raw JSON string literal whose decoded
/// value redaction just masked.
///
/// A masked value's decoded form is in the echo set, but the client may have
/// spelled it with escapes — `"token"`, `"line\nbreak"` — and an error
/// that quotes the *raw* body carries that spelling, which a search for the
/// decoded bytes cannot find. This walks the raw text's `"`-delimited
/// literals (honouring backslash escapes), decodes each through serde, and
/// retains the literal's inner bytes whenever the decoded value is already in
/// the set and the spellings differ.
fn retain_raw_json_string_spellings(bytes: &[u8], values: &mut RedactedValues) {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return;
    };
    let mut rest = text;
    while let Some(start) = rest.find('"') {
        let Some(after_quote) = rest.get(start.saturating_add(1)..) else {
            return;
        };
        // Find the closing quote, skipping escaped characters.
        let mut end = None;
        let mut escape = false;
        for (index, c) in after_quote.char_indices() {
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                end = Some(index);
                break;
            }
        }
        let Some(end) = end else {
            return;
        };
        let Some(inner) = after_quote.get(..end) else {
            return;
        };
        if inner.contains('\\') {
            // Only escaped spellings can differ from their decoded form.
            let literal = format!("\"{inner}\"");
            if let Ok(decoded) = serde_json::from_str::<String>(&literal)
                && values.contains(decoded.as_bytes())
            {
                values.insert(inner.as_bytes());
            }
        }
        let Some(next) = after_quote.get(end.saturating_add(1)..) else {
            return;
        };
        rest = next;
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
    fn encoded_query_value_forms_join_the_echo_set() {
        let (request, values) = redact(
            Request::get("/callback?token=a%2Fb%2Bc&page=2"),
            CapturedBody::Absent,
            &filter_with(&[]),
        );

        assert!(
            !request.uri.contains("a%2Fb"),
            "the encoded value must be masked out of the capsule uri, got {}",
            request.uri
        );
        assert!(
            values.contains(b"a/b+c"),
            "the decoded value must be in the echo set"
        );
        assert!(
            values.contains(b"a%2Fb%2Bc"),
            "the on-the-wire encoded value must be in the echo set too — an error \
             that echoes the raw request target carries this spelling"
        );
        assert_eq!(
            mask_echoes("failed on /callback?token=a%2Fb%2Bc", &values),
            format!("failed on /callback?token={FILTERED_PLACEHOLDER}"),
            "an echoed raw request target must scrub"
        );
    }

    #[test]
    fn encoded_form_body_value_forms_join_the_echo_set() {
        let (request, values) = redact(
            Request::post("/callback")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded"),
            CapturedBody::Buffered(Bytes::from_static(b"token=a%2Fb%2Bc&page=2")),
            &filter_with(&[]),
        );

        let CapsuleBody::Text(body) = &request.body else {
            panic!(
                "a form body must be captured as text, got {:?}",
                request.body
            );
        };
        assert!(
            !body.contains("a%2Fb"),
            "the encoded value must be masked out of the capsule body, got {body}"
        );
        assert!(
            values.contains(b"a/b+c"),
            "the decoded value must be in the echo set"
        );
        assert!(
            values.contains(b"a%2Fb%2Bc"),
            "the on-the-wire encoded value must be in the echo set too — an error \
             that echoes the raw form body carries this spelling"
        );
        assert_eq!(
            mask_echoes("bad form field token=a%2Fb%2Bc", &values),
            format!("bad form field token={FILTERED_PLACEHOLDER}"),
            "an echoed raw form body must scrub"
        );
    }

    /// A sensitive key whose value is an object or array masks the whole
    /// container — but a handler extracts, echoes, or binds the *leaves*, so
    /// each scalar beneath the matched key must join the echo set alongside
    /// the container's serialization.
    #[test]
    fn leaf_values_of_a_masked_json_container_join_the_echo_set() {
        let raw = br#"{"secret":{"value":"hunter2secret","attempts":42},"keep":"public"}"#;
        let (request, values) = redact(
            Request::post("/hook").header(header::CONTENT_TYPE, "application/json"),
            CapturedBody::Buffered(Bytes::from_static(raw)),
            &filter_with(&[]),
        );

        let CapsuleBody::Text(body) = &request.body else {
            panic!(
                "a JSON body must be captured as text, got {:?}",
                request.body
            );
        };
        assert!(
            !body.contains("hunter2secret"),
            "the nested value must be masked out of the body, got {body}"
        );
        assert!(body.contains("public"), "unmatched fields must survive");
        assert!(
            values.contains(b"hunter2secret"),
            "a string leaf under a masked container must be in the echo set"
        );
        assert!(
            values.contains(b"42"),
            "a numeric leaf under a masked container must be in the echo set"
        );
        assert!(
            values.contains(br#"{"value":"hunter2secret","attempts":42}"#)
                || values.contains(br#"{"attempts":42,"value":"hunter2secret"}"#),
            "the container's own serialization stays in the set too"
        );
        assert_eq!(
            mask_echoes("could not verify hunter2secret", &values),
            format!("could not verify {FILTERED_PLACEHOLDER}"),
            "an error echoing an extracted leaf must scrub"
        );
    }

    /// A masked JSON value the client spelled with escapes must scrub in
    /// *both* spellings: the decoded form (what a handler error usually
    /// quotes) and the on-wire escaped form (what an error echoing the raw
    /// body carries).
    #[test]
    fn escaped_json_value_spellings_join_the_echo_set() {
        let raw = br#"{"token":"line\nbreak!"}"#;
        let (request, values) = redact(
            Request::post("/hook").header(header::CONTENT_TYPE, "application/json"),
            CapturedBody::Buffered(Bytes::from_static(raw)),
            &filter_with(&[]),
        );

        let CapsuleBody::Text(body) = &request.body else {
            panic!(
                "a JSON body must be captured as text, got {:?}",
                request.body
            );
        };
        assert!(
            !body.contains("break"),
            "the value must be masked out of the body, got {body}"
        );
        assert!(
            values.contains(b"line\nbreak!"),
            "the decoded spelling must be in the echo set"
        );
        assert!(
            values.contains(br"line\nbreak!"),
            "the on-wire escaped spelling must be in the echo set too"
        );
        assert_eq!(
            mask_echoes(r#"rejected body {"token":"line\nbreak!"}"#, &values),
            format!(r#"rejected body {{"token":"{FILTERED_PLACEHOLDER}"}}"#),
            "an error echoing the raw body must scrub the escaped spelling"
        );
    }

    /// A signed webhook's handler verifies a signature over the exact bytes
    /// the client sent. When nothing needs masking, the capsule must carry
    /// those exact bytes — re-serializing would drift whitespace, number
    /// spellings and key order, and the replay would flunk a signature check
    /// the production request passed.
    #[test]
    fn an_unredacted_json_body_keeps_its_exact_bytes() {
        let raw = br#"{ "b": 1.50,  "a": "x" }"#;
        let (request, _values) = redact(
            Request::post("/webhook").header(header::CONTENT_TYPE, "application/json"),
            CapturedBody::Buffered(Bytes::from_static(raw)),
            &filter_with(&[]),
        );

        assert_eq!(
            request.body,
            CapsuleBody::Text(String::from_utf8(raw.to_vec()).expect("utf8")),
            "a JSON body with nothing to mask must be preserved byte for byte"
        );
    }

    /// Decoding and re-encoding a query string canonicalizes spellings —
    /// `%2f` → `%2F`, a bare `flag` → `flag=` — and routes that inspect the
    /// raw query (or sign the request target) would diverge on replay. Only
    /// matched pairs may be rewritten; everything else keeps its bytes.
    #[test]
    fn unredacted_query_pairs_keep_their_raw_spelling() {
        // No sensitive pair at all: the whole target is untouched.
        let (request, _values) = redact(
            Request::get("/hook?path=a%2fb&flag&q=1+2"),
            CapturedBody::Absent,
            &filter_with(&[]),
        );
        assert_eq!(
            request.uri, "/hook?path=a%2fb&flag&q=1+2",
            "an unredacted query must be preserved byte for byte"
        );

        // A sensitive pair elsewhere: its neighbours still keep their bytes.
        let (request, values) = redact(
            Request::get("/hook?path=a%2fb&token=s3cret&flag"),
            CapturedBody::Absent,
            &filter_with(&[]),
        );
        assert_eq!(
            request.uri, "/hook?path=a%2fb&token=%5BFILTERED%5D&flag",
            "only the matched pair may be rewritten"
        );
        assert!(values.contains(b"s3cret"));
    }

    /// An absolute-form request target (the shape a proxy-style route sees)
    /// keeps its scheme and authority through masking: the rewritten query is
    /// spliced onto the original prefix, never rebuilt from the path alone.
    #[test]
    fn an_absolute_form_target_keeps_its_scheme_and_authority() {
        let (request, _values) = redact(
            Request::get("https://api.example/items?token=s3cret&keep=a%2f"),
            CapturedBody::Absent,
            &filter_with(&[]),
        );
        assert_eq!(
            request.uri, "https://api.example/items?token=%5BFILTERED%5D&keep=a%2f",
            "scheme and authority must survive query masking"
        );

        // And untouched absolute-form targets are preserved byte for byte.
        let (request, _values) = redact(
            Request::get("https://api.example/items?keep=a%2f"),
            CapturedBody::Absent,
            &filter_with(&[]),
        );
        assert_eq!(request.uri, "https://api.example/items?keep=a%2f");
    }

    /// The form-body twin of the raw-spelling guarantee: an untouched form —
    /// and the untouched neighbours of a masked field — keep the exact bytes
    /// the client sent.
    #[test]
    fn unredacted_form_pairs_keep_their_raw_spelling() {
        let (request, _values) = redact(
            Request::post("/hook")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded"),
            CapturedBody::Buffered(Bytes::from_static(b"path=a%2fb&q=1+2")),
            &filter_with(&[]),
        );
        assert_eq!(
            request.body,
            CapsuleBody::Text("path=a%2fb&q=1+2".to_owned()),
            "an unredacted form body must be preserved byte for byte"
        );

        let (request, values) = redact(
            Request::post("/hook")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded"),
            CapturedBody::Buffered(Bytes::from_static(b"path=a%2fb&token=s3cret")),
            &filter_with(&[]),
        );
        assert_eq!(
            request.body,
            CapsuleBody::Text("path=a%2fb&token=%5BFILTERED%5D".to_owned()),
            "only the matched form field may be rewritten"
        );
        assert!(values.contains(b"s3cret"));
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
