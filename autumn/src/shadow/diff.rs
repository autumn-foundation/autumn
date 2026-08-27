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
//! `[log] filter_parameters` / `[log] unfilter_parameters` config. Only **JSON**
//! bodies are sampled: a JSON body has named keys the filter can reason about,
//! whereas an HTML or binary body does not, so for those only a digest, a
//! length, and the content type are recorded. A divergence is still reported —
//! just without a body excerpt that no redaction rule could vet.

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

    let content_type = facts
        .content_type
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();

    if looks_like_json(&content_type, &facts.body)
        && let Ok(value) = serde_json::from_slice::<Value>(&facts.body)
    {
        return NormalizedBody::Json(canonicalize(&value));
    }

    if is_textual(&content_type)
        && let Ok(text) = std::str::from_utf8(&facts.body)
    {
        return NormalizedBody::Text(normalize_text(text));
    }

    NormalizedBody::Bytes(facts.body.clone())
}

/// Whether this body is worth attempting to parse as JSON.
fn looks_like_json(content_type: &str, body: &[u8]) -> bool {
    if content_type.contains("json") {
        return true;
    }
    body.iter()
        .find(|byte| !byte.is_ascii_whitespace())
        .is_some_and(|byte| matches!(byte, b'{' | b'['))
}

/// Whether a content type describes text whose whitespace can be normalized.
fn is_textual(content_type: &str) -> bool {
    content_type.starts_with("text/")
        || content_type.contains("xml")
        || content_type.contains("javascript")
        || content_type.contains("csv")
        || content_type.contains("x-www-form-urlencoded")
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
fn sample(body: &NormalizedBody, filter: &ParameterFilter, sample_limit: usize) -> Option<Value> {
    let NormalizedBody::Json(value) = body else {
        return None;
    };
    let scrubbed = filter.scrub_json(value);
    let rendered = serde_json::to_string(&scrubbed).unwrap_or_default();
    if rendered.chars().count() <= sample_limit {
        return Some(scrubbed);
    }
    let mut truncated: String = rendered.chars().take(sample_limit).collect();
    truncated.push_str(TRUNCATION_MARKER);
    Some(Value::String(truncated))
}

/// Redact a request target (`/path?query`) for recording.
///
/// The path is kept verbatim — it is a route, not user data — while each query
/// parameter whose name the filter matches is replaced with the filter's
/// placeholder. Parameter order is preserved so the result stays deterministic.
#[must_use]
pub fn redact_path_and_query(target: &str, filter: &ParameterFilter) -> String {
    let Some((path, query)) = target.split_once('?') else {
        return target.to_owned();
    };
    let redacted: Vec<String> = query
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((key, _)) if filter.matches_key(key) => {
                format!("{key}={}", crate::log::filter::FILTERED_PLACEHOLDER)
            }
            _ => pair.to_owned(),
        })
        .collect();
    format!("{path}?{}", redacted.join("&"))
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
            body: Bytes::from(body.to_owned()),
        }
    }

    #[test]
    fn identical_json_matches() {
        let filter = ParameterFilter::default();
        let a = json(200, r#"{"id":1,"name":"ada"}"#);
        let b = json(200, r#"{"id":1,"name":"ada"}"#);
        assert!(matches!(
            compare(&a, &b, &filter, 2048),
            Comparison::Match { .. }
        ));
    }

    #[test]
    fn json_object_key_order_is_normalized_away() {
        let filter = ParameterFilter::default();
        let a = json(200, r#"{"id":1,"name":"ada"}"#);
        let b = json(200, r#"{"name":"ada","id":1}"#);
        assert!(matches!(
            compare(&a, &b, &filter, 2048),
            Comparison::Match { .. }
        ));
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
        assert!(matches!(
            compare(&a, &b, &filter, 2048),
            Comparison::Match { .. }
        ));
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
    fn text_bodies_normalize_line_endings_and_outer_whitespace() {
        let filter = ParameterFilter::default();
        let text = |body: &str| ResponseFacts {
            status: 200,
            content_type: Some("text/plain".to_owned()),
            body: Bytes::from(body.to_owned()),
        };
        assert!(matches!(
            compare(
                &text("hello\r\nworld"),
                &text("  hello\nworld\n"),
                &filter,
                2048
            ),
            Comparison::Match { .. }
        ));
    }

    #[test]
    fn oversized_samples_are_truncated() {
        let filter = ParameterFilter::default();
        let big = format!(r#"{{"blob":"{}"}}"#, "x".repeat(4096));
        let Comparison::Diverged(d) = compare(&json(200, &big), &json(200, "{}"), &filter, 64)
        else {
            panic!("expected divergence");
        };
        let rendered = serde_json::to_string(&d.primary_sample).unwrap();
        assert!(
            rendered.len() < 512,
            "sample must be truncated: {}",
            rendered.len()
        );
    }

    #[test]
    fn empty_bodies_on_both_sides_match() {
        let filter = ParameterFilter::default();
        let head = || ResponseFacts {
            status: 204,
            content_type: None,
            body: Bytes::new(),
        };
        assert!(matches!(
            compare(&head(), &head(), &filter, 2048),
            Comparison::Match { .. }
        ));
    }

    #[test]
    fn invalid_json_falls_back_to_byte_comparison() {
        let filter = ParameterFilter::default();
        let a = json(200, "{not json");
        let b = json(200, "{not json");
        assert!(matches!(
            compare(&a, &b, &filter, 2048),
            Comparison::Match { .. }
        ));
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
        assert!(matches!(
            compare(&a, &b, &filter, 2048),
            Comparison::Match { .. }
        ));
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
