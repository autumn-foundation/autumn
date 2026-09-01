//! HTTP `Range` request support (RFC 7233) for in-memory and streamed bodies.
//!
//! This is the reusable core behind [`Download`](crate::download::Download)'s
//! ranged responses and the embedded static-asset path. It parses a single
//! `Range: bytes=…` header against a known total size and resolves it to a
//! byte interval, an "ignore the range" signal, or a "not satisfiable" signal —
//! then builds the matching `206 Partial Content` / `200 OK` / `416 Range Not
//! Satisfiable` response.
//!
//! # What it handles
//!
//! - `bytes=N-` — from byte `N` to the end.
//! - `bytes=N-M` — inclusive `[N, M]` (M clamped to `total-1`).
//! - `bytes=-N` — the final `N` bytes (a *suffix* range; `N` clamped to `total`).
//! - **Multi-range** (`bytes=0-50,100-150`): collapsed deterministically to the
//!   **first satisfiable** range and served as a single-range `206`. A true
//!   `multipart/byteranges` body is intentionally not emitted — the collapse
//!   guarantees a well-formed single-range response rather than a malformed
//!   multi-part one. See [`resolve`].
//! - **`If-Range`** (RFC 7233 §3.2): a strong entity-tag or an HTTP-date
//!   validator. When the client's validator is stale (or absent), the `Range`
//!   is ignored and the full `200` is served.
//!
//! Invalid or unparseable ranges are ignored (RFC 7233 §3.1: serve the whole
//! representation with `200`), never rejected.
//!
//! # Seekable video from a stored blob (AC #8)
//!
//! ```ignore
//! use autumn_web::download::Download;
//! use autumn_web::storage::SharedBlobStore;
//! use autumn_web::{secured, AutumnError};
//! use http::HeaderMap;
//!
//! // A browser `<video>` element issues `Range` requests to seek. Returning
//! // the download through `into_response_ranged` makes the stream seekable:
//! // the store is asked for only the requested byte slice.
//! #[secured(policy = "media.watch")]
//! async fn watch(
//!     store: SharedBlobStore,
//!     key: String,
//!     headers: HeaderMap,
//! ) -> Result<axum::response::Response, AutumnError> {
//!     Ok(Download::from_blob(&store, key)
//!         .await?
//!         .content_type("video/mp4")
//!         .inline()
//!         .into_response_ranged(&headers)
//!         .await)
//! }
//! ```

use axum::body::Body;
use axum::response::Response;
use bytes::Bytes;
use http::header::{ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, IF_RANGE, RANGE};
use http::{HeaderMap, HeaderValue, StatusCode};

use crate::etag::ETag;

/// The resolved outcome of a `Range` request against a known total size.
///
/// `start` and `end` are **inclusive** byte offsets, matching the HTTP
/// `Content-Range` convention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RangeResolution {
    /// Serve the whole representation with `200 OK`. Produced when there is no
    /// (valid) `Range` header, the range unit is not `bytes`, the range cannot
    /// be parsed, or an `If-Range` validator did not match.
    Full,
    /// Serve the inclusive byte interval `[start, end]` with `206 Partial
    /// Content`.
    Partial {
        /// First byte offset (inclusive).
        start: u64,
        /// Last byte offset (inclusive).
        end: u64,
        /// Total size of the full representation.
        total: u64,
    },
    /// The requested range cannot be satisfied (start past EOF, empty suffix,
    /// or an empty representation). Serve `416 Range Not Satisfiable` with
    /// `Content-Range: bytes */total`.
    Unsatisfiable {
        /// Total size of the full representation.
        total: u64,
    },
}

/// The current validators for an `If-Range` conditional range request.
///
/// Carries the resource's current **strong** [`ETag`] and/or its
/// `Last-Modified` HTTP-date string. When a request includes `If-Range`, the
/// range is honoured only if the client's validator still matches; otherwise
/// the full representation is returned (see [`resolve`]).
#[derive(Debug, Clone, Copy, Default)]
pub struct Validator<'a> {
    /// The resource's current strong entity-tag, if any.
    etag: Option<&'a ETag>,
    /// The resource's current `Last-Modified` value as an HTTP-date string.
    last_modified: Option<&'a str>,
}

impl<'a> Validator<'a> {
    /// An empty validator (matches nothing — any `If-Range` falls back to `200`).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            etag: None,
            last_modified: None,
        }
    }

    /// Attach the resource's current strong [`ETag`].
    #[must_use]
    pub const fn with_etag(mut self, etag: &'a ETag) -> Self {
        self.etag = Some(etag);
        self
    }

    /// Attach the resource's current `Last-Modified` HTTP-date string.
    #[must_use]
    pub const fn with_last_modified(mut self, last_modified: &'a str) -> Self {
        self.last_modified = Some(last_modified);
        self
    }
}

/// Resolve a request's `Range` header against a `total` byte size.
///
/// Returns a [`RangeResolution`]:
///
/// - No `Range` header, a non-`bytes` unit, or an unparseable value → [`Full`]
///   (RFC 7233 §3.1: ignore an invalid range, serve `200`).
/// - An `If-Range` header whose validator is stale or unmatched → [`Full`].
/// - A satisfiable range → [`Partial`], carrying the inclusive `[start, end]`.
/// - A syntactically valid but unsatisfiable range (start ≥ `total`, a zero
///   suffix, or `total == 0`) → [`Unsatisfiable`].
///
/// # Multi-range collapse
///
/// A multi-range request (`bytes=0-50,100-150`) is collapsed to the **first
/// satisfiable** sub-range and served as a single-range `206`. This is
/// deterministic and always well-formed; a `multipart/byteranges` body is
/// deliberately not produced.
///
/// [`Full`]: RangeResolution::Full
/// [`Partial`]: RangeResolution::Partial
/// [`Unsatisfiable`]: RangeResolution::Unsatisfiable
#[must_use]
pub fn resolve(
    req_headers: &HeaderMap,
    total: u64,
    validator: Option<Validator<'_>>,
) -> RangeResolution {
    let Some(range_value) = req_headers.get(RANGE) else {
        return RangeResolution::Full;
    };
    let Ok(range_str) = range_value.to_str() else {
        return RangeResolution::Full;
    };

    // RFC 7233 §3.2: an unmatched (or absent-validator) `If-Range` means the
    // client's cached copy is stale, so the `Range` MUST be ignored and the
    // full representation returned.
    if let Some(if_range) = req_headers.get(IF_RANGE).and_then(|v| v.to_str().ok())
        && !if_range_matches(if_range, validator.as_ref())
    {
        return RangeResolution::Full;
    }

    parse_range(range_str, total)
}

/// Outcome of parsing a single byte-range spec.
enum SpecOutcome {
    /// Syntactically invalid — ignore this spec (RFC 7233 §3.1).
    Invalid,
    /// A satisfiable inclusive interval.
    Satisfiable { start: u64, end: u64 },
    /// Syntactically valid but not satisfiable against the total size.
    Unsatisfiable,
}

/// Parse a `bytes=…` header value, applying the single-range collapse.
fn parse_range(range_str: &str, total: u64) -> RangeResolution {
    let Some(rest) = range_str.trim().strip_prefix("bytes=") else {
        // Non-`bytes` unit or malformed prefix → ignore, serve full.
        return RangeResolution::Full;
    };

    let mut saw_unsatisfiable = false;

    for spec in rest.split(',') {
        let spec = spec.trim();
        if spec.is_empty() {
            continue;
        }
        match parse_single_spec(spec, total) {
            SpecOutcome::Invalid => {}
            SpecOutcome::Satisfiable { start, end } => {
                // First satisfiable range wins (single-range collapse).
                return RangeResolution::Partial { start, end, total };
            }
            SpecOutcome::Unsatisfiable => {
                saw_unsatisfiable = true;
            }
        }
    }

    if saw_unsatisfiable {
        RangeResolution::Unsatisfiable { total }
    } else {
        // No syntactically valid range at all → ignore, serve full.
        RangeResolution::Full
    }
}

/// Parse one byte-range spec (`N-`, `N-M`, or `-N`) against `total`.
fn parse_single_spec(spec: &str, total: u64) -> SpecOutcome {
    // Suffix form: `-N` → the final N bytes.
    if let Some(suffix) = spec.strip_prefix('-') {
        let Ok(n) = suffix.trim().parse::<u64>() else {
            return SpecOutcome::Invalid;
        };
        if n == 0 || total == 0 {
            // A zero-length suffix (or empty representation) is unsatisfiable.
            return SpecOutcome::Unsatisfiable;
        }
        // Clamp N ≤ total → start at 0.
        let start = total.saturating_sub(n);
        return SpecOutcome::Satisfiable {
            start,
            end: total - 1,
        };
    }

    let Some((start_s, end_s)) = spec.split_once('-') else {
        return SpecOutcome::Invalid;
    };
    let Ok(start) = start_s.trim().parse::<u64>() else {
        return SpecOutcome::Invalid;
    };

    // `N-` → from N to the end.
    if end_s.trim().is_empty() {
        if total == 0 || start >= total {
            return SpecOutcome::Unsatisfiable;
        }
        return SpecOutcome::Satisfiable {
            start,
            end: total - 1,
        };
    }

    // `N-M` → inclusive [N, min(M, total-1)].
    let Ok(end) = end_s.trim().parse::<u64>() else {
        return SpecOutcome::Invalid;
    };
    if start > end {
        // Backwards range (`5-2`) is invalid — ignore this spec.
        return SpecOutcome::Invalid;
    }
    if total == 0 || start >= total {
        return SpecOutcome::Unsatisfiable;
    }
    SpecOutcome::Satisfiable {
        start,
        end: end.min(total - 1),
    }
}

/// Whether a client `If-Range` validator still matches the current resource.
///
/// `If-Range` may be a strong entity-tag or an HTTP-date. A weak entity-tag can
/// never match (RFC 7233 §3.2 forbids weak validators here). A missing
/// validator never matches.
fn if_range_matches(if_range: &str, validator: Option<&Validator<'_>>) -> bool {
    let Some(validator) = validator else {
        return false;
    };
    let trimmed = if_range.trim();

    // Entity-tag form starts with a quote or the weak prefix.
    if trimmed.starts_with('"') || trimmed.starts_with("W/") {
        // Weak validators are not usable for `If-Range`.
        if trimmed.starts_with("W/") {
            return false;
        }
        let Some(etag) = validator.etag else {
            return false;
        };
        if etag.is_weak() {
            return false;
        }
        trimmed.trim_matches('"') == etag.tag()
    } else {
        // HTTP-date form: must equal the current `Last-Modified` exactly.
        // Clients echo the value verbatim, so a byte comparison is correct and
        // avoids a second date-parsing dependency.
        validator
            .last_modified
            .is_some_and(|lm| lm.trim() == trimmed)
    }
}

// ── Response builders ───────────────────────────────────────────────────────

/// Slice the inclusive byte interval `[start, end]` out of `full`.
///
/// `start`/`end` are `u64` offsets that were validated against the total size;
/// this clamps defensively (and via `try_from`, not `as`, so it never truncates
/// on a 32-bit target) so a bad caller can never panic on slice bounds.
fn slice_inclusive(full: &Bytes, start: u64, end: u64) -> Bytes {
    if full.is_empty() {
        return Bytes::new();
    }
    let last = full.len() - 1;
    let lo = usize::try_from(start).unwrap_or(usize::MAX).min(last);
    let hi = usize::try_from(end).unwrap_or(usize::MAX).min(last);
    if lo > hi {
        return Bytes::new();
    }
    full.slice(lo..=hi)
}

/// Build the in-memory response for a resolved range over `full` bytes.
///
/// - [`Full`](RangeResolution::Full) → `200 OK`, the whole body,
///   `Accept-Ranges: bytes`, `Content-Length` = `full.len()`.
/// - [`Partial`](RangeResolution::Partial) → `206 Partial Content`, the sliced
///   body, `Accept-Ranges: bytes`, `Content-Range: bytes start-end/total`,
///   `Content-Length` = slice length.
/// - [`Unsatisfiable`](RangeResolution::Unsatisfiable) → `416 Range Not
///   Satisfiable`, empty body, `Content-Range: bytes */total`.
///
/// The caller layers `Content-Type` / `Content-Disposition` on top.
#[must_use]
pub fn partial_bytes_response(resolution: &RangeResolution, full: Bytes) -> Response<Body> {
    match *resolution {
        RangeResolution::Full => {
            let len = full.len();
            let mut response = Response::new(Body::from(full));
            set_accept_ranges(response.headers_mut());
            response
                .headers_mut()
                .insert(CONTENT_LENGTH, HeaderValue::from(len));
            response
        }
        RangeResolution::Partial { start, end, total } => {
            let slice = slice_inclusive(&full, start, end);
            let len = slice.len();
            let mut response = Response::new(Body::from(slice));
            *response.status_mut() = StatusCode::PARTIAL_CONTENT;
            let headers = response.headers_mut();
            set_accept_ranges(headers);
            if let Ok(v) = HeaderValue::from_str(&content_range_value(start, end, total)) {
                headers.insert(CONTENT_RANGE, v);
            }
            headers.insert(CONTENT_LENGTH, HeaderValue::from(len));
            response
        }
        RangeResolution::Unsatisfiable { total } => {
            let mut response = Response::new(Body::empty());
            *response.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
            let headers = response.headers_mut();
            set_accept_ranges(headers);
            if let Ok(v) = HeaderValue::from_str(&unsatisfied_content_range(total)) {
                headers.insert(CONTENT_RANGE, v);
            }
            headers.insert(CONTENT_LENGTH, HeaderValue::from(0));
            response
        }
    }
}

/// Format a `Content-Range: bytes start-end/total` value (`start`/`end`
/// inclusive).
#[must_use]
pub fn content_range_value(start: u64, end: u64, total: u64) -> String {
    format!("bytes {start}-{end}/{total}")
}

/// Format an unsatisfied `Content-Range: bytes */total` value for a `416`.
#[must_use]
pub fn unsatisfied_content_range(total: u64) -> String {
    format!("bytes */{total}")
}

/// Set `Accept-Ranges: bytes` on a header map, advertising range support.
pub fn set_accept_ranges(headers: &mut HeaderMap) {
    headers.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers_with_range(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(RANGE, HeaderValue::from_str(value).unwrap());
        h
    }

    // ── parser table ────────────────────────────────────────────────────────

    #[test]
    fn no_range_header_is_full() {
        assert_eq!(resolve(&HeaderMap::new(), 10, None), RangeResolution::Full);
    }

    #[test]
    fn simple_closed_range() {
        assert_eq!(
            resolve(&headers_with_range("bytes=0-3"), 10, None),
            RangeResolution::Partial {
                start: 0,
                end: 3,
                total: 10
            }
        );
    }

    #[test]
    fn open_ended_range_extends_to_eof() {
        assert_eq!(
            resolve(&headers_with_range("bytes=5-"), 10, None),
            RangeResolution::Partial {
                start: 5,
                end: 9,
                total: 10
            }
        );
    }

    #[test]
    fn suffix_range_takes_last_n_bytes() {
        assert_eq!(
            resolve(&headers_with_range("bytes=-4"), 10, None),
            RangeResolution::Partial {
                start: 6,
                end: 9,
                total: 10
            }
        );
    }

    #[test]
    fn suffix_range_clamps_when_larger_than_total() {
        assert_eq!(
            resolve(&headers_with_range("bytes=-100"), 10, None),
            RangeResolution::Partial {
                start: 0,
                end: 9,
                total: 10
            }
        );
    }

    #[test]
    fn closed_end_is_clamped_to_last_byte() {
        assert_eq!(
            resolve(&headers_with_range("bytes=2-999"), 10, None),
            RangeResolution::Partial {
                start: 2,
                end: 9,
                total: 10
            }
        );
    }

    #[test]
    fn multi_range_collapses_to_first_satisfiable() {
        assert_eq!(
            resolve(&headers_with_range("bytes=0-3,5-7"), 10, None),
            RangeResolution::Partial {
                start: 0,
                end: 3,
                total: 10
            }
        );
    }

    #[test]
    fn multi_range_skips_leading_unsatisfiable() {
        // First spec is past EOF; the collapse picks the next satisfiable one.
        assert_eq!(
            resolve(&headers_with_range("bytes=100-200,2-4"), 10, None),
            RangeResolution::Partial {
                start: 2,
                end: 4,
                total: 10
            }
        );
    }

    #[test]
    fn start_beyond_eof_is_unsatisfiable() {
        assert_eq!(
            resolve(&headers_with_range("bytes=100-"), 10, None),
            RangeResolution::Unsatisfiable { total: 10 }
        );
    }

    #[test]
    fn non_numeric_range_is_full() {
        assert_eq!(
            resolve(&headers_with_range("bytes=abc"), 10, None),
            RangeResolution::Full
        );
    }

    #[test]
    fn suffix_range_zero_total_is_unsatisfiable() {
        assert_eq!(
            resolve(&headers_with_range("bytes=-5"), 0, None),
            RangeResolution::Unsatisfiable { total: 0 }
        );
    }

    #[test]
    fn open_ended_range_zero_total_is_unsatisfiable() {
        assert_eq!(
            resolve(&headers_with_range("bytes=5-"), 0, None),
            RangeResolution::Unsatisfiable { total: 0 }
        );
    }

    #[test]
    fn simple_closed_range_zero_total_is_unsatisfiable() {
        assert_eq!(
            resolve(&headers_with_range("bytes=0-5"), 0, None),
            RangeResolution::Unsatisfiable { total: 0 }
        );
    }

    #[test]
    fn range_with_invalid_start_is_full() {
        assert_eq!(
            resolve(&headers_with_range("bytes=abc-10"), 10, None),
            RangeResolution::Full
        );
    }

    #[test]
    fn range_with_invalid_end_is_full() {
        assert_eq!(
            resolve(&headers_with_range("bytes=0-abc"), 10, None),
            RangeResolution::Full
        );
    }

    #[test]
    fn suffix_range_with_invalid_number_is_full() {
        assert_eq!(
            resolve(&headers_with_range("bytes=-abc"), 10, None),
            RangeResolution::Full
        );
    }

    #[test]
    fn multi_range_with_empty_specs_are_ignored() {
        assert_eq!(
            resolve(&headers_with_range("bytes=0-3,,5-7"), 10, None),
            RangeResolution::Partial {
                start: 0,
                end: 3,
                total: 10
            }
        );
    }

    #[test]
    fn multi_range_all_empty_specs_is_full() {
        assert_eq!(
            resolve(&headers_with_range("bytes=,,,"), 10, None),
            RangeResolution::Full
        );
    }

    #[test]
    fn suffix_range_matching_total() {
        assert_eq!(
            resolve(&headers_with_range("bytes=-10"), 10, None),
            RangeResolution::Partial {
                start: 0,
                end: 9,
                total: 10
            }
        );
    }

    #[test]
    fn backwards_range_is_full() {
        assert_eq!(
            resolve(&headers_with_range("bytes=5-2"), 10, None),
            RangeResolution::Full
        );
    }

    #[test]
    fn non_bytes_unit_is_full() {
        assert_eq!(
            resolve(&headers_with_range("items=0-3"), 10, None),
            RangeResolution::Full
        );
    }

    #[test]
    fn zero_total_range_is_unsatisfiable() {
        assert_eq!(
            resolve(&headers_with_range("bytes=0-3"), 0, None),
            RangeResolution::Unsatisfiable { total: 0 }
        );
    }

    #[test]
    fn zero_suffix_is_unsatisfiable() {
        assert_eq!(
            resolve(&headers_with_range("bytes=-0"), 10, None),
            RangeResolution::Unsatisfiable { total: 10 }
        );
    }

    // ── If-Range ──────────────────────────────────────────────────────────────

    #[test]
    fn if_range_matching_etag_honours_range() {
        let etag = ETag::strong("v1");
        let mut h = headers_with_range("bytes=0-3");
        h.insert(IF_RANGE, etag.header_value());
        let validator = Validator::new().with_etag(&etag);
        assert_eq!(
            resolve(&h, 10, Some(validator)),
            RangeResolution::Partial {
                start: 0,
                end: 3,
                total: 10
            }
        );
    }

    #[test]
    fn if_range_stale_etag_falls_back_to_full() {
        let current = ETag::strong("v2");
        let mut h = headers_with_range("bytes=0-3");
        h.insert(IF_RANGE, HeaderValue::from_static("\"v1\""));
        let validator = Validator::new().with_etag(&current);
        assert_eq!(resolve(&h, 10, Some(validator)), RangeResolution::Full);
    }

    #[test]
    fn if_range_weak_etag_never_matches() {
        let weak = ETag::weak("v1");
        let mut h = headers_with_range("bytes=0-3");
        h.insert(IF_RANGE, HeaderValue::from_static("W/\"v1\""));
        let validator = Validator::new().with_etag(&weak);
        assert_eq!(resolve(&h, 10, Some(validator)), RangeResolution::Full);
    }

    #[test]
    fn if_range_matching_last_modified_honours_range() {
        let lm = "Wed, 21 Oct 2015 07:28:00 GMT";
        let mut h = headers_with_range("bytes=0-3");
        h.insert(
            IF_RANGE,
            HeaderValue::from_static("Wed, 21 Oct 2015 07:28:00 GMT"),
        );
        let validator = Validator::new().with_last_modified(lm);
        assert_eq!(
            resolve(&h, 10, Some(validator)),
            RangeResolution::Partial {
                start: 0,
                end: 3,
                total: 10
            }
        );
    }

    #[test]
    fn if_range_stale_last_modified_falls_back_to_full() {
        let lm = "Wed, 21 Oct 2015 07:28:00 GMT";
        let mut h = headers_with_range("bytes=0-3");
        h.insert(
            IF_RANGE,
            HeaderValue::from_static("Tue, 20 Oct 2015 00:00:00 GMT"),
        );
        let validator = Validator::new().with_last_modified(lm);
        assert_eq!(resolve(&h, 10, Some(validator)), RangeResolution::Full);
    }

    #[test]
    fn if_range_without_validator_falls_back_to_full() {
        let mut h = headers_with_range("bytes=0-3");
        h.insert(IF_RANGE, HeaderValue::from_static("\"v1\""));
        assert_eq!(resolve(&h, 10, None), RangeResolution::Full);
    }

    #[test]
    fn if_range_missing_falls_through_to_range() {
        let h = headers_with_range("bytes=0-3");
        let etag = ETag::strong("v1");
        let validator = Validator::new().with_etag(&etag);
        assert_eq!(
            resolve(&h, 10, Some(validator)),
            RangeResolution::Partial {
                start: 0,
                end: 3,
                total: 10
            }
        );
    }

    #[test]
    fn if_range_matching_last_modified_with_whitespace() {
        let lm = "Wed, 21 Oct 2015 07:28:00 GMT";
        let mut h = headers_with_range("bytes=0-3");
        h.insert(
            IF_RANGE,
            HeaderValue::from_static("  Wed, 21 Oct 2015 07:28:00 GMT  "),
        );
        let validator = Validator::new().with_last_modified(lm);
        assert_eq!(
            resolve(&h, 10, Some(validator)),
            RangeResolution::Partial {
                start: 0,
                end: 3,
                total: 10
            }
        );
    }

    // ── slice_inclusive ───────────────────────────────────────────────────────

    #[test]
    fn slice_inclusive_empty_returns_empty() {
        assert_eq!(slice_inclusive(&Bytes::new(), 0, 10).len(), 0);
    }

    #[test]
    fn slice_inclusive_out_of_bounds_clamps() {
        let full = Bytes::from_static(b"0123456789");
        assert_eq!(&slice_inclusive(&full, 5, 20)[..], b"56789");
    }

    #[test]
    fn slice_inclusive_start_greater_than_end() {
        let full = Bytes::from_static(b"0123456789");
        assert_eq!(slice_inclusive(&full, 5, 2).len(), 0);
    }

    #[test]
    fn slice_inclusive_start_greater_than_total() {
        let full = Bytes::from_static(b"0123456789");
        assert_eq!(&slice_inclusive(&full, 20, 30)[..], b"9");
    }

    // ── response builders ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn partial_response_slices_body_and_sets_headers() {
        use http_body_util::BodyExt as _;

        let full = Bytes::from_static(b"0123456789");
        let resolution = RangeResolution::Partial {
            start: 2,
            end: 5,
            total: 10,
        };
        let resp = partial_bytes_response(&resolution, full);
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(resp.headers().get(CONTENT_RANGE).unwrap(), "bytes 2-5/10");
        assert_eq!(resp.headers().get(CONTENT_LENGTH).unwrap(), "4");
        assert_eq!(resp.headers().get(ACCEPT_RANGES).unwrap(), "bytes");
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"2345");
    }

    #[tokio::test]
    async fn full_response_sets_accept_ranges_and_length() {
        let full = Bytes::from_static(b"0123456789");
        let resp = partial_bytes_response(&RangeResolution::Full, full);
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers().get(ACCEPT_RANGES).unwrap(), "bytes");
        assert_eq!(resp.headers().get(CONTENT_LENGTH).unwrap(), "10");
    }

    #[tokio::test]
    async fn unsatisfiable_response_is_416_with_star_content_range() {
        let full = Bytes::from_static(b"0123456789");
        let resp = partial_bytes_response(&RangeResolution::Unsatisfiable { total: 10 }, full);
        assert_eq!(resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(resp.headers().get(CONTENT_RANGE).unwrap(), "bytes */10");
    }

    #[test]
    fn content_range_helpers_format_correctly() {
        assert_eq!(content_range_value(0, 3, 10), "bytes 0-3/10");
        assert_eq!(unsatisfied_content_range(10), "bytes */10");
    }
}
