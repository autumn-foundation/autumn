//! Pure primary-vs-shadow response comparison (issue #1653).
//!
//! Everything in this module is a **pure function of its inputs**: no clock, no
//! randomness, no I/O. That is what makes the acceptance criterion "divergence
//! recording is deterministic and reproducible for a captured request" testable
//! — feed [`compare`] the same two [`ResponseFacts`] and it yields a
//! byte-identical [`Divergence`], fingerprint included. The registry
//! ([`crate::shadow::ShadowRegistry`]) stamps the wall-clock time, so the
//! comparison itself never varies.
//!
//! # What is compared
//!
//! Exactly two things, per the first slice's scope:
//!
//! 1. **Status class** — `2xx` vs `5xx` diverges; `200` vs `201` does not.
//! 2. **Normalized body** — see [`normalize_body`]. JSON object key order is
//!    normalized away (two builds serialising the same map differently are not
//!    a regression); **array order is preserved**, because a reordered list is
//!    precisely the kind of subtly-wrong-but-`200` response this feature
//!    exists to catch.
//!
//! Headers, latency, and fuzzy/semantic JSON tolerance are deliberately out of
//! scope.
//!
//! # PII
//!
//! Recorded samples pass through [`ParameterFilter`] — the same filter the
//! access log, error pages, and failure capsules use, driven by the same
//! `[log] filter_parameters` / `[log] unfilter_parameters` config.
//!
//! That filter redacts by **key name**, replacing a matched key's whole value,
//! so an excerpt is recorded only when every scalar has some object key above
//! it — that is exactly when naming a key could reach it. An HTML or binary
//! body has no keys; neither does a bare scalar (a `text/plain` one-time code
//! parses as a JSON number) nor a top-level array of strings. Those record a
//! digest, a length, and how the body was normalized. A divergence is still
//! reported — just without a body excerpt that no redaction rule could vet.

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

use bytes::Bytes;
use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::log::filter::ParameterFilter;

/// Placeholder appended to a sample that was cut short by the sample budget.
pub const TRUNCATION_MARKER: &str = "…";

/// The facts about one side of a comparison: what a build returned for a
/// mirrored request.
///
/// Deliberately not a `http::Response` — the shadow side never becomes a
/// response object at all (it is read out of the transport and dropped), and
/// keeping both sides in one plain struct is what lets [`compare`] be pure and
/// re-runnable from a recorded capture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResponseFacts {
    /// HTTP status code as returned by that build.
    pub status: u16,
    /// `Content-Type` header value, when the build sent one.
    pub content_type: Option<String>,
    /// `Content-Encoding` header value, when the build sent one.
    ///
    /// A handler may serve a precompressed representation, in which case the
    /// bytes here are encoded — on **either** side. Both are decoded before
    /// comparison; decoding only one would report identical builds as
    /// divergent.
    pub content_encoding: Option<String>,
    /// Response body bytes, already bounded by the mirror's capture budget.
    pub body: Bytes,
}

impl ResponseFacts {
    /// Construct facts for a body that is already in memory.
    #[must_use]
    pub const fn new(status: u16, content_type: Option<String>, body: Bytes) -> Self {
        Self {
            status,
            content_type,
            content_encoding: None,
            body,
        }
    }

    /// Construct facts for a body that arrived under a `Content-Encoding`.
    #[must_use]
    pub const fn encoded(
        status: u16,
        content_type: Option<String>,
        content_encoding: Option<String>,
        body: Bytes,
    ) -> Self {
        Self {
            status,
            content_type,
            content_encoding,
            body,
        }
    }
}

/// A response body reduced to the form the comparison actually operates on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NormalizedBody {
    /// No bytes at all (a `HEAD` or `204`, say).
    Empty,
    /// A parsed JSON document with every object's keys in sorted order.
    Json(Value),
    /// UTF-8 text with `\r\n` folded to `\n` and outer whitespace trimmed.
    Text(String),
    /// Anything else: compared byte-for-byte.
    Bytes(Bytes),
}

impl NormalizedBody {
    /// The canonical byte encoding this body digests to.
    #[must_use]
    fn canonical_bytes(&self) -> Vec<u8> {
        match self {
            Self::Empty => Vec::new(),
            // Canonicalisation already sorted every object's keys, so
            // `to_string` is stable regardless of whether `serde_json` was
            // compiled with `preserve_order`.
            Self::Json(value) => serde_json::to_vec(value).unwrap_or_default(),
            Self::Text(text) => text.as_bytes().to_vec(),
            Self::Bytes(bytes) => bytes.to_vec(),
        }
    }

    /// The tag recorded alongside a digest so a reader can tell *how* the two
    /// sides were compared.
    #[must_use]
    const fn kind_label(&self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::Json(_) => "json",
            Self::Text(_) => "text",
            Self::Bytes(_) => "bytes",
        }
    }
}

/// Which of the two compared dimensions disagreed.
///
/// A response can of course diverge on both; the status class is reported in
/// that case because it is the coarser (and more actionable) signal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DivergenceKind {
    /// The two builds returned different status *classes* (`status / 100`).
    StatusClass,
    /// Same status class, different normalized body.
    Body,
}

impl DivergenceKind {
    /// Stable lowercase name, used as a metric label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StatusClass => "status_class",
            Self::Body => "body",
        }
    }
}

/// One recorded disagreement between the live build and the candidate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Divergence {
    /// Which dimension disagreed.
    pub kind: DivergenceKind,
    /// Status the live build returned.
    pub primary_status: u16,
    /// Status the shadow build returned.
    pub shadow_status: u16,
    /// How the live body was normalized before digesting (`json`/`text`/…).
    pub primary_body_kind: &'static str,
    /// How the shadow body was normalized before digesting.
    pub shadow_body_kind: &'static str,
    /// Bytes in the live body as captured.
    pub primary_body_bytes: usize,
    /// Bytes in the shadow body as captured.
    pub shadow_body_bytes: usize,
    /// Hex SHA-256 of the live body's canonical encoding.
    pub primary_digest: String,
    /// Hex SHA-256 of the shadow body's canonical encoding.
    pub shadow_digest: String,
    /// Redacted, budget-bounded excerpt of the live body — JSON bodies only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary_sample: Option<Value>,
    /// Redacted, budget-bounded excerpt of the shadow body — JSON bodies only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shadow_sample: Option<Value>,
    /// Content-addressed identity of this divergence: the same captured pair
    /// always yields the same value, so repeat occurrences collapse and an
    /// operator can quote one in a bug report.
    pub fingerprint: String,
}

/// The result of comparing one mirrored request's two responses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Comparison {
    /// The two builds agree on status class and normalized body.
    Match,
    /// They disagree; the boxed record is what gets reported.
    Diverged(Box<Divergence>),
}

impl Comparison {
    /// Metric-label value for this outcome.
    #[must_use]
    pub const fn outcome_label(&self) -> &'static str {
        match self {
            Self::Match => "match",
            Self::Diverged(_) => "diverged",
        }
    }
}

/// Compare a primary response against its shadow.
///
/// `sample_limit` bounds the serialized length of each recorded JSON sample;
/// anything longer is replaced by a truncated string ending in
/// [`TRUNCATION_MARKER`].
#[must_use]
pub fn compare(
    primary: &ResponseFacts,
    shadow: &ResponseFacts,
    filter: &ParameterFilter,
    sample_limit: usize,
) -> Comparison {
    let primary_body = normalize_body(primary);
    let shadow_body = normalize_body(shadow);

    let status_class_differs = status_class(primary.status) != status_class(shadow.status);
    let body_differs = primary_body != shadow_body;

    if !status_class_differs && !body_differs {
        return Comparison::Match;
    }

    let kind = if status_class_differs {
        DivergenceKind::StatusClass
    } else {
        DivergenceKind::Body
    };

    let primary_digest = digest(&primary_body);
    let shadow_digest = digest(&shadow_body);
    let fingerprint = fingerprint(
        kind,
        primary.status,
        &primary_digest,
        shadow.status,
        &shadow_digest,
    );

    Comparison::Diverged(Box::new(Divergence {
        kind,
        primary_status: primary.status,
        shadow_status: shadow.status,
        primary_body_kind: primary_body.kind_label(),
        shadow_body_kind: shadow_body.kind_label(),
        primary_body_bytes: primary.body.len(),
        shadow_body_bytes: shadow.body.len(),
        primary_digest,
        shadow_digest,
        primary_sample: sample(&primary_body, filter, sample_limit),
        shadow_sample: sample(&shadow_body, filter, sample_limit),
        fingerprint,
    }))
}

/// The status *class* two responses are compared on (`2` for any `2xx`).
#[must_use]
pub const fn status_class(status: u16) -> u16 {
    status.saturating_div(100)
}

/// Reduce a raw response body to its comparable form.
///
/// JSON is recognised either from the content type or from the body's own
/// first non-whitespace byte, so a build that mislabels its JSON is still
/// compared structurally rather than byte-for-byte. A body that *claims* to be
/// JSON but does not parse falls back to a byte comparison — deliberately: two
/// identical malformed bodies still agree, and pretending otherwise would
/// manufacture a divergence out of a shared bug.
#[must_use]
pub fn normalize_body(facts: &ResponseFacts) -> NormalizedBody {
    if facts.body.is_empty() {
        return NormalizedBody::Empty;
    }

    // NB: `facts.content_type` is deliberately unread. It is recorded for the
    // operator's benefit and never consulted here — see the two branches below.
    // Attempted on every body, with no content-type sniff in front of it. A
    // sniff for `{`/`[` missed scalar JSON — `null`, `true`, `1`, `"ok"` — so a
    // body that parsed as JSON on the labelled side fell through to `Text` on
    // the unlabelled one, and a header-only difference became a body
    // divergence. Parsing is the only symmetric test. A non-JSON body fails on
    // its first byte, so this costs nothing on the common path.
    if let Ok(value) = serde_json::from_slice::<Value>(&facts.body) {
        return NormalizedBody::Json(canonicalize(&value));
    }

    // UTF-8 decides this, NOT the content type. Headers are explicitly outside
    // the comparison contract, so two builds returning the identical bytes
    // `hello` must not diverge merely because only one of them said
    // `text/plain` — that would put a header back inside the contract through
    // the back door, as a difference in normalized *form* rather than in
    // content. A genuinely binary body is not valid UTF-8 and still lands in
    // `Bytes`.
    if let Ok(text) = std::str::from_utf8(&facts.body) {
        return NormalizedBody::Text(normalize_text(text));
    }

    NormalizedBody::Bytes(facts.body.clone())
}

/// Fold `\r\n` to `\n` and trim the body's outer whitespace.
///
/// Interior whitespace is left alone: it is load-bearing inside `<pre>`, in
/// generated CSV, and in anything indentation-sensitive, so collapsing it would
/// hide real divergences.
fn normalize_text(text: &str) -> String {
    text.replace("\r\n", "\n").trim().to_owned()
}

/// Recursively sort every JSON object's keys.
///
/// Arrays keep their order — see the module docs.
fn canonicalize(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable();
            let mut out = Map::new();
            for key in keys {
                if let Some(child) = map.get(key) {
                    out.insert(key.clone(), canonicalize(child));
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonicalize).collect()),
        other => other.clone(),
    }
}

/// Hex SHA-256 of a normalized body's canonical encoding, prefixed by its kind
/// so an empty JSON body and an empty text body cannot collide.
fn digest(body: &NormalizedBody) -> String {
    let mut hasher = Sha256::new();
    hasher.update(body.kind_label().as_bytes());
    hasher.update([0]);
    hasher.update(body.canonical_bytes());
    hex::encode(hasher.finalize())
}

/// Content-addressed identity for a divergence.
fn fingerprint(
    kind: DivergenceKind,
    primary_status: u16,
    primary_digest: &str,
    shadow_status: u16,
    shadow_digest: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(kind.as_str().as_bytes());
    hasher.update([0]);
    hasher.update(primary_status.to_string().as_bytes());
    hasher.update([0]);
    hasher.update(primary_digest.as_bytes());
    hasher.update([0]);
    hasher.update(shadow_status.to_string().as_bytes());
    hasher.update([0]);
    hasher.update(shadow_digest.as_bytes());
    hex::encode(hasher.finalize()).chars().take(16).collect()
}

/// Build the recorded excerpt for one side, or `None` when the body is not
/// JSON (see the module docs on why non-JSON bodies are not sampled).
///
/// `sample_limit` is a budget in **bytes** of the serialized form, matching the
/// `shadow.max_sample_bytes` config key: counting characters instead would let
/// a body of multi-byte text store up to four times the nominal budget.
fn sample(body: &NormalizedBody, filter: &ParameterFilter, sample_limit: usize) -> Option<Value> {
    let NormalizedBody::Json(value) = body else {
        return None;
    };
    // Parsing as JSON is not on its own a licence to record. `ParameterFilter`
    // redacts by KEY NAME, so a value only has protection if it sits under one:
    // a `text/plain` six-digit OTP parses as a JSON number, and a bare scalar —
    // or a scalar inside an array — offers the filter nothing to match. Those
    // record a digest and a length like any other unsamplable body.
    if !scalars_are_all_keyed(value) {
        return None;
    }
    let scrubbed = filter.scrub_json(value);
    let rendered = serde_json::to_string(&scrubbed).unwrap_or_default();
    if rendered.len() <= sample_limit {
        return Some(scrubbed);
    }
    Some(Value::String(truncate_to_serialized_budget(
        &rendered,
        sample_limit,
    )))
}

/// Build the truncated excerpt so that the value's own **serialized** form fits
/// in `sample_limit` bytes.
///
/// Cutting the source string at the budget is not enough: the result is stored
/// as a JSON string, so serializing it adds the surrounding quotes and escapes
/// every `"`, `\` and control character already in the prefix. A prefix taken
/// at exactly the budget can therefore serialize to well over it — which is the
/// number the operator actually configured. So the cost is accumulated per
/// character as it will be *written*, with the quotes and the marker reserved
/// up front.
fn truncate_to_serialized_budget(rendered: &str, sample_limit: usize) -> String {
    /// The two `"` a JSON string is written between.
    const QUOTES: usize = 2;

    let reserved = QUOTES.saturating_add(TRUNCATION_MARKER.len());
    // Not even the marker fits; the marker alone is the honest answer.
    let Some(budget) = sample_limit.checked_sub(reserved) else {
        return TRUNCATION_MARKER.to_owned();
    };

    let mut truncated = String::new();
    let mut cost = 0usize;
    for character in rendered.chars() {
        cost = cost.saturating_add(serialized_len(character));
        if cost > budget {
            break;
        }
        truncated.push(character);
    }
    truncated.push_str(TRUNCATION_MARKER);
    truncated
}

/// How many bytes `character` occupies once written inside a JSON string.
const fn serialized_len(character: char) -> usize {
    match character {
        // Escaped as a two-character sequence.
        '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
        // Every other control character becomes `\u00XX`.
        c if (c as u32) < 0x20 => 6,
        c => c.len_utf8(),
    }
}

/// Whether every scalar in `value` has an object key somewhere above it, and is
/// therefore something [`ParameterFilter`] can be asked about.
///
/// The question is not whether a scalar is *directly* under a key — it is
/// whether naming a key in `[log] filter_parameters` could redact it.
/// `scrub_json` replaces a matched key's **whole value**, so everything inside
/// an object is covered by that object's keys, however deeply nested:
/// `{"tags": ["x"]}` is redacted entirely by listing `tags`.
///
/// What cannot be covered is a value with no key above it at all — a bare
/// scalar body, or a top-level array of scalars. There is no name an operator
/// could write down that would reach those, so a body containing one is not
/// sampled and records only a digest and a length.
fn scalars_are_all_keyed(value: &Value) -> bool {
    match value {
        // Every descendant has this object's keys above it.
        Value::Object(_) => true,
        // Elements have no key of their own, so each must supply one.
        Value::Array(items) => items.iter().all(scalars_are_all_keyed),
        // Nothing above this to name it.
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => false,
    }
}

/// Redact a request target (`/path?query`) for recording.
///
/// Each query parameter whose name the filter matches is replaced with the
/// filter's placeholder; parameter order is preserved so the result stays
/// deterministic.
///
/// Matching is done on the **percent-decoded** name and on each of its
/// structural segments, mirroring how failure capsules redact a captured URL.
/// Both matter: `?%74oken=…` hides a filtered name behind an encoding, and
/// `?auth[access_token]=…` / `?filter.password=…` hide it inside a nested key
/// that whole-string matching would miss. A parameter with no `=` is treated as
/// a bare name and redacted to the placeholder when it matches, since
/// `?SECRETVALUE`-style targets carry the secret in the name position.
///
/// The path itself is kept verbatim. It is the route, which is what makes a
/// record actionable — but a path *can* carry user data
/// (`/password-reset/{token}`), so the recorded target is only ever published
/// on the sensitive actuator endpoint, never in the ordinary log stream.
#[must_use]
pub fn redact_path_and_query(target: &str, filter: &ParameterFilter) -> String {
    let Some((path, query)) = target.split_once('?') else {
        return target.to_owned();
    };
    let redacted: Vec<String> = query
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((key, _)) if key_is_filtered(key, filter) => {
                format!("{key}={}", crate::log::filter::FILTERED_PLACEHOLDER)
            }
            None if !pair.is_empty() && key_is_filtered(pair, filter) => {
                crate::log::filter::FILTERED_PLACEHOLDER.to_owned()
            }
            _ => pair.to_owned(),
        })
        .collect();
    format!("{path}?{}", redacted.join("&"))
}

/// Whether a raw query-parameter name should be filtered.
///
/// Checks the name as written, its percent-decoded form, and every structural
/// segment of that decoded form (`a[b]` and `a.b` both yield `a` and `b`).
fn key_is_filtered(raw_key: &str, filter: &ParameterFilter) -> bool {
    if filter.matches_key(raw_key) {
        return true;
    }
    let decoded = percent_decode(raw_key);
    if filter.matches_key(&decoded) {
        return true;
    }
    decoded
        .split(['[', ']', '.'])
        .any(|segment| !segment.is_empty() && filter.matches_key(segment))
}

/// Percent-decode a query-parameter name, treating `+` as a space.
///
/// Undecodable bytes are passed through as written: this feeds a filter-name
/// comparison, so a best-effort decode that never fails is the right shape.
///
/// Written as a split over `%` rather than an index walk so it carries no
/// cursor arithmetic at all — this module is on the request path and its panic
/// gate denies arithmetic that could overflow.
fn percent_decode(raw: &str) -> String {
    let replaced = raw.replace('+', " ");
    let mut out: Vec<u8> = Vec::with_capacity(replaced.len());
    let mut parts = replaced.as_bytes().split(|byte| *byte == b'%');

    // Everything before the first `%` is literal.
    if let Some(first) = parts.next() {
        out.extend_from_slice(first);
    }
    for part in parts {
        if let (Some(high), Some(low)) = (
            part.first().copied().and_then(hex_value),
            part.get(1).copied().and_then(hex_value),
        ) {
            out.push(high.wrapping_shl(4) | low);
            out.extend_from_slice(part.get(2..).unwrap_or_default());
        } else {
            // Not a valid escape — keep the `%` and the rest verbatim.
            out.push(b'%');
            out.extend_from_slice(part);
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The numeric value of one ASCII hex digit.
///
/// The `wrapping_*` calls cannot wrap — each arm's range guarantees the
/// subtraction is in bounds — but they say so without arithmetic this module's
/// panic gate has to take on faith.
const fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte.wrapping_sub(b'0')),
        b'a'..=b'f' => Some(byte.wrapping_sub(b'a').wrapping_add(10)),
        b'A'..=b'F' => Some(byte.wrapping_sub(b'A').wrapping_add(10)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::filter::ParameterFilter;
    use bytes::Bytes;

    fn json(status: u16, body: &str) -> ResponseFacts {
        ResponseFacts {
            status,
            content_type: Some("application/json".to_owned()),
            content_encoding: None,
            body: Bytes::from(body.to_owned()),
        }
    }

    #[test]
    fn identical_json_matches() {
        let filter = ParameterFilter::default();
        let a = json(200, r#"{"id":1,"name":"ada"}"#);
        let b = json(200, r#"{"id":1,"name":"ada"}"#);
        assert!(matches!(compare(&a, &b, &filter, 2048), Comparison::Match));
    }

    #[test]
    fn json_object_key_order_is_normalized_away() {
        let filter = ParameterFilter::default();
        let a = json(200, r#"{"id":1,"name":"ada"}"#);
        let b = json(200, r#"{"name":"ada","id":1}"#);
        assert!(matches!(compare(&a, &b, &filter, 2048), Comparison::Match));
    }

    #[test]
    fn json_array_order_is_a_divergence() {
        let filter = ParameterFilter::default();
        let a = json(200, r#"{"ids":[1,2,3]}"#);
        let b = json(200, r#"{"ids":[3,2,1]}"#);
        assert!(matches!(
            compare(&a, &b, &filter, 2048),
            Comparison::Diverged(_)
        ));
    }

    #[test]
    fn dropped_json_field_is_a_body_divergence() {
        let filter = ParameterFilter::default();
        let a = json(200, r#"{"id":1,"total":42}"#);
        let b = json(200, r#"{"id":1}"#);
        let Comparison::Diverged(d) = compare(&a, &b, &filter, 2048) else {
            panic!("expected divergence");
        };
        assert_eq!(d.kind, DivergenceKind::Body);
    }

    #[test]
    fn differing_status_class_is_a_status_divergence() {
        let filter = ParameterFilter::default();
        let a = json(200, r#"{"id":1}"#);
        let b = json(500, r#"{"id":1}"#);
        let Comparison::Diverged(d) = compare(&a, &b, &filter, 2048) else {
            panic!("expected divergence");
        };
        assert_eq!(d.kind, DivergenceKind::StatusClass);
        assert_eq!(d.primary_status, 200);
        assert_eq!(d.shadow_status, 500);
    }

    #[test]
    fn same_status_class_different_code_is_not_a_status_divergence() {
        let filter = ParameterFilter::default();
        let a = json(200, r#"{"id":1}"#);
        let b = json(201, r#"{"id":1}"#);
        assert!(matches!(compare(&a, &b, &filter, 2048), Comparison::Match));
    }

    #[test]
    fn comparison_is_deterministic_and_reproducible() {
        let filter = ParameterFilter::default();
        let a = json(200, r#"{"id":1,"total":42}"#);
        let b = json(200, r#"{"id":1}"#);
        let first = compare(&a, &b, &filter, 2048);
        let second = compare(&a, &b, &filter, 2048);
        let (Comparison::Diverged(x), Comparison::Diverged(y)) = (first, second) else {
            panic!("expected divergences");
        };
        assert_eq!(x, y, "the same captured pair must produce the same record");
        assert!(!x.fingerprint.is_empty());
    }

    #[test]
    fn samples_are_pii_redacted() {
        let filter = ParameterFilter::default();
        let a = json(200, r#"{"id":1,"password":"hunter2","total":1}"#);
        let b = json(200, r#"{"id":1,"password":"hunter2"}"#);
        let Comparison::Diverged(d) = compare(&a, &b, &filter, 2048) else {
            panic!("expected divergence");
        };
        let rendered = serde_json::to_string(&d.primary_sample).unwrap();
        assert!(
            !rendered.contains("hunter2"),
            "recorded sample must not carry a filtered value: {rendered}"
        );
        assert!(rendered.contains("[FILTERED]"));
    }

    #[test]
    fn non_json_bodies_record_no_sample() {
        let filter = ParameterFilter::default();
        let html = |body: &str| ResponseFacts {
            status: 200,
            content_type: Some("text/html; charset=utf-8".to_owned()),
            content_encoding: None,
            body: Bytes::from(body.to_owned()),
        };
        let Comparison::Diverged(d) = compare(
            &html("<p>ada@example.com</p>"),
            &html("<p>grace@example.com</p>"),
            &filter,
            2048,
        ) else {
            panic!("expected divergence");
        };
        assert!(d.primary_sample.is_none());
        assert!(d.shadow_sample.is_none());
        assert_ne!(d.primary_digest, d.shadow_digest);
    }

    #[test]
    fn identical_bytes_do_not_diverge_because_only_one_side_named_a_content_type() {
        // Headers are outside the comparison contract, so a header difference
        // must not reappear as a difference in normalized *form*.
        let filter = ParameterFilter::default();
        let labelled = ResponseFacts::new(
            200,
            Some("text/plain".to_owned()),
            Bytes::from_static(b"hello"),
        );
        let unlabelled = ResponseFacts::new(200, None, Bytes::from_static(b"hello"));
        assert!(matches!(
            compare(&labelled, &unlabelled, &filter, 2048),
            Comparison::Match
        ));

        // ...and a genuinely different body still diverges.
        let other = ResponseFacts::new(200, None, Bytes::from_static(b"goodbye"));
        assert!(matches!(
            compare(&labelled, &other, &filter, 2048),
            Comparison::Diverged(_)
        ));
    }

    #[test]
    fn scalar_json_is_recognised_without_a_content_type() {
        // `{`/`[` sniffing missed these, so a labelled side became `Json` while
        // an unlabelled one became `Text` — a header-only difference surfacing
        // as a body divergence.
        let filter = ParameterFilter::default();
        for body in ["null", "true", "1", "\"ok\"", "1.5"] {
            let labelled = ResponseFacts::new(
                200,
                Some("application/json".to_owned()),
                Bytes::from(body.to_owned()),
            );
            let unlabelled = ResponseFacts::new(200, None, Bytes::from(body.to_owned()));
            assert!(
                matches!(
                    compare(&labelled, &unlabelled, &filter, 2048),
                    Comparison::Match
                ),
                "scalar JSON body {body:?} diverged on a header-only difference"
            );
        }
    }

    #[test]
    fn a_scalar_body_is_never_sampled_however_it_is_labelled() {
        // A `text/plain` six-digit OTP parses as a JSON number. Comparing it
        // structurally is right; RECORDING it is not — `ParameterFilter`
        // redacts by key name, and a bare scalar gives it nothing to match.
        let filter = ParameterFilter::default();
        for (content_type, body) in [
            (Some("text/plain".to_owned()), "482913"),
            (Some("application/json".to_owned()), "482913"),
            (Some("application/json".to_owned()), "\"a-bearer-token\""),
            (None, "482913"),
        ] {
            let primary = ResponseFacts::new(200, content_type.clone(), Bytes::from(body));
            let shadow = ResponseFacts::new(200, content_type, Bytes::from_static(b"0"));
            let Comparison::Diverged(d) = compare(&primary, &shadow, &filter, 2048) else {
                panic!("expected divergence for {body}");
            };
            assert!(
                d.primary_sample.is_none(),
                "a scalar body must record a digest only, got a sample for {body}"
            );
            assert!(!d.primary_digest.is_empty());
        }
    }

    #[test]
    fn an_array_of_bare_scalars_is_not_sampled_either() {
        // Array elements have no key above them, so the filter cannot vet them.
        let filter = ParameterFilter::default();
        let list = |body: &str| {
            ResponseFacts::new(
                200,
                Some("application/json".to_owned()),
                Bytes::from(body.to_owned()),
            )
        };
        let Comparison::Diverged(d) =
            compare(&list(r#"["a-bearer-token"]"#), &list("[]"), &filter, 2048)
        else {
            panic!("expected divergence");
        };
        assert!(d.primary_sample.is_none());
    }

    #[test]
    fn a_list_of_strings_under_a_key_is_still_sampled() {
        // Regression: an earlier form of this rule demanded that every scalar be
        // DIRECTLY under a key, which refused `{"items": ["a", "b"]}` — an
        // extremely ordinary shape whose array IS governed by `items`. Listing
        // `items` in `filter_parameters` redacts the whole array, so the filter
        // can be asked about it and the excerpt is safe to record.
        let filter = ParameterFilter::default();
        let body = |raw: &str| {
            ResponseFacts::new(
                200,
                Some("application/json".to_owned()),
                Bytes::from(raw.to_owned()),
            )
        };
        let Comparison::Diverged(d) = compare(
            &body(r#"{"id":7,"total":42,"items":["a","b"]}"#),
            &body(r#"{"id":7,"items":["a","b"]}"#),
            &filter,
            2048,
        ) else {
            panic!("expected divergence");
        };
        let sample = d.primary_sample.expect("a keyed body must be sampled");
        assert_eq!(sample["total"], serde_json::json!(42));
        assert_eq!(sample["items"], serde_json::json!(["a", "b"]));
    }

    #[test]
    fn a_list_under_a_filtered_key_is_redacted_whole() {
        let filter = ParameterFilter::default();
        let body = |raw: &str| {
            ResponseFacts::new(
                200,
                Some("application/json".to_owned()),
                Bytes::from(raw.to_owned()),
            )
        };
        let Comparison::Diverged(d) = compare(
            &body(r#"{"id":1,"token":["abc","def"]}"#),
            &body(r#"{"id":2,"token":["abc","def"]}"#),
            &filter,
            2048,
        ) else {
            panic!("expected divergence");
        };
        let rendered = serde_json::to_string(&d.primary_sample).unwrap();
        assert!(!rendered.contains("abc"), "{rendered}");
        assert!(rendered.contains("[FILTERED]"), "{rendered}");
    }

    #[test]
    fn keyed_bodies_are_still_sampled_including_arrays_of_objects() {
        // The common shape must keep its excerpt: every scalar here sits under
        // a key the filter can be asked about.
        let filter = ParameterFilter::default();
        let json_body = |body: &str| {
            ResponseFacts::new(
                200,
                Some("application/json".to_owned()),
                Bytes::from(body.to_owned()),
            )
        };
        let Comparison::Diverged(d) = compare(
            &json_body(r#"[{"id":1,"password":"hunter2"},{"id":2}]"#),
            &json_body("[]"),
            &filter,
            2048,
        ) else {
            panic!("expected divergence");
        };
        let rendered = serde_json::to_string(&d.primary_sample).unwrap();
        assert!(rendered.contains("[FILTERED]"), "{rendered}");
        assert!(!rendered.contains("hunter2"), "{rendered}");
    }

    #[test]
    fn scalar_json_still_diverges_on_a_real_difference() {
        let filter = ParameterFilter::default();
        let scalar = |body: &str| ResponseFacts::new(200, None, Bytes::from(body.to_owned()));
        assert!(matches!(
            compare(&scalar("true"), &scalar("false"), &filter, 2048),
            Comparison::Diverged(_)
        ));
        assert!(matches!(
            compare(&scalar("1"), &scalar("2"), &filter, 2048),
            Comparison::Diverged(_)
        ));
    }

    #[test]
    fn a_body_that_is_not_utf8_is_compared_byte_for_byte() {
        let filter = ParameterFilter::default();
        let png = |trailer: u8| {
            ResponseFacts::new(
                200,
                Some("image/png".to_owned()),
                Bytes::from(vec![0x89, b'P', b'N', b'G', 0xFF, trailer]),
            )
        };
        assert!(matches!(
            compare(&png(1), &png(1), &filter, 2048),
            Comparison::Match
        ));
        assert!(matches!(
            compare(&png(1), &png(2), &filter, 2048),
            Comparison::Diverged(_)
        ));
    }

    #[test]
    fn text_bodies_normalize_line_endings_and_outer_whitespace() {
        let filter = ParameterFilter::default();
        let text = |body: &str| ResponseFacts {
            status: 200,
            content_type: Some("text/plain".to_owned()),
            content_encoding: None,
            body: Bytes::from(body.to_owned()),
        };
        assert!(matches!(
            compare(
                &text("hello\r\nworld"),
                &text("  hello\nworld\n"),
                &filter,
                2048
            ),
            Comparison::Match
        ));
    }

    #[test]
    fn oversized_samples_are_truncated_to_the_byte_budget() {
        let filter = ParameterFilter::default();
        let big = format!(r#"{{"blob":"{}"}}"#, "x".repeat(4096));
        let Comparison::Diverged(d) = compare(&json(200, &big), &json(200, "{}"), &filter, 64)
        else {
            panic!("expected divergence");
        };
        let Some(Value::String(sample)) = d.primary_sample else {
            panic!("a truncated sample is recorded as a string");
        };
        assert!(sample.ends_with(TRUNCATION_MARKER), "{sample}");
        let body = sample.strip_suffix(TRUNCATION_MARKER).expect("marker");
        assert!(
            body.len() <= 64,
            "sample body is {} bytes, budget was 64",
            body.len()
        );
    }

    #[test]
    fn a_truncated_sample_fits_its_budget_once_serialized() {
        // The excerpt is stored as a JSON string, so serializing re-adds the
        // quotes and escapes every quote and backslash in the prefix. Cutting
        // the source at the budget therefore overshoots the number the operator
        // configured.
        let filter = ParameterFilter::default();
        let quoted = format!(r#"{{"blob":"{}"}}"#, "a\"b\\c".repeat(400));
        for limit in [8usize, 16, 64, 512] {
            let Comparison::Diverged(d) =
                compare(&json(200, &quoted), &json(200, "{}"), &filter, limit)
            else {
                panic!("expected divergence at limit {limit}");
            };
            let serialized = serde_json::to_string(&d.primary_sample).expect("sample serializes");
            assert!(
                serialized.len() <= limit,
                "limit {limit}: serialized sample is {} bytes: {serialized}",
                serialized.len()
            );
        }
    }

    #[test]
    fn an_absurdly_small_budget_still_produces_something_valid() {
        let filter = ParameterFilter::default();
        let Comparison::Diverged(d) =
            compare(&json(200, r#"{"a":1}"#), &json(200, "{}"), &filter, 1)
        else {
            panic!("expected divergence");
        };
        // Not required to fit a budget smaller than the marker itself, but it
        // must still be a valid, obviously-truncated value rather than a panic.
        let Some(Value::String(sample)) = d.primary_sample else {
            panic!("expected a truncated string");
        };
        assert!(sample.ends_with(TRUNCATION_MARKER), "{sample}");
    }

    #[test]
    fn the_sample_budget_is_bytes_not_characters() {
        let filter = ParameterFilter::default();
        // Each `€` is three UTF-8 bytes. Counting characters would let this
        // store roughly three times the operator's nominal budget.
        let wide = format!(r#"{{"note":"{}"}}"#, "€".repeat(200));
        let Comparison::Diverged(d) = compare(&json(200, &wide), &json(200, "{}"), &filter, 64)
        else {
            panic!("expected divergence");
        };
        let Some(Value::String(sample)) = d.primary_sample else {
            panic!("a truncated sample is recorded as a string");
        };
        let body = sample.strip_suffix(TRUNCATION_MARKER).expect("marker");
        assert!(body.len() <= 64, "{} bytes exceeds the budget", body.len());
    }

    #[test]
    fn both_samples_are_redacted_not_just_the_primary() {
        let filter = ParameterFilter::default();
        let a = json(200, r#"{"id":1,"password":"hunter2","total":1}"#);
        let b = json(200, r#"{"id":2,"password":"hunter2"}"#);
        let Comparison::Diverged(d) = compare(&a, &b, &filter, 2048) else {
            panic!("expected divergence");
        };
        for (side, sample) in [("primary", &d.primary_sample), ("shadow", &d.shadow_sample)] {
            let rendered = serde_json::to_string(sample).unwrap();
            assert!(
                !rendered.contains("hunter2"),
                "{side} sample leaked a filtered value: {rendered}"
            );
        }
    }

    #[test]
    fn redaction_sees_through_percent_encoded_parameter_names() {
        let filter = ParameterFilter::default();
        assert_eq!(
            redact_path_and_query("/x?%74oken=abc123", &filter),
            "/x?%74oken=[FILTERED]"
        );
    }

    #[test]
    fn redaction_sees_into_nested_parameter_names() {
        let filter = ParameterFilter::default();
        for (raw, expected) in [
            (
                "/x?auth[access_token]=s",
                "/x?auth[access_token]=[FILTERED]",
            ),
            ("/x?filter.password=s", "/x?filter.password=[FILTERED]"),
            ("/x?auth%5Bapi_key%5D=s", "/x?auth%5Bapi_key%5D=[FILTERED]"),
        ] {
            assert_eq!(redact_path_and_query(raw, &filter), expected, "{raw}");
        }
    }

    #[test]
    fn a_valueless_parameter_that_names_a_secret_is_redacted() {
        let filter = ParameterFilter::default();
        assert_eq!(
            redact_path_and_query("/reset?token", &filter),
            "/reset?[FILTERED]"
        );
        assert_eq!(redact_path_and_query("/list?debug", &filter), "/list?debug");
    }

    #[test]
    fn redaction_preserves_unrelated_parameters_and_their_order() {
        let filter = ParameterFilter::default();
        assert_eq!(
            redact_path_and_query("/x?a=1&token=t&b=2", &filter),
            "/x?a=1&token=[FILTERED]&b=2"
        );
    }

    #[test]
    fn empty_bodies_on_both_sides_match() {
        let filter = ParameterFilter::default();
        let head = || ResponseFacts {
            status: 204,
            content_type: None,
            content_encoding: None,
            body: Bytes::new(),
        };
        assert!(matches!(
            compare(&head(), &head(), &filter, 2048),
            Comparison::Match
        ));
    }

    #[test]
    fn invalid_json_falls_back_to_byte_comparison() {
        let filter = ParameterFilter::default();
        let a = json(200, "{not json");
        let b = json(200, "{not json");
        assert!(matches!(compare(&a, &b, &filter, 2048), Comparison::Match));
        let c = json(200, "{not json!");
        assert!(matches!(
            compare(&a, &c, &filter, 2048),
            Comparison::Diverged(_)
        ));
    }

    #[test]
    fn digests_are_stable_across_equivalent_encodings() {
        let filter = ParameterFilter::default();
        let a = json(200, r#"{"a":1,"b":2}"#);
        let b = json(200, "{ \"b\" : 2 , \"a\" : 1 }");
        assert!(matches!(compare(&a, &b, &filter, 2048), Comparison::Match));
    }

    #[test]
    fn redact_query_filters_sensitive_parameters() {
        let filter = ParameterFilter::default();
        assert_eq!(
            redact_path_and_query("/search?q=ada&token=abc123", &filter),
            "/search?q=ada&token=[FILTERED]"
        );
        assert_eq!(redact_path_and_query("/plain", &filter), "/plain");
    }
}
