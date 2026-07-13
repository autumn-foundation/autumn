//! Exception filter that renders HTML error pages for browser requests.
//!
//! When a request's `Accept` header prefers HTML over JSON (typical for
//! browser navigation), this filter replaces the default JSON error
//! response with a styled HTML error page. JSON API requests are left
//! untouched.
//!
//! In dev mode, a Next.js-style error badge is injected into the HTML
//! response for quick debugging.

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

use axum::response::{IntoResponse, Response};

#[cfg(feature = "maud")]
use crate::error_pages::dev_badge::{self, DevBadgeContext};
#[cfg(feature = "maud")]
use crate::error_pages::renderer::ErrorContext;
#[cfg(feature = "maud")]
use crate::error_pages::{self, SharedRenderer};
#[cfg(feature = "maud")]
use crate::middleware::exception_filter::AutumnErrorInfo;
#[cfg(feature = "maud")]
use crate::middleware::exception_filter::ExceptionFilter;

/// Exception filter that renders HTML error pages for browser requests.
///
/// Injected automatically by the framework. When a request has an `Accept`
/// header indicating HTML preference (browser navigation), the JSON error
/// response is replaced with a styled HTML page.
///
/// In dev profile, the HTML page includes a floating error badge overlay.
///
/// Only available when the `maud` feature is enabled.
#[cfg(feature = "maud")]
pub struct ErrorPageFilter {
    pub renderer: SharedRenderer,
    pub is_dev: bool,
    pub parameter_filter: crate::log::filter::ParameterFilter,
}

#[cfg(feature = "maud")]
impl ExceptionFilter for ErrorPageFilter {
    fn filter(&self, error: &AutumnErrorInfo, response: Response) -> Response {
        let wants_html = response
            .extensions()
            .get::<WantsHtml>()
            .is_some_and(|w| w.0);

        if !wants_html {
            return response;
        }

        // Preserve the headers already attached to the error response —
        // `build_html_response` constructs a fresh response, so without this the
        // rebuilt HTML error page would drop them. Two matter in particular:
        //   * `X-Request-Id` (stamped by `RequestIdLayer`) — operators need it to
        //     tie the page to their logs.
        //   * CORS headers mirrored onto a timeout 503 by the request-timeout
        //     middleware (`ProblemDetailsFilter` preserves these too). A
        //     cross-origin HTML/HTMX request that times out would otherwise see an
        //     opaque CORS failure instead of the styled error page.
        // Body-representation headers (`Content-Type`/`Content-Length`/
        // `Content-Encoding`/...) are dropped because this fresh, uncompressed
        // HTML body sets its own — see `preserved_error_headers`.
        let preserved_headers =
            crate::middleware::exception_filter::preserved_error_headers(&response);

        let ctx = Self::build_error_context(error, &response, self.is_dev);
        let mut html_body =
            error_pages::render_error_page(self.renderer.as_ref(), error.status, &ctx)
                .into_string();

        if self.is_dev {
            self.inject_dev_badge(&mut html_body, error, &ctx, &response);
        }

        let mut html = Self::build_html_response(error, html_body);
        html.headers_mut().extend(preserved_headers);
        html
    }
}

/// Marker stored in response extensions to indicate the original request
/// preferred HTML responses.
#[derive(Clone, Debug)]
pub struct WantsHtml(pub bool);

/// Request context stored in response extensions for the error page filter.
#[derive(Clone, Debug)]
pub struct ErrorPageRequestContext {
    pub uri: axum::http::Uri,
    pub request_id: Option<String>,
    pub query: Option<String>,
    pub headers: Option<axum::http::HeaderMap>,
    /// HTTP method (e.g. "GET").
    pub method: Option<String>,
    /// Matched route pattern (e.g. `/posts/{id}`), set by `DevRouteInfoLayer` in dev mode.
    pub matched_path: Option<String>,
    /// Scrubbed cookies parsed from the Cookie header.
    pub cookies: Option<serde_json::Value>,
    /// Raw path parameters (e.g. `{"id": "42"}`), captured in dev mode.
    pub path_params: Option<serde_json::Value>,
    /// SQL queries recorded during this request by `InspectorLayer` (dev only).
    pub sql_queries: Vec<crate::inspector::QueryRecord>,
    /// Buffered request body bytes (dev mode only, capped at 64 KB).
    pub body_bytes: Option<bytes::Bytes>,
    /// `Content-Type` header value at the time of buffering (dev mode only).
    pub content_type: Option<String>,
    /// True when the body exceeded the capture limit and was truncated.
    pub body_truncated: bool,
}

/// Max bytes captured from the request body for the dev overlay.
/// Bodies larger than this will show a truncation notice.
const BODY_CAPTURE_LIMIT: usize = 64 * 1024;

/// Tower layer that annotates requests with Accept header preference
/// so the error page filter knows whether to render HTML or pass through JSON.
///
/// This layer runs before the exception filter and stores [`WantsHtml`]
/// and [`ErrorPageRequestContext`] in the response extensions.
#[derive(Clone)]
pub struct ErrorPageContextLayer {
    pub is_dev: bool,
}

impl<S> tower::Layer<S> for ErrorPageContextLayer {
    type Service = ErrorPageContextService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        ErrorPageContextService {
            inner,
            is_dev: self.is_dev,
        }
    }
}

#[derive(Clone)]
pub struct ErrorPageContextService<S> {
    inner: S,
    is_dev: bool,
}

impl<S> tower::Service<axum::http::Request<axum::body::Body>> for ErrorPageContextService<S>
where
    S: tower::Service<axum::http::Request<axum::body::Body>, Response = Response>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
{
    type Response = Response;
    type Error = S::Error;
    type Future =
        std::pin::Pin<Box<dyn std::future::Future<Output = Result<Response, S::Error>> + Send>>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: axum::http::Request<axum::body::Body>) -> Self::Future {
        let wants_html = accepts_html(&req);
        let uri = req.uri().clone();
        let request_id = req
            .extensions()
            .get::<crate::middleware::RequestId>()
            .map(std::string::ToString::to_string);

        let query = uri.query().map(str::to_owned);
        let headers = Some(req.headers().clone());
        let method = Some(req.method().to_string());
        let is_dev = self.is_dev;
        let captures_body = is_dev && should_capture_body(method.as_deref());
        let content_type = if captures_body {
            req.headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        } else {
            None
        };

        // Use clone-and-replace so we can move inner into the async block while
        // still having called poll_ready on the original.
        let cloned = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, cloned);

        Box::pin(async move {
            let (req, body_bytes, body_truncated) = if captures_body {
                // Check Content-Length first to avoid consuming the body when it's
                // clearly over the cap (preserving the stream for the inner service).
                let declared_len = req
                    .headers()
                    .get(axum::http::header::CONTENT_LENGTH)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<usize>().ok());

                if declared_len.is_none_or(|len| len > BODY_CAPTURE_LIMIT) {
                    // Body size is unknown or known to be too large — don't consume
                    // the stream at all so the downstream handler receives it intact.
                    (
                        req,
                        None,
                        declared_len.is_some_and(|len| len > BODY_CAPTURE_LIMIT),
                    )
                } else {
                    // Content-Length is present and within cap; read the full body
                    // so we can reconstruct it exactly for the downstream handler.
                    let (parts, body) = req.into_parts();
                    let bytes = axum::body::to_bytes(body, BODY_CAPTURE_LIMIT + 1)
                        .await
                        .unwrap_or_default();
                    let new_body = axum::body::Body::from(bytes.clone());
                    let req = axum::http::Request::from_parts(parts, new_body);
                    (req, Some(bytes), false)
                }
            } else {
                (req, None, false)
            };

            let mut response = inner.call(req).await?;

            response.extensions_mut().insert(WantsHtml(wants_html));
            let request_id = request_id.or_else(|| {
                response
                    .headers()
                    .get("x-request-id")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned)
            });
            let matched_path = response
                .extensions()
                .get::<crate::middleware::dev::DevMatchedPath>()
                .map(|m| m.0.clone());
            let path_params = matched_path
                .as_deref()
                .map(|pattern| extract_path_params(pattern, uri.path()));
            let cookies = headers
                .as_ref()
                .and_then(|h| h.get(axum::http::header::COOKIE))
                .and_then(|v| v.to_str().ok())
                .map(parse_cookie_header);
            let sql_queries = response
                .extensions()
                .get::<crate::inspector::RequestQueryList>()
                .map(crate::inspector::RequestQueryList::snapshot)
                .unwrap_or_default();
            response.extensions_mut().insert(ErrorPageRequestContext {
                uri,
                request_id,
                query,
                headers,
                method,
                matched_path,
                cookies,
                path_params,
                sql_queries,
                body_bytes,
                content_type,
                body_truncated,
            });
            Ok(response)
        })
    }
}

/// Returns true for HTTP methods that typically carry a request body.
fn should_capture_body(method: Option<&str>) -> bool {
    matches!(method, Some("POST" | "PUT" | "PATCH"))
}

/// Check if the request's Accept header indicates a preference for HTML.
///
/// Returns `true` for typical browser requests (`text/html` or `*/*`),
/// `false` for API requests (`application/json`).
fn accepts_html<B>(req: &axum::http::Request<B>) -> bool {
    accept_prefers_html(req.headers())
}

/// Which representation the client's `Accept` header prefers.
///
/// Produced by the single canonical negotiator [`accept_preference`]; the
/// public [`accept_prefers_html`] wrapper is defined in terms of it so all
/// content negotiation in the crate shares one source of truth.
// `pub` here is crate-visible only: the enclosing `error_page_filter` module is
// itself `pub(crate)`, so this never escapes the crate (clippy::redundant_pub_crate).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcceptPreference {
    /// `text/html` wins (explicitly or via q-values).
    Html,
    /// `application/json` (or `application/problem+json`) wins.
    Json,
    /// Only `*/*` present — no concrete html/json preference.
    Any,
    /// No `Accept` header, or it was empty.
    Unspecified,
}

/// Check whether an Accept header prefers an HTML response over JSON.
pub fn accept_prefers_html(headers: &axum::http::HeaderMap) -> bool {
    matches!(
        accept_preference(headers),
        AcceptPreference::Html | AcceptPreference::Any
    )
}

/// Best `(q-value, list-index)` seen for each media type the crate negotiates
/// on, parsed from a request's `Accept` header by the one canonical parser
/// [`accept_qualities`].
///
/// `index` is the position of the winning entry in the comma-separated header
/// (0-based), used to break q-value ties in favour of the earlier entry. Each
/// field is `None` only when that media type was entirely absent from the
/// header; a media type listed with `q=0` records `Some((0.0, index))` so an
/// explicit "not acceptable" exclusion (RFC 7231 §5.3.1) stays distinguishable
/// from a mere absence. Both [`accept_preference`] (legacy enum resolution) and
/// the
/// [`Negotiate`](crate::negotiate::Negotiate) responder (effective-q
/// resolution) build on this shared parse, so the crate never forks its
/// `Accept` parsing.
// `pub` (not `pub(crate)`): the enclosing `error_page_filter` module is itself
// `pub(crate)`, so these never escape the crate (clippy::redundant_pub_crate).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AcceptQualities {
    /// Best match for `text/html`.
    pub html: Option<(f32, usize)>,
    /// Best match for the success JSON type `application/json` (only).
    ///
    /// Kept separate from [`Self::problem_json`] so the
    /// [`Negotiate`](crate::negotiate::Negotiate) responder can negotiate the
    /// success `application/json` body without treating the error-only
    /// `application/problem+json` signal as accepting it.
    pub json: Option<(f32, usize)>,
    /// Best match for `application/problem+json`.
    ///
    /// `application/problem+json` is an **error-path** (Problem Details) signal,
    /// not a success representation. The legacy
    /// [`accept_preference`]/[`accept_prefers_html`] path folds it into the JSON
    /// preference (so a client asking for problem+json still gets a JSON error
    /// body rather than an HTML error page), but the `Negotiate` success
    /// responder deliberately ignores it: advertising problem+json alone does
    /// **not** count as accepting the `application/json` success body.
    pub problem_json: Option<(f32, usize)>,
    /// Best match for the `text/*` subtype wildcard.
    ///
    /// A media range like `text/*` covers `text/html` per RFC 7231 §5.3.2 but is
    /// less specific than a concrete `text/html`. Recorded like the other slots
    /// (max-q including `q=0`, plus list index) so the `Negotiate` responder can
    /// slot it between the concrete type and the `*/*` wildcard. The legacy
    /// `accept_preference`/`accept_prefers_html` path ignores it.
    pub text_star: Option<(f32, usize)>,
    /// Best match for the `application/*` subtype wildcard.
    ///
    /// A media range like `application/*` covers `application/json` per RFC 7231
    /// §5.3.2 but is less specific than a concrete `application/json`. Recorded
    /// like the other slots (max-q including `q=0`, plus list index) so the
    /// `Negotiate` responder can slot it between the concrete type and the `*/*`
    /// wildcard. The legacy `accept_preference`/`accept_prefers_html` path
    /// ignores it.
    pub application_star: Option<(f32, usize)>,
    /// Best match for the `*/*` wildcard.
    pub wildcard: Option<(f32, usize)>,
}

/// The one canonical `Accept` parser.
///
/// Scans the comma-separated `Accept` header once, recording the best
/// `(q-value, list-index)` seen for `text/html`, for `application/json`, for
/// `application/problem+json` (tracked in its own slot — an error-path signal
/// that does not drive success JSON negotiation), for the `text/*` and
/// `application/*` subtype wildcards, and for the `*/*` wildcard. Out-of-range
/// q-values are clamped to `[0.0, 1.0]`. Entries with `q=0` are *retained*
/// (recorded as `Some((0.0, index))`) so an explicit "not acceptable"
/// exclusion survives; each consumer decides how to treat them.
///
/// This is the crate's single source of truth for `Accept` tokenisation; the
/// two consumers apply their own *resolution policy* on top of the returned
/// [`AcceptQualities`] (they differ only in policy, not parsing).
pub fn accept_qualities(headers: &axum::http::HeaderMap) -> AcceptQualities {
    let accept = headers
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let mut qualities = AcceptQualities::default();

    for (index, raw_part) in accept.split(',').enumerate() {
        let part = raw_part.trim();
        if part.is_empty() {
            continue;
        }

        let mut mime = "";
        let mut q = 1.0_f32;

        for (i, segment) in part.split(';').enumerate() {
            let segment = segment.trim();
            if i == 0 {
                mime = segment;
                continue;
            }

            if let Some(value) = segment.strip_prefix("q=")
                && let Ok(parsed) = value.trim().parse::<f32>()
            {
                q = parsed.clamp(0.0, 1.0);
            }
        }

        // Record the entry even when `q == 0`: an explicit `q=0` means the type
        // is *not acceptable* (RFC 7231 §5.3.1), which is distinct from the type
        // being absent. Consumers that only care about positive preference (the
        // legacy `accept_preference`/`accept_prefers_html` path) filter these
        // `q == 0` slots back out; the `Negotiate` responder honours them as
        // explicit exclusions.
        match mime {
            "text/html" if qualities.html.is_none_or(|(existing_q, _)| q > existing_q) => {
                qualities.html = Some((q, index));
            }
            "application/json" if qualities.json.is_none_or(|(existing_q, _)| q > existing_q) => {
                qualities.json = Some((q, index));
            }
            "application/problem+json"
                if qualities
                    .problem_json
                    .is_none_or(|(existing_q, _)| q > existing_q) =>
            {
                qualities.problem_json = Some((q, index));
            }
            "text/*"
                if qualities
                    .text_star
                    .is_none_or(|(existing_q, _)| q > existing_q) =>
            {
                qualities.text_star = Some((q, index));
            }
            "application/*"
                if qualities
                    .application_star
                    .is_none_or(|(existing_q, _)| q > existing_q) =>
            {
                qualities.application_star = Some((q, index));
            }
            "*/*"
                if qualities
                    .wildcard
                    .is_none_or(|(existing_q, _)| q > existing_q) =>
            {
                qualities.wildcard = Some((q, index));
            }
            _ => {}
        }
    }

    qualities
}

/// Merge two `(q-value, list-index)` slots into the single best one — higher q
/// wins, ties broken by the earlier list index — reproducing the behaviour of
/// the original single-slot `accept_qualities`, where `application/json` and
/// `application/problem+json` shared one slot updated by strictly-greater q.
///
/// Used by the legacy [`accept_preference`] path to fold the (now separate)
/// `problem_json` slot back into the JSON signal. The `Negotiate` success
/// responder does **not** call this: it negotiates on the `json` slot alone.
fn combine_slots(a: Option<(f32, usize)>, b: Option<(f32, usize)>) -> Option<(f32, usize)> {
    match (a, b) {
        (Some((aq, ai)), Some((bq, bi))) => Some(if (aq - bq).abs() < f32::EPSILON {
            // Equal q: the earlier list entry is the one the single-slot scan
            // would have kept (it only replaced on strictly-greater q).
            if ai <= bi { (aq, ai) } else { (bq, bi) }
        } else if aq > bq {
            (aq, ai)
        } else {
            (bq, bi)
        }),
        (a @ Some(_), None) => a,
        (None, b) => b,
    }
}

/// The one canonical q-value `Accept` negotiator (legacy enum resolution).
///
/// Returns the client's concrete representation preference. Empty or missing
/// `Accept` yields [`AcceptPreference::Unspecified`]; a `*/*`-only header
/// yields [`AcceptPreference::Any`]. When both `text/html` and
/// `application/json` are present, the higher q-value wins, ties broken by
/// earlier list position. Parsing is delegated to the shared
/// [`accept_qualities`]; this function only applies the enum resolution policy.
pub fn accept_preference(headers: &axum::http::HeaderMap) -> AcceptPreference {
    // Distinguish a missing/empty `Accept` (no concrete preference at all) from
    // one that is present but names no html/json/wildcard type. The shared
    // parser collapses both to all-`None`, so the emptiness check stays here.
    let accept = headers
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if accept.is_empty() {
        return AcceptPreference::Unspecified;
    }

    // The legacy enum negotiator predates type wildcards and intentionally does
    // not consult the `text/*` / `application/*` slots — only the concrete types
    // and the `*/*` wildcard — so `accept_prefers_html` stays byte-identical.
    let AcceptQualities {
        html,
        json,
        problem_json,
        wildcard,
        ..
    } = accept_qualities(headers);

    // Legacy resolution treats `application/problem+json` as a JSON signal (a
    // problem+json client wants a JSON error body, not an HTML page). The shared
    // parser now records it in its own slot, so recombine the two into one
    // effective JSON signal here — higher q wins, ties broken by earlier list
    // index — reproducing the original single-slot `(q, index)` semantics
    // exactly so this path stays byte-identical.
    let json = combine_slots(json, problem_json);

    // Legacy resolution only cares about *positive* preference: a `q=0` slot
    // (now retained by the shared parser) is treated as absent here, exactly
    // reproducing the pre-`q=0`-aware behaviour of this enum negotiator.
    let positive = |slot: Option<(f32, usize)>| slot.filter(|&(q, _)| q > 0.0);
    let (html, json, wildcard) = (positive(html), positive(json), positive(wildcard));

    match (html, json, wildcard) {
        (Some((hq, hidx)), Some((jq, jidx)), _) => {
            let html_wins = if (hq - jq).abs() < f32::EPSILON {
                hidx < jidx
            } else {
                hq > jq
            };
            if html_wins {
                AcceptPreference::Html
            } else {
                AcceptPreference::Json
            }
        }
        (Some(_), None, _) => AcceptPreference::Html,
        (None, None, Some(_)) => AcceptPreference::Any,
        (None, Some(_), _) | (None, None, None) => AcceptPreference::Json,
    }
}

/// Derive path parameters by matching URI segments against a route pattern.
///
/// Handles both `{name}` (Axum/Autumn default) and `:name` (legacy) capture
/// syntax. Returns an empty object when segment counts differ.
fn extract_path_params(pattern: &str, uri_path: &str) -> serde_json::Value {
    let pat_segs: Vec<&str> = pattern.split('/').collect();
    let uri_segs: Vec<&str> = uri_path.split('/').collect();
    let mut map = serde_json::Map::new();
    if pat_segs.len() == uri_segs.len() {
        for (pat, val) in pat_segs.iter().zip(uri_segs.iter()) {
            let name = pat
                .strip_prefix(':')
                .or_else(|| pat.strip_prefix('{').and_then(|s| s.strip_suffix('}')));
            if let Some(name) = name {
                map.insert(
                    name.to_owned(),
                    serde_json::Value::String((*val).to_owned()),
                );
            }
        }
    }
    serde_json::Value::Object(map)
}

/// Convert an inspector [`QueryRecord`] to the overlay's [`SqlQueryInfo`].
#[cfg(feature = "maud")]
fn query_record_to_sql_info(
    r: &crate::inspector::QueryRecord,
) -> crate::error_pages::dev_badge::SqlQueryInfo {
    crate::error_pages::dev_badge::SqlQueryInfo {
        statement: r.sql.clone(),
        bind_count: r.params.len(),
        #[allow(clippy::cast_precision_loss)]
        duration_ms: r.elapsed_ms as f64,
    }
}

/// Parse `Cookie: name=value; name2=value2` into a JSON object.
///
/// Returns an object where each cookie name maps to its (possibly filtered) value.
fn parse_cookie_header(raw: &str) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for pair in raw.split(';') {
        let pair = pair.trim();
        if let Some((name, value)) = pair.split_once('=') {
            map.insert(
                name.trim().to_owned(),
                serde_json::Value::String(value.trim().to_owned()),
            );
        } else if !pair.is_empty() {
            map.insert(pair.to_owned(), serde_json::Value::String(String::new()));
        }
    }
    serde_json::Value::Object(map)
}

/// Parse request body bytes into a JSON value for the dev overlay.
///
/// Understands `application/json` and `application/x-www-form-urlencoded`.
/// Returns `None` for empty bodies or unrecognised content types.
fn parse_body_preview(
    bytes: &bytes::Bytes,
    content_type: Option<&str>,
) -> Option<serde_json::Value> {
    if bytes.is_empty() {
        return None;
    }
    let ct = content_type.unwrap_or("");
    if ct.contains("application/json") {
        serde_json::from_slice(bytes).ok()
    } else if ct.contains("application/x-www-form-urlencoded") {
        // Deserialize as ordered pairs so bracket-notation keys like
        // `user[password]` can be expanded into nested JSON before scrubbing.
        let pairs: Vec<(String, String)> = serde_urlencoded::from_bytes(bytes).ok()?;
        if pairs.is_empty() {
            None
        } else {
            Some(form_pairs_to_nested_json(pairs))
        }
    } else {
        None
    }
}

/// Convert URL-encoded form pairs into a nested JSON object, expanding
/// bracket notation so that `user[password]=secret` becomes
/// `{"user": {"password": "secret"}}`.  This lets `ParameterFilter::scrub_json`
/// match sensitive leaf keys regardless of their parent prefix.
fn form_pairs_to_nested_json(pairs: Vec<(String, String)>) -> serde_json::Value {
    let mut root = serde_json::Map::new();
    for (key, value) in pairs {
        let path = bracket_key_segments(&key);
        form_insert_at_path(&mut root, &path, serde_json::Value::String(value));
    }
    serde_json::Value::Object(root)
}

/// Split a form key on bracket or dot notation into path segments.
///
/// `"user[password]"` → `["user", "password"]`
/// `"user.password"` → `["user", "password"]`
/// `"a[b][c]"` → `["a", "b", "c"]`
/// `"simple"` → `["simple"]`
#[allow(
    clippy::indexing_slicing,
    reason = "pos is a valid byte index returned by str::find('['); both slices split on that boundary"
)]
fn bracket_key_segments(key: &str) -> Vec<String> {
    // Dot notation: split on '.' first (only if no brackets present)
    if key.contains('[') {
        let Some(pos) = key.find('[') else {
            return vec![key.to_owned()];
        };
        let mut parts = vec![key[..pos].to_owned()];
        for seg in key[pos + 1..].split('[') {
            let seg = seg.trim_end_matches(']');
            if !seg.is_empty() {
                parts.push(seg.to_owned());
            }
        }
        parts
    } else if key.contains('.') {
        key.split('.')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect()
    } else {
        vec![key.to_owned()]
    }
}

#[allow(
    clippy::indexing_slicing,
    reason = "path is guaranteed non-empty here and len >= 2 past the length guards below"
)]
fn form_insert_at_path(
    map: &mut serde_json::Map<String, serde_json::Value>,
    path: &[String],
    value: serde_json::Value,
) {
    if path.is_empty() {
        return;
    }
    if path.len() == 1 {
        map.insert(path[0].clone(), value);
        return;
    }
    let entry = map
        .entry(path[0].clone())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if let serde_json::Value::Object(child) = entry {
        form_insert_at_path(child, &path[1..], value);
    } else {
        // Key collision between a scalar value and a nested key: replace with object.
        let mut new_map = serde_json::Map::new();
        form_insert_at_path(&mut new_map, &path[1..], value);
        *entry = serde_json::Value::Object(new_map);
    }
}

fn scrub_headers(
    headers: &axum::http::HeaderMap,
    parameter_filter: &crate::log::filter::ParameterFilter,
) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for (name, value) in headers {
        let key = name.as_str().to_owned();
        let val = value.to_str().unwrap_or("<non-utf8>").to_owned();
        map.insert(key, serde_json::Value::String(val));
    }
    parameter_filter.scrub_json(&serde_json::Value::Object(map))
}

/// 404 fallback handler for unmatched routes.
///
/// This is mounted as the router's fallback so unmatched routes get proper
/// error pages instead of Axum's default plain-text "Not Found".
pub async fn fallback_404_handler(method: axum::http::Method, uri: axum::http::Uri) -> Response {
    if matches!(method, axum::http::Method::GET | axum::http::Method::HEAD)
        && uri.path() == crate::router::DEFAULT_FAVICON_PATH
    {
        return axum::http::StatusCode::NO_CONTENT.into_response();
    }

    crate::error::AutumnError::not_found_msg(format!("No route matches {}", uri.path()))
        .into_response()
}

#[cfg(feature = "maud")]
impl ErrorPageFilter {
    fn build_error_context(
        error: &AutumnErrorInfo,
        response: &Response,
        is_dev: bool,
    ) -> ErrorContext {
        let request_id = response
            .extensions()
            .get::<ErrorPageRequestContext>()
            .and_then(|ctx| ctx.request_id.clone());

        let path = response
            .extensions()
            .get::<ErrorPageRequestContext>()
            .map(|ctx| ctx.uri.path().to_string())
            .unwrap_or_default();

        ErrorContext {
            status: error.status,
            message: error.message.clone(),
            path,
            request_id,
            details: error.details.clone(),
            is_dev,
        }
    }

    fn inject_dev_badge(
        &self,
        html_body: &mut String,
        error: &AutumnErrorInfo,
        ctx: &ErrorContext,
        response: &Response,
    ) {
        let req_ctx = response.extensions().get::<ErrorPageRequestContext>();

        let stack_frames = error
            .backtrace_string
            .as_deref()
            .map(|bt| crate::error_pages::source::parse_backtrace_string(bt, 24))
            .unwrap_or_default();

        let raw_cookies = req_ctx
            .and_then(|c| c.cookies.as_ref())
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        let cookies = self.parameter_filter.scrub_json(&raw_cookies);

        let body_preview = req_ctx.and_then(|c| {
            if c.body_truncated {
                return Some(serde_json::Value::String(
                    "[body truncated: exceeds 64 KB limit]".to_owned(),
                ));
            }
            let bytes = c.body_bytes.as_ref()?;
            let parsed = parse_body_preview(bytes, c.content_type.as_deref())?;
            Some(self.parameter_filter.scrub_json(&parsed))
        });

        let badge_ctx = DevBadgeContext {
            status_code: error.status.as_u16(),
            status_reason: error
                .status
                .canonical_reason()
                .unwrap_or("Error")
                .to_string(),
            message: error.message.clone(),
            path: ctx.path.clone(),
            request_id: ctx.request_id.clone(),
            source_location: None,
            query: req_ctx.and_then(|c| c.query.clone()),
            headers: req_ctx.and_then(|c| c.headers.as_ref()).map_or_else(
                || serde_json::json!({}),
                |h| scrub_headers(h, &self.parameter_filter),
            ),
            method: req_ctx.and_then(|c| c.method.clone()),
            route_pattern: req_ctx.and_then(|c| c.matched_path.clone()),
            path_params: req_ctx.and_then(|c| c.path_params.as_ref()).map_or_else(
                || serde_json::json!({}),
                |p| self.parameter_filter.scrub_json(p),
            ),
            cookies,
            stack_frames,
            sql_queries: req_ctx
                .map(|c| c.sql_queries.iter().map(query_record_to_sql_info).collect())
                .unwrap_or_default(),
            body_preview,
        };
        let badge = dev_badge::dev_error_badge_html(&badge_ctx).into_string();
        if let Some(pos) = html_body.rfind("</body>") {
            html_body.insert_str(pos, &badge);
        } else {
            html_body.push_str(&badge);
        }
    }

    fn build_html_response(error: &AutumnErrorInfo, html_body: String) -> Response {
        let content_length = html_body.len();

        let mut resp = (
            error.status,
            [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
            html_body,
        )
            .into_response();

        // Re-attach error info so downstream filters still see it
        resp.extensions_mut().insert(error.clone());

        // Ensure content-length is set correctly, as middleware might otherwise
        // drop it in some environments like fallback routes.
        resp.headers_mut().insert(
            axum::http::header::CONTENT_LENGTH,
            axum::http::HeaderValue::from(content_length),
        );

        resp
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;

    #[test]
    fn accepts_html_for_browser() {
        let req = Request::builder()
            .header("accept", "text/html,application/xhtml+xml,*/*;q=0.8")
            .body(Body::empty())
            .unwrap();
        assert!(accepts_html(&req));
    }

    #[test]
    fn rejects_html_for_json_api() {
        let req = Request::builder()
            .header("accept", "application/json")
            .body(Body::empty())
            .unwrap();
        assert!(!accepts_html(&req));
    }

    #[test]
    fn rejects_html_for_empty_accept() {
        let req = Request::builder().body(Body::empty()).unwrap();
        assert!(!accepts_html(&req));
    }

    #[test]
    fn accepts_html_for_wildcard() {
        let req = Request::builder()
            .header("accept", "*/*")
            .body(Body::empty())
            .unwrap();
        assert!(accepts_html(&req));
    }

    #[test]
    fn prefers_json_when_json_first() {
        let req = Request::builder()
            .header("accept", "application/json, text/html")
            .body(Body::empty())
            .unwrap();
        assert!(!accepts_html(&req));
    }

    #[test]
    fn prefers_html_when_html_first() {
        let req = Request::builder()
            .header("accept", "text/html, application/json")
            .body(Body::empty())
            .unwrap();
        assert!(accepts_html(&req));
    }

    #[test]
    fn prefers_json_when_json_has_higher_q() {
        let req = Request::builder()
            .header("accept", "text/html;q=0.4, application/json;q=0.9")
            .body(Body::empty())
            .unwrap();
        assert!(!accepts_html(&req));
    }

    #[test]
    fn prefers_problem_json_when_problem_json_has_higher_q() {
        let req = Request::builder()
            .header("accept", "application/problem+json, text/html;q=0.1")
            .body(Body::empty())
            .unwrap();
        assert!(!accepts_html(&req));
    }

    #[test]
    fn prefers_html_when_html_has_higher_q() {
        let req = Request::builder()
            .header("accept", "application/json;q=0.3, text/html;q=0.8")
            .body(Body::empty())
            .unwrap();
        assert!(accepts_html(&req));
    }

    #[test]
    fn prefers_html_for_ac_tie_case() {
        // Acceptance-criteria case: html has the higher q-value and wins.
        let headers = header_map("application/json;q=0.9, text/html;q=1.0");
        assert!(accept_prefers_html(&headers));
        assert_eq!(accept_preference(&headers), AcceptPreference::Html);
    }

    /// Build a `HeaderMap` with a single `Accept` header for direct
    /// [`accept_preference`] / [`accept_prefers_html`] assertions.
    fn header_map(accept: &str) -> axum::http::HeaderMap {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::ACCEPT,
            axum::http::HeaderValue::from_str(accept).unwrap(),
        );
        headers
    }

    #[test]
    fn accept_preference_maps_cases_and_wrapper_is_unchanged() {
        // Empty / missing header → Unspecified, wrapper → false.
        let empty = axum::http::HeaderMap::new();
        assert_eq!(accept_preference(&empty), AcceptPreference::Unspecified);
        assert!(!accept_prefers_html(&empty));

        // `*/*` only → Any, wrapper → true.
        let any = header_map("*/*");
        assert_eq!(accept_preference(&any), AcceptPreference::Any);
        assert!(accept_prefers_html(&any));

        // `text/html` → Html, wrapper → true.
        let html = header_map("text/html");
        assert_eq!(accept_preference(&html), AcceptPreference::Html);
        assert!(accept_prefers_html(&html));

        // `application/json` → Json, wrapper → false.
        let json = header_map("application/json");
        assert_eq!(accept_preference(&json), AcceptPreference::Json);
        assert!(!accept_prefers_html(&json));
    }

    // ── Integration tests with the full middleware pipeline ──────

    use std::sync::Arc;

    use axum::Router;
    use axum::routing::get;
    use http::StatusCode;
    use tower::ServiceExt;

    use crate::error::AutumnError;
    #[cfg(feature = "maud")]
    use crate::error_pages;
    use crate::middleware::exception_filter::ExceptionFilterLayer;

    /// Helper: build a router with the error page filter and context layer.
    fn test_router_with_error_pages(is_dev: bool) -> Router {
        let renderer = error_pages::default_renderer();
        let error_page_filter = ErrorPageFilter {
            renderer,
            is_dev,
            parameter_filter: crate::log::filter::ParameterFilter::default(),
        };
        let filters: Vec<Arc<dyn crate::middleware::ExceptionFilter>> =
            vec![Arc::new(error_page_filter)];

        Router::new()
            .route("/ok", get(|| async { "all good" }))
            .route(
                "/fail",
                get(|| async { Err::<String, AutumnError>(AutumnError::not_found_msg("gone")) }),
            )
            .route(
                "/boom",
                get(|| async {
                    Err::<String, AutumnError>(AutumnError::from(std::io::Error::other(
                        "internal failure",
                    )))
                }),
            )
            .fallback(fallback_404_handler)
            .layer(ErrorPageContextLayer { is_dev })
            .layer(ExceptionFilterLayer::new(filters))
    }

    #[tokio::test]
    async fn html_404_returns_styled_page() {
        let app = test_router_with_error_pages(false);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/nonexistent")
                    .header("accept", "text/html")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let content_length = resp
            .headers()
            .get("content-length")
            .expect("Content-Length header should be set")
            .to_str()
            .unwrap()
            .parse::<usize>()
            .unwrap();

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();

        assert_eq!(content_length, body.len());
        let body_str = String::from_utf8(body.to_vec()).unwrap();
        assert!(body_str.contains("<!DOCTYPE html>"), "should be HTML");
        assert!(body_str.contains("404"), "should contain status code");
        assert!(
            body_str.contains("Page not found"),
            "should contain not found message"
        );
    }

    #[tokio::test]
    async fn html_error_page_preserves_mirrored_cors_and_request_id_headers() {
        // A timeout 503 reaches the error-page filter already carrying the
        // mirrored CORS headers and the request id, marked as wanting HTML.
        // The HTML rebuild must not drop them, or a cross-origin browser/HTMX
        // client sees an opaque CORS failure instead of the styled page.
        let renderer = error_pages::default_renderer();
        let filter = ErrorPageFilter {
            renderer,
            is_dev: false,
            parameter_filter: crate::log::filter::ParameterFilter::default(),
        };

        let error = AutumnErrorInfo {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: "request deadline exceeded".into(),
            details: None,
            problem_type: None,
            backtrace_string: None,
        };

        let mut original = (StatusCode::SERVICE_UNAVAILABLE, "json body").into_response();
        original.headers_mut().insert(
            "access-control-allow-origin",
            http::HeaderValue::from_static("https://app.example.com"),
        );
        original.headers_mut().insert(
            "access-control-allow-credentials",
            http::HeaderValue::from_static("true"),
        );
        original
            .headers_mut()
            .insert("x-request-id", http::HeaderValue::from_static("req-123"));
        original.extensions_mut().insert(WantsHtml(true));

        let response = filter.filter(&error, original);

        assert_eq!(
            response.headers()["access-control-allow-origin"],
            "https://app.example.com",
            "mirrored CORS origin must survive the HTML rebuild"
        );
        assert_eq!(
            response.headers()["access-control-allow-credentials"],
            "true",
            "mirrored CORS credentials flag must survive the HTML rebuild"
        );
        assert_eq!(
            response.headers()["x-request-id"],
            "req-123",
            "request id must survive the HTML rebuild"
        );
        assert_eq!(
            response.headers()[axum::http::header::CONTENT_TYPE],
            "text/html; charset=utf-8"
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_str = String::from_utf8(body.to_vec()).unwrap();
        assert!(body_str.contains("<!DOCTYPE html>"), "should be HTML");
    }

    #[tokio::test]
    async fn html_rebuild_drops_stale_content_encoding() {
        // An inner response-transforming layer (e.g. a user `CompressionLayer`)
        // may set `Content-Encoding: gzip` before the exception filter runs. The
        // rebuilt HTML body is uncompressed, so the stale encoding header must be
        // dropped or the browser fails to decode the page.
        let renderer = error_pages::default_renderer();
        let filter = ErrorPageFilter {
            renderer,
            is_dev: false,
            parameter_filter: crate::log::filter::ParameterFilter::default(),
        };

        let error = AutumnErrorInfo {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "boom".into(),
            details: None,
            problem_type: None,
            backtrace_string: None,
        };

        let mut original = (StatusCode::INTERNAL_SERVER_ERROR, "compressed json").into_response();
        original.headers_mut().insert(
            http::header::CONTENT_ENCODING,
            http::HeaderValue::from_static("gzip"),
        );
        original
            .headers_mut()
            .insert("x-request-id", http::HeaderValue::from_static("req-9"));
        original.extensions_mut().insert(WantsHtml(true));

        let response = filter.filter(&error, original);

        assert!(
            !response
                .headers()
                .contains_key(http::header::CONTENT_ENCODING),
            "stale Content-Encoding must not ride along on the rebuilt HTML body"
        );
        assert_eq!(
            response.headers()["x-request-id"],
            "req-9",
            "unrelated headers must still be preserved"
        );
        assert_eq!(
            response.headers()[axum::http::header::CONTENT_TYPE],
            "text/html; charset=utf-8"
        );
    }

    #[tokio::test]
    async fn html_500_returns_styled_page() {
        let app = test_router_with_error_pages(false);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/boom")
                    .header("accept", "text/html")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_str = String::from_utf8(body.to_vec()).unwrap();
        assert!(body_str.contains("<!DOCTYPE html>"), "should be HTML");
        assert!(body_str.contains("500"), "should contain status code");
        assert!(
            !body_str.contains("internal failure"),
            "must NOT show error details in prod"
        );
    }

    #[tokio::test]
    async fn json_api_gets_json_errors() {
        let app = test_router_with_error_pages(false);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/fail")
                    .header("accept", "application/json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            ct.contains("application/problem+json"),
            "JSON API should get Problem Details response, got: {ct}"
        );
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], 404);
        assert_eq!(json["code"], "autumn.not_found");
    }

    #[tokio::test]
    async fn dev_badge_appears_in_dev_mode() {
        let app = test_router_with_error_pages(true);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/nonexistent")
                    .header("accept", "text/html")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_str = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            body_str.contains("autumn-dev-error-badge"),
            "dev mode should include error badge"
        );
    }

    #[tokio::test]
    async fn dev_badge_hidden_in_prod_mode() {
        let app = test_router_with_error_pages(false);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/nonexistent")
                    .header("accept", "text/html")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_str = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            !body_str.contains("autumn-dev-error-badge"),
            "prod mode must NOT include error badge"
        );
    }

    #[tokio::test]
    async fn custom_renderer_works() {
        use crate::error_pages::{ErrorContext, ErrorPageRenderer};
        use maud::{Markup, html};

        struct CustomPages;
        impl ErrorPageRenderer for CustomPages {
            fn render_error(&self, ctx: &ErrorContext) -> Markup {
                html! {
                    h1 { "CUSTOM " (ctx.status.as_u16()) }
                }
            }
        }

        let renderer: crate::error_pages::SharedRenderer = Arc::new(CustomPages);
        let error_page_filter = ErrorPageFilter {
            renderer,
            is_dev: false,
            parameter_filter: crate::log::filter::ParameterFilter::default(),
        };
        let filters: Vec<Arc<dyn crate::middleware::ExceptionFilter>> =
            vec![Arc::new(error_page_filter)];

        let app = Router::new()
            .route(
                "/err",
                get(|| async { Err::<String, AutumnError>(AutumnError::not_found_msg("nope")) }),
            )
            .layer(ErrorPageContextLayer { is_dev: false })
            .layer(ExceptionFilterLayer::new(filters));

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/err")
                    .header("accept", "text/html")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_str = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            body_str.contains("CUSTOM 404"),
            "should use custom renderer, got: {body_str}"
        );
    }

    #[tokio::test]
    async fn custom_renderer_is_ignored_for_json_requests() {
        use crate::error_pages::{ErrorContext, ErrorPageRenderer};
        use maud::{Markup, html};

        struct LoudCustomPages;
        impl ErrorPageRenderer for LoudCustomPages {
            fn render_error(&self, ctx: &ErrorContext) -> Markup {
                html! {
                    h1 { "LOUD CUSTOM " (ctx.status.as_u16()) }
                }
            }
        }

        let renderer: crate::error_pages::SharedRenderer = Arc::new(LoudCustomPages);
        let error_page_filter = ErrorPageFilter {
            renderer,
            is_dev: false,
            parameter_filter: crate::log::filter::ParameterFilter::default(),
        };
        let filters: Vec<Arc<dyn crate::middleware::ExceptionFilter>> =
            vec![Arc::new(error_page_filter)];

        let app = Router::new()
            .route(
                "/err",
                get(|| async { Err::<String, AutumnError>(AutumnError::not_found_msg("nope")) }),
            )
            .layer(ErrorPageContextLayer { is_dev: false })
            .layer(ExceptionFilterLayer::new(filters));

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/err")
                    .header("accept", "application/json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let ct = resp
            .headers()
            .get("content-type")
            .expect("content-type should be present")
            .to_str()
            .expect("content-type should be valid UTF-8");
        assert!(
            ct.contains("application/problem+json"),
            "JSON requests should still get Problem Details, got: {ct}"
        );

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_str = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            !body_str.contains("LOUD CUSTOM 404"),
            "custom HTML renderer should not run for JSON requests, got: {body_str}"
        );
    }

    #[tokio::test]
    async fn success_responses_not_affected() {
        let app = test_router_with_error_pages(false);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/ok")
                    .header("accept", "text/html")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], b"all good");
    }

    #[tokio::test]
    async fn fallback_404_handler_creates_correct_error() {
        let uri = axum::http::Uri::from_static("/some/unknown/path");
        let response = fallback_404_handler(axum::http::Method::GET, uri).await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&body).contains("No route matches /some/unknown/path"));
    }

    #[tokio::test]
    async fn fallback_404_handler_ignores_query_params() {
        let uri = axum::http::Uri::from_static("/search?q=rust&sort=desc");
        let response = fallback_404_handler(axum::http::Method::GET, uri).await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&body).contains("No route matches /search"));
    }

    #[tokio::test]
    async fn fallback_404_handler_with_root_path() {
        let uri = axum::http::Uri::from_static("/");
        let response = fallback_404_handler(axum::http::Method::GET, uri).await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&body).contains("No route matches /"));
    }

    #[tokio::test]
    async fn fallback_404_handler_returns_empty_no_content_for_favicon_get() {
        let response = fallback_404_handler(
            axum::http::Method::GET,
            axum::http::Uri::from_static(crate::router::DEFAULT_FAVICON_PATH),
        )
        .await;

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(body.is_empty());
    }

    #[tokio::test]
    async fn fallback_404_handler_returns_empty_no_content_for_favicon_head() {
        let response = fallback_404_handler(
            axum::http::Method::HEAD,
            axum::http::Uri::from_static(crate::router::DEFAULT_FAVICON_PATH),
        )
        .await;

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(body.is_empty());
    }

    #[tokio::test]
    async fn dev_badge_scrubs_sensitive_headers_on_form_post() {
        let app = test_router_with_error_pages(true);

        let response = tower::ServiceExt::oneshot(
            app,
            Request::builder()
                .method("POST")
                .uri("/nope?debug=true")
                .header("accept", "text/html")
                .header("content-type", "application/x-www-form-urlencoded")
                .header("authorization", "Bearer super-secret-token")
                .body(Body::from("password=hunter2&email=user@example.com"))
                .expect("request"),
        )
        .await
        .expect("response");

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("bytes");
        let body_str = String::from_utf8(body.to_vec()).expect("utf8");

        assert!(body_str.contains("authorization"));
        assert!(body_str.contains("[FILTERED]"));
        assert!(body_str.contains("Query"));
    }

    #[tokio::test]
    async fn dev_badge_uses_configured_custom_filter_parameters() {
        let renderer = error_pages::default_renderer();
        let error_page_filter = ErrorPageFilter {
            renderer,
            is_dev: true,
            parameter_filter: crate::log::filter::ParameterFilter::new(&["pin".to_owned()], &[]),
        };
        let filters: Vec<Arc<dyn crate::middleware::ExceptionFilter>> =
            vec![Arc::new(error_page_filter)];

        let app = Router::new()
            .fallback(fallback_404_handler)
            .layer(ErrorPageContextLayer { is_dev: true })
            .layer(ExceptionFilterLayer::new(filters));

        let response = tower::ServiceExt::oneshot(
            app,
            Request::builder()
                .method("POST")
                .uri("/nope")
                .header("accept", "text/html")
                .header("pin", "1234")
                .body(Body::from("x=1"))
                .expect("request"),
        )
        .await
        .expect("response");

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("bytes");
        let body_str = String::from_utf8(body.to_vec()).expect("utf8");

        assert!(body_str.contains("pin"));
        assert!(body_str.contains("[FILTERED]"));
    }

    #[test]
    fn extract_path_params_brace_syntax() {
        let v = super::extract_path_params("/posts/{id}/comments/{cid}", "/posts/42/comments/7");
        assert_eq!(v["id"], "42");
        assert_eq!(v["cid"], "7");
    }

    #[test]
    fn extract_path_params_colon_syntax() {
        let v = super::extract_path_params("/posts/:id/comments/:cid", "/posts/42/comments/7");
        assert_eq!(v["id"], "42");
        assert_eq!(v["cid"], "7");
    }

    #[test]
    fn extract_path_params_static_route_returns_empty() {
        let v = super::extract_path_params("/posts", "/posts");
        assert_eq!(v.as_object().unwrap().len(), 0);
    }

    #[test]
    fn extract_path_params_segment_count_mismatch_returns_empty() {
        let v = super::extract_path_params("/posts/{id}", "/posts/42/extra");
        assert_eq!(
            v.as_object().unwrap().len(),
            0,
            "mismatched segments should yield empty map"
        );
    }

    #[test]
    fn extract_path_params_root_wildcard_returns_empty_for_multisegment_uri() {
        // A wildcard capture like `{*rest}` spans multiple URI segments but the
        // naive segment-split produces a length mismatch; we should return empty
        // rather than crashing or silently dropping segments.
        let v = super::extract_path_params("/files/{*rest}", "/files/foo/bar/baz");
        assert_eq!(
            v.as_object().unwrap().len(),
            0,
            "wildcard glob should yield empty map"
        );
    }

    // ── AC2: path param values scrubbed via ParameterFilter ──────────────────

    #[test]
    fn extract_path_params_scrubbed_by_parameter_filter() {
        // A route like /reset/{token} would expose a sensitive value; the
        // ParameterFilter should mask it before it appears in the overlay.
        let raw = super::extract_path_params("/reset/{token}", "/reset/abc123secret");
        let filter = crate::log::filter::ParameterFilter::default();
        let scrubbed = filter.scrub_json(&raw);
        assert_eq!(
            scrubbed["token"], "[FILTERED]",
            "token param value should be scrubbed by the default filter"
        );
    }

    // ── AC3: Path Params section hidden when route has no path params ─────────

    #[tokio::test]
    async fn dev_badge_hides_path_params_section_for_static_route() {
        // A route with no `{param}` segments must NOT render the Path Params
        // section in the overlay.
        let app = test_router_with_error_pages_and_matched_path(true);
        let resp = tower::ServiceExt::oneshot(
            app,
            Request::builder()
                .uri("/fail")
                .header("accept", "text/html")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_str = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            !body_str.contains("Path Params"),
            "static route overlay must not include Path Params section, got:\n{body_str}"
        );
    }

    // ── AC1: overlay shows parsed path params ────────────────────────────────

    #[tokio::test]
    async fn dev_badge_shows_path_params_in_overlay() {
        // A route with a `{id}` param must render the parsed value in the overlay.
        let app = test_router_with_error_pages_and_matched_path(true);
        let resp = tower::ServiceExt::oneshot(
            app,
            Request::builder()
                .uri("/items/99")
                .header("accept", "text/html")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_str = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            body_str.contains("Path Params"),
            "overlay must include Path Params section for parametric route"
        );
        assert!(
            body_str.contains("99"),
            "overlay must show extracted path param value"
        );
    }

    // ── AC2 integration: sensitive path param scrubbed in overlay ────────────

    #[tokio::test]
    async fn dev_badge_scrubs_sensitive_path_param_in_overlay() {
        // Route is /tokens/{token}; "token" is a DEFAULT_FILTER_KEYS entry.
        // The Path Params table must show [FILTERED], not the raw value.
        // Note: the raw URI path still appears in the "Path" row — only the
        // *parsed* params map is scrubbed by ParameterFilter.
        let app = test_router_with_error_pages_and_matched_path(true);
        let resp = tower::ServiceExt::oneshot(
            app,
            Request::builder()
                .uri("/tokens/supersecret")
                .header("accept", "text/html")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_str = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            body_str.contains("Path Params"),
            "overlay must show Path Params section even when value is filtered"
        );
        assert!(
            body_str.contains("[FILTERED]"),
            "sensitive path param value must be scrubbed in the Path Params map"
        );
    }

    /// Build a test router that additionally applies `capture_matched_path_middleware`
    /// (simulating dev profile) and includes routes with path parameters.
    fn test_router_with_error_pages_and_matched_path(is_dev: bool) -> axum::Router {
        use crate::middleware::dev::capture_matched_path_middleware;
        use axum::routing::get;

        let renderer = error_pages::default_renderer();
        let error_page_filter = ErrorPageFilter {
            renderer,
            is_dev,
            parameter_filter: crate::log::filter::ParameterFilter::default(),
        };
        let filters: Vec<Arc<dyn crate::middleware::ExceptionFilter>> =
            vec![Arc::new(error_page_filter)];

        axum::Router::new()
            .route(
                "/fail",
                get(|| async { Err::<String, AutumnError>(AutumnError::not_found_msg("gone")) }),
            )
            .route(
                "/items/{id}",
                get(|| async {
                    Err::<String, AutumnError>(AutumnError::not_found_msg("item not found"))
                }),
            )
            .route(
                "/tokens/{token}",
                get(|| async {
                    Err::<String, AutumnError>(AutumnError::not_found_msg("token not found"))
                }),
            )
            .route_layer(axum::middleware::from_fn(capture_matched_path_middleware))
            .fallback(fallback_404_handler)
            .layer(ErrorPageContextLayer { is_dev })
            .layer(ExceptionFilterLayer::new(filters))
    }

    #[tokio::test]
    async fn fallback_404_handler_keeps_non_get_favicon_requests_as_not_found() {
        let response = fallback_404_handler(
            axum::http::Method::POST,
            axum::http::Uri::from_static(crate::router::DEFAULT_FAVICON_PATH),
        )
        .await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    // ── Body capture tests (issue #1081) ────────────────────────────────────

    #[tokio::test]
    async fn dev_badge_shows_form_body_in_dev_mode() {
        let app = test_router_with_error_pages(true);

        let response = tower::ServiceExt::oneshot(
            app,
            Request::builder()
                .method("POST")
                .uri("/nope")
                .header("accept", "text/html")
                .header("content-type", "application/x-www-form-urlencoded")
                .header("content-length", "20")
                .body(Body::from("username=alice&age=30"))
                .expect("request"),
        )
        .await
        .expect("response");

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("bytes");
        let body_str = String::from_utf8(body.to_vec()).expect("utf8");

        assert!(
            body_str.contains("Body"),
            "dev overlay should show a Body section for form POST, got:\n{body_str}"
        );
        assert!(
            body_str.contains("alice"),
            "dev overlay should show form field value, got:\n{body_str}"
        );
    }

    #[tokio::test]
    async fn dev_badge_shows_json_body_in_dev_mode() {
        let app = test_router_with_error_pages(true);

        let response = tower::ServiceExt::oneshot(
            app,
            Request::builder()
                .method("POST")
                .uri("/nope")
                .header("accept", "text/html")
                .header("content-type", "application/json")
                .header("content-length", "24")
                .body(Body::from(r#"{"user":"bob","score":42}"#))
                .expect("request"),
        )
        .await
        .expect("response");

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("bytes");
        let body_str = String::from_utf8(body.to_vec()).expect("utf8");

        assert!(
            body_str.contains("Body"),
            "dev overlay should show a Body section for JSON POST"
        );
        assert!(
            body_str.contains("bob"),
            "dev overlay should show JSON field value"
        );
    }

    #[tokio::test]
    async fn dev_badge_scrubs_sensitive_form_body_fields() {
        let app = test_router_with_error_pages(true);

        let response = tower::ServiceExt::oneshot(
            app,
            Request::builder()
                .method("POST")
                .uri("/nope")
                .header("accept", "text/html")
                .header("content-type", "application/x-www-form-urlencoded")
                .header("content-length", "33")
                .body(Body::from("username=alice&password=secret123"))
                .expect("request"),
        )
        .await
        .expect("response");

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("bytes");
        let body_str = String::from_utf8(body.to_vec()).expect("utf8");

        assert!(
            body_str.contains("password"),
            "should show the password key"
        );
        assert!(
            !body_str.contains("secret123"),
            "raw password value must be filtered out"
        );
        assert!(
            body_str.contains("[FILTERED]"),
            "filtered field should show [FILTERED]"
        );
    }

    #[tokio::test]
    async fn dev_badge_scrubs_sensitive_json_body_fields() {
        let app = test_router_with_error_pages(true);

        let response = tower::ServiceExt::oneshot(
            app,
            Request::builder()
                .method("POST")
                .uri("/nope")
                .header("accept", "text/html")
                .header("content-type", "application/json")
                .header("content-length", "33")
                .body(Body::from(r#"{"user":"alice","token":"abc123"}"#))
                .expect("request"),
        )
        .await
        .expect("response");

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("bytes");
        let body_str = String::from_utf8(body.to_vec()).expect("utf8");

        assert!(body_str.contains("token"), "should show the token key");
        assert!(
            !body_str.contains("abc123"),
            "raw token value must be filtered out"
        );
        assert!(body_str.contains("[FILTERED]"), "should show [FILTERED]");
    }

    #[tokio::test]
    async fn dev_badge_does_not_show_body_in_prod_mode() {
        let app = test_router_with_error_pages(false);

        let response = tower::ServiceExt::oneshot(
            app,
            Request::builder()
                .method("POST")
                .uri("/nope")
                .header("accept", "text/html")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from("username=alice&age=30"))
                .expect("request"),
        )
        .await
        .expect("response");

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("bytes");
        let body_str = String::from_utf8(body.to_vec()).expect("utf8");

        // In prod mode there is no dev badge at all, so no Body section
        assert!(
            !body_str.contains("autumn-dev-error-badge"),
            "prod mode must not include the dev badge"
        );
    }

    #[tokio::test]
    async fn dev_badge_does_not_show_body_section_for_get() {
        let app = test_router_with_error_pages(true);

        let response = tower::ServiceExt::oneshot(
            app,
            Request::builder()
                .method("GET")
                .uri("/nope")
                .header("accept", "text/html")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("bytes");
        let body_str = String::from_utf8(body.to_vec()).expect("utf8");

        // The overlay should exist but not have a Body section for GET
        assert!(
            body_str.contains("autumn-dev-error-badge"),
            "dev badge should appear on GET errors"
        );
        // No "Body" label in the overlay for a GET request
        assert!(
            !body_str.contains(">Body<"),
            "GET requests should not show a Body section"
        );
    }

    #[tokio::test]
    async fn dev_badge_skips_body_capture_when_no_content_length() {
        // Without a Content-Length header the body stream is left unconsumed
        // so that downstream handlers receive it intact. The overlay simply
        // shows no body section rather than a potentially truncated preview.
        let app = test_router_with_error_pages(true);

        // Build a body larger than the 64 KB cap but send no Content-Length.
        let large_body = "x=".to_owned() + &"a".repeat(70 * 1024);

        let response = tower::ServiceExt::oneshot(
            app,
            Request::builder()
                .method("POST")
                .uri("/nope")
                .header("accept", "text/html")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(large_body))
                .expect("request"),
        )
        .await
        .expect("response");

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("bytes");
        let body_str = String::from_utf8(body.to_vec()).expect("utf8");

        // No body section and no truncation notice — stream was not consumed.
        assert!(
            !body_str.contains("truncated"),
            "body without Content-Length should not show a truncation notice, got:\n{body_str}"
        );
    }

    #[tokio::test]
    async fn dev_badge_truncates_when_content_length_header_exceeds_limit() {
        // When Content-Length is declared over the cap the body stream is left
        // unconsumed and the overlay shows a truncation notice immediately.
        let app = test_router_with_error_pages(true);

        let over_limit = (BODY_CAPTURE_LIMIT + 1).to_string();
        let response = tower::ServiceExt::oneshot(
            app,
            Request::builder()
                .method("POST")
                .uri("/nope")
                .header("accept", "text/html")
                .header("content-type", "application/x-www-form-urlencoded")
                .header("content-length", over_limit)
                // Actual body is small — only Content-Length matters for the early-exit path.
                .body(Body::from("x=1"))
                .expect("request"),
        )
        .await
        .expect("response");

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("bytes");
        let body_str = String::from_utf8(body.to_vec()).expect("utf8");

        assert!(
            body_str.contains("truncated"),
            "declared-oversized body should show truncation notice"
        );
    }

    #[tokio::test]
    async fn dev_badge_scrubs_bracket_notation_form_body_fields() {
        // user[password]=secret should be treated the same as password=secret.
        let app = test_router_with_error_pages(true);

        let response = tower::ServiceExt::oneshot(
            app,
            Request::builder()
                .method("POST")
                .uri("/nope")
                .header("accept", "text/html")
                .header("content-type", "application/x-www-form-urlencoded")
                .header("content-length", "41")
                .body(Body::from("user[name]=alice&user[password]=secret123"))
                .expect("request"),
        )
        .await
        .expect("response");

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("bytes");
        let body_str = String::from_utf8(body.to_vec()).expect("utf8");

        assert!(
            body_str.contains("password"),
            "should show the password key"
        );
        assert!(
            !body_str.contains("secret123"),
            "bracket-prefixed password value must be filtered"
        );
        assert!(body_str.contains("[FILTERED]"), "should show [FILTERED]");
        // The non-sensitive sibling field should still be visible.
        assert!(
            body_str.contains("alice"),
            "non-sensitive field should remain"
        );
    }

    #[tokio::test]
    async fn dev_badge_scrubs_dotted_notation_form_body_fields() {
        // user.password=secret uses dot notation; must be split and scrubbed.
        let app = test_router_with_error_pages(true);
        let body_str_literal = "user.name=alice&user.password=secret123";

        let response = tower::ServiceExt::oneshot(
            app,
            Request::builder()
                .method("POST")
                .uri("/nope")
                .header("accept", "text/html")
                .header("content-type", "application/x-www-form-urlencoded")
                .header("content-length", body_str_literal.len().to_string())
                .body(Body::from(body_str_literal))
                .expect("request"),
        )
        .await
        .expect("response");

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("bytes");
        let body_str = String::from_utf8(body.to_vec()).expect("utf8");

        assert!(
            body_str.contains("password"),
            "should show the password key"
        );
        assert!(
            !body_str.contains("secret123"),
            "dot-prefixed password value must be filtered"
        );
        assert!(body_str.contains("[FILTERED]"), "should show [FILTERED]");
        assert!(
            body_str.contains("alice"),
            "non-sensitive field should remain"
        );
    }

    #[test]
    fn bracket_key_segments_handles_simple_key() {
        assert_eq!(bracket_key_segments("simple"), vec!["simple"]);
    }

    #[test]
    fn bracket_key_segments_handles_one_level() {
        assert_eq!(
            bracket_key_segments("user[password]"),
            vec!["user", "password"]
        );
    }

    #[test]
    fn bracket_key_segments_handles_multi_level() {
        assert_eq!(bracket_key_segments("a[b][c]"), vec!["a", "b", "c"]);
    }
}
