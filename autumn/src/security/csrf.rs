//! CSRF (Cross-Site Request Forgery) protection middleware.
//!
//! Protects against CSRF attacks by requiring a token on mutating
//! HTTP methods (POST, PUT, DELETE, PATCH). The token is stored in a
//! cookie and must be echoed back via a request header or form field.
//!
//! # How it works
//!
//! 1. On every response, a CSRF cookie is set (if not already present)
//!    containing a random UUID v4 token.
//! 2. On mutating requests, the middleware checks that the token from
//!    the cookie matches the token in the `X-CSRF-Token` header (or
//!    `_csrf` form field).
//! 3. Safe methods (GET, HEAD, OPTIONS, TRACE) are exempt.
//!
//! # Configuration
//!
//! See [`CsrfConfig`] for available settings.
//!
//! # Examples
//!
//! ## Template integration (Maud)
//!
//! ```rust,ignore
//! use autumn_web::prelude::*;
//! use autumn_web::security::CsrfToken;
//!
//! #[get("/form")]
//! async fn form(csrf: CsrfToken) -> Markup {
//!     html! {
//!         form method="POST" action="/submit" {
//!             input type="hidden" name="_csrf" value=(csrf.token());
//!             input type="text" name="title";
//!             button { "Submit" }
//!         }
//!     }
//! }
//! ```
//!
//! ## JavaScript / htmx
//!
//! Read the CSRF token from the `autumn-csrf` cookie and send it
//! as an `X-CSRF-Token` header with every mutating request.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::extract::{FromRequestParts, OptionalFromRequestParts};
use axum::http::{Request, Response, StatusCode};
use http::header::HeaderName;

use tower::{Layer, Service};
use uuid::Uuid;

use super::config::CsrfConfig;

/// Error body returned with a `403 Forbidden` when CSRF validation fails.
const CSRF_FORBIDDEN_MESSAGE: &str = "CSRF token missing or invalid";

/// Error body returned with a `413 Payload Too Large` when the form body carrying
/// the `_csrf` token exceeds the configured scan cap (see [`CsrfVerdict`]).
const CSRF_PAYLOAD_TOO_LARGE_MESSAGE: &str = "Request body exceeds the maximum allowed size";

/// Outcome of CSRF verification for a mutating request.
enum CsrfVerdict {
    /// A matching token was found — allow the request through.
    Valid,
    /// No valid token was presented — reject with `403 Forbidden`.
    Missing,
    /// The urlencoded/multipart form body carrying the token exceeded
    /// `max_scan_bytes`, so `to_bytes` could not buffer it to locate the
    /// `_csrf` field. Reject with `413 Payload Too Large` rather than a
    /// misleading `403` "missing token": the body genuinely is over the
    /// request-size limit, and the intended `413` would otherwise be
    /// unobservable because this CSRF layer short-circuits before the upload
    /// extractor/body-limit can surface it (see issue #1866).
    BodyTooLarge,
}

/// Return `true` when `err` (from [`axum::body::to_bytes`]) was caused by the
/// scan-cap length limit being exceeded, as opposed to some other body read
/// error (e.g. a client disconnect mid-body).
///
/// `to_bytes` wraps the limit failure as an [`http_body_util::LengthLimitError`]
/// in the error's source chain (see the axum `to_bytes` docs). We walk the whole
/// chain and downcast so the detection stays robust even if axum adds wrapping
/// layers.
fn is_body_length_limit_error(err: &axum::Error) -> bool {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(err) = source {
        if err.is::<http_body_util::LengthLimitError>() {
            return true;
        }
        source = err.source();
    }
    false
}

/// The configured CSRF form field name, placed in request extensions by [`CsrfLayer`].
///
/// [`ChangesetForm`](crate::form::ChangesetForm) reads this so `form_tag` emits the
/// hidden input under the correct field name even when `security.csrf.form_field` has
/// been customised from its default `"_csrf"`.
#[derive(Clone, Debug)]
pub struct CsrfFormField(pub String);

/// The configured CSRF token header name, placed in request extensions by [`CsrfLayer`].
///
/// Templates can read this to emit the correct `data-header` attribute on the
/// `<meta name="csrf-token">` tag so JavaScript CSRF helpers (e.g. the admin panel
/// multipart submit handler) use the configured header name rather than defaulting
/// to `X-CSRF-Token`.
#[derive(Clone, Debug)]
pub struct CsrfTokenHeader(pub String);

/// A CSRF token extracted from the request.
///
/// Use this as a handler parameter to access the CSRF token for embedding
/// in HTML forms. The token is generated per-request and stored in
/// request extensions by the [`CsrfLayer`].
///
/// ## Examples
///
/// ```rust,ignore
/// use autumn_web::prelude::*;
/// use autumn_web::security::CsrfToken;
///
/// #[get("/edit")]
/// async fn edit_form(csrf: CsrfToken) -> Markup {
///     html! {
///         form method="POST" {
///             input type="hidden" name="_csrf" value=(csrf.token());
///             // ...
///         }
///     }
/// }
/// ```
#[derive(Clone, Debug)]
pub struct CsrfToken(String);

impl CsrfToken {
    /// Returns the CSRF token value for embedding in forms or headers.
    #[must_use]
    pub fn token(&self) -> &str {
        &self.0
    }

    #[cfg(test)]
    pub(crate) const fn new(token: String) -> Self {
        Self(token)
    }
}

impl std::fmt::Display for CsrfToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl<S> FromRequestParts<S> for CsrfToken
where
    S: Send + Sync,
{
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        parts.extensions.get::<Self>().cloned().ok_or((
            StatusCode::INTERNAL_SERVER_ERROR,
            "CSRF token not found in request extensions. Is CsrfLayer enabled?",
        ))
    }
}

impl<S> OptionalFromRequestParts<S> for CsrfToken
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Option<Self>, Self::Rejection> {
        Ok(parts.extensions.get::<Self>().cloned())
    }
}

impl<S> FromRequestParts<S> for CsrfFormField
where
    S: Send + Sync,
{
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        parts.extensions.get::<Self>().cloned().ok_or((
            StatusCode::INTERNAL_SERVER_ERROR,
            "CSRF form field not found in request extensions. Is CsrfLayer enabled?",
        ))
    }
}

impl<S> OptionalFromRequestParts<S> for CsrfFormField
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Option<Self>, Self::Rejection> {
        Ok(parts.extensions.get::<Self>().cloned())
    }
}

impl<S> FromRequestParts<S> for CsrfTokenHeader
where
    S: Send + Sync,
{
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        parts.extensions.get::<Self>().cloned().ok_or((
            StatusCode::INTERNAL_SERVER_ERROR,
            "CSRF token header not found in request extensions. Is CsrfLayer enabled?",
        ))
    }
}

impl<S> OptionalFromRequestParts<S> for CsrfTokenHeader
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Option<Self>, Self::Rejection> {
        Ok(parts.extensions.get::<Self>().cloned())
    }
}

/// Shared CSRF configuration.
#[derive(Debug, Clone)]
struct CsrfSettings {
    cookie_name: String,
    token_header: HeaderName,
    form_field: String,
    safe_methods: Vec<http::Method>,
    exempt_paths: Vec<String>,
    signing_keys: Option<Arc<crate::security::config::ResolvedSigningKeys>>,
    max_scan_bytes: usize,
}

/// Tower [`Layer`] that applies CSRF protection.
///
/// Applied automatically when `security.csrf.enabled = true` in config.
#[derive(Clone, Debug)]
pub struct CsrfLayer {
    settings: Arc<CsrfSettings>,
}

impl CsrfLayer {
    /// Create a new CSRF layer from configuration.
    #[must_use]
    pub fn from_config(config: &CsrfConfig) -> Self {
        let safe_methods = config
            .safe_methods
            .iter()
            .filter_map(|m| m.parse::<http::Method>().ok())
            .collect();

        let token_header = config
            .token_header
            .parse::<HeaderName>()
            .unwrap_or_else(|_| HeaderName::from_static("x-csrf-token"));

        Self {
            settings: Arc::new(CsrfSettings {
                cookie_name: config.cookie_name.clone(),
                token_header,
                form_field: config.form_field.clone(),
                safe_methods,
                exempt_paths: config.exempt_paths.clone(),
                signing_keys: None,
                max_scan_bytes: 2 * 1024 * 1024,
            }),
        }
    }

    /// Limit the form-body bytes read when scanning for the CSRF token field.
    ///
    /// Honors the caller-supplied `n` verbatim. The router passes the configured
    /// `security.upload.max_request_size_bytes` here so the token scan cap tracks
    /// the request body limit: a zero-JS multipart upload up to that size can
    /// still have its `_csrf` field located (see issue #1866). The request body
    /// is already bounded upstream by the global `DefaultBodyLimit` (also derived
    /// from `max_request_size_bytes`), so buffering up to `n` here introduces no
    /// new unbounded-memory exposure. When this method is not called the default
    /// from [`Self::from_config`] (2 MiB) applies.
    #[must_use]
    pub(crate) fn with_max_scan_bytes(mut self, n: usize) -> Self {
        let settings = Arc::make_mut(&mut self.settings);
        settings.max_scan_bytes = n;
        self
    }

    /// Attach signing keys so CSRF tokens are HMAC-signed.
    ///
    /// When set, tokens are in `{uuid}.{hmac_hex}` format. Unsigned tokens are
    /// rejected. Previous keys (see `ResolvedSigningKeys`) allow tokens signed
    /// with an old key to remain valid during a rotation grace window.
    #[must_use]
    pub fn with_signing_keys(
        mut self,
        keys: Arc<crate::security::config::ResolvedSigningKeys>,
    ) -> Self {
        Arc::make_mut(&mut self.settings).signing_keys = Some(keys);
        self
    }

    /// Add a path prefix that is exempt from CSRF validation.
    #[must_use]
    pub fn with_exempt_path(mut self, path: impl Into<String>) -> Self {
        Arc::make_mut(&mut self.settings)
            .exempt_paths
            .push(path.into());
        self
    }
}

impl<S> Layer<S> for CsrfLayer {
    type Service = CsrfService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        CsrfService {
            inner,
            settings: Arc::clone(&self.settings),
        }
    }
}

/// Tower [`Service`] produced by [`CsrfLayer`].
#[derive(Clone, Debug)]
pub struct CsrfService<S> {
    inner: S,
    settings: Arc<CsrfSettings>,
}

use subtle::{Choice, ConstantTimeEq};

/// Constant-time string comparison to prevent timing attacks when verifying CSRF tokens.
///
/// The comparison always processes exactly `b.len()` bytes so that execution
/// time is independent of the length of the submitted token `a`.  Neither a
/// length mismatch nor a short input causes an early exit.
#[inline(never)]
fn constant_time_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();

    // Constant-time length check — no early exit.
    let len_eq = a.len().ct_eq(&b.len());

    // Iterate over `a` (the trusted stored token) so the loop count is fixed
    // at the server-side token length, regardless of what the caller submits
    // as `b`.  Callers pass the attacker-controlled value as `b`, so iterating
    // over `a` ensures every submission — short or long — executes the same
    // amount of work.  Out-of-range positions in `b` use the sentinel 0xFF,
    // which can never match a valid ASCII/UTF-8 token byte.
    let mut bytes_eq = Choice::from(1u8);
    for (i, &a_byte) in a.iter().enumerate() {
        let b_byte = *b.get(i).unwrap_or(&0xFF);
        bytes_eq &= a_byte.ct_eq(&b_byte);
    }

    (len_eq & bytes_eq).into()
}

/// Extract the CSRF cookie value from the Cookie header.
fn extract_cookie_token(req_headers: &http::HeaderMap, cookie_name: &str) -> Option<String> {
    let mut found_token = None;

    for cookie_header in &req_headers.get_all(http::header::COOKIE) {
        let Ok(cookie_str) = cookie_header.to_str() else {
            continue;
        };

        for pair in cookie_str.split(';') {
            let pair = pair.trim();
            let Some((name, value)) = pair.split_once('=') else {
                continue;
            };

            if name.trim() != cookie_name {
                continue;
            }

            if found_token.is_some() {
                // Multiple cookies with the same name found.
                // This indicates a potential Cookie Tossing attack!
                // Reject by returning None.
                return None;
            }

            found_token = Some(value.trim().to_owned());
        }
    }

    found_token
}

impl<S, ResBody> Service<Request<axum::body::Body>> for CsrfService<S>
where
    S: Service<Request<axum::body::Body>, Response = Response<ResBody>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
    ResBody: From<&'static str> + From<String> + Default + Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request<axum::body::Body>) -> Self::Future {
        // Match exemptions against the normalized path so dot-segment tricks
        // like `/api/../submit` (or percent-encoded `%2e%2e` variants) cannot
        // satisfy an `/api/` exemption prefix while targeting another route.
        let clean = crate::security::path::clean_path(req.uri().path());
        let path = clean.as_str();
        let is_exempt = self.settings.exempt_paths.iter().any(|prefix| {
            if path == prefix {
                true
            } else if let Some(stripped) = path.strip_prefix(prefix) {
                prefix.ends_with('/') || stripped.starts_with('/')
            } else {
                false
            }
        });
        let is_safe = is_exempt || self.settings.safe_methods.contains(req.method());
        let raw_cookie_token = extract_cookie_token(req.headers(), &self.settings.cookie_name);

        // When signing is active, discard any cookie that fails HMAC verification
        // (unsigned pre-upgrade cookies, removed-key cookies, etc.) so a fresh signed
        // token is minted and the Set-Cookie header refreshes the browser value.
        let cookie_token = match (&raw_cookie_token, &self.settings.signing_keys) {
            (Some(tok), Some(_)) if !validate_cookie_token_hmac(tok, &self.settings) => None,
            _ => raw_cookie_token.clone(),
        };

        // Generate a new token if none exists in the cookie.
        // When signing keys are active, the token is {uuid}.{hmac_hex}.
        let token = cookie_token.clone().unwrap_or_else(|| {
            let raw = Uuid::new_v4().to_string();
            if let Some(keys) = &self.settings.signing_keys {
                let sig = keys.sign(raw.as_bytes());
                format!("{raw}.{sig}")
            } else {
                raw
            }
        });

        // Insert CsrfToken, the configured form field name, and the configured
        // token header name into request extensions.
        req.extensions_mut().insert(CsrfToken(token.clone()));
        req.extensions_mut()
            .insert(CsrfFormField(self.settings.form_field.clone()));
        req.extensions_mut().insert(CsrfTokenHeader(
            self.settings.token_header.as_str().to_owned(),
        ));

        // Check if we need to set a cookie
        let set_cookie = if cookie_token.is_none() {
            Some(format!(
                "{}={}; Path=/; SameSite=Lax; HttpOnly",
                self.settings.cookie_name, token
            ))
        } else {
            None
        };

        let settings = Arc::clone(&self.settings);
        let mut inner = self.inner.clone();

        // Swap to ensure correct poll_ready semantics
        std::mem::swap(&mut self.inner, &mut inner);

        Box::pin(async move {
            if !is_safe {
                let verdict = verify_csrf_token(&mut req, &settings, cookie_token.as_deref()).await;
                if !matches!(verdict, CsrfVerdict::Valid) {
                    let request_id = req
                        .extensions()
                        .get::<crate::middleware::RequestId>()
                        .map(std::string::ToString::to_string);
                    let instance = Some(req.uri().path().to_owned());
                    let prefers_problem = wants_problem_details(req.headers());

                    // Oversized form body → 413; anything else (missing/invalid
                    // token, unreadable body) → 403.
                    let (status, message) = match verdict {
                        CsrfVerdict::BodyTooLarge => (
                            StatusCode::PAYLOAD_TOO_LARGE,
                            CSRF_PAYLOAD_TOO_LARGE_MESSAGE,
                        ),
                        _ => (StatusCode::FORBIDDEN, CSRF_FORBIDDEN_MESSAGE),
                    };

                    if prefers_problem {
                        return Ok(csrf_problem_response(status, message, request_id, instance));
                    }

                    let mut response = Response::new(ResBody::from(message));
                    *response.status_mut() = status;
                    response.headers_mut().insert(
                        http::header::CONTENT_TYPE,
                        http::HeaderValue::from_static("text/plain; charset=utf-8"),
                    );
                    return Ok(response);
                }
            }

            // Validation passed (or method is safe)
            let mut response = inner.call(req).await?;

            if let Some(cookie) = set_cookie
                && let Ok(val) = http::header::HeaderValue::from_str(&cookie)
            {
                response.headers_mut().append(http::header::SET_COOKIE, val);
            }

            Ok(response)
        })
    }
}

fn wants_problem_details(headers: &http::HeaderMap) -> bool {
    !crate::middleware::error_page_filter::accept_prefers_html(headers)
}

fn csrf_problem_response<ResBody: From<String> + Default>(
    status: StatusCode,
    message: &str,
    request_id: Option<String>,
    instance: Option<String>,
) -> Response<ResBody> {
    // The 403 case keeps the CSRF-specific problem type/code; the 413 case
    // (oversized form body, see issue #1866) uses the module-wide
    // payload-too-large mapping derived from the status.
    let explicit_type =
        (status == StatusCode::FORBIDDEN).then_some("https://autumn.dev/problems/csrf");
    let mut problem = crate::error::problem_details(
        status,
        message.to_owned(),
        None,
        explicit_type,
        request_id,
        instance,
        true,
    );
    if status == StatusCode::FORBIDDEN {
        "autumn.csrf".clone_into(&mut problem.code);
    }
    let body = crate::error::problem_details_to_json_string(&problem);

    Response::builder()
        .status(status)
        .header(http::header::CONTENT_TYPE, "application/problem+json")
        .body(ResBody::from(body))
        .unwrap_or_default()
}

/// Validate a CSRF cookie token's HMAC when signing is active.
///
/// Returns `false` when signing keys are set but the token is unsigned or carries
/// an invalid HMAC (catches tampered or pre-rotation unsigned tokens).
fn validate_cookie_token_hmac(cookie_token: &str, settings: &CsrfSettings) -> bool {
    let Some(keys) = &settings.signing_keys else {
        return true; // signing not active — accept raw token
    };
    // Signed format: "{uuid}.{hmac_hex}"
    let Some((uuid_part, sig)) = cookie_token.split_once('.') else {
        return false; // unsigned token rejected when signing is required
    };
    keys.verify(uuid_part.as_bytes(), sig)
}

/// Extract the `boundary` parameter from a `multipart/form-data` Content-Type value.
fn extract_multipart_boundary(content_type: &str) -> Option<&str> {
    content_type.split(';').find_map(|part| {
        part.trim()
            .strip_prefix("boundary=")
            .map(|b| b.trim_matches('"'))
    })
}

/// Return the byte position of the first occurrence of `needle` in `haystack`.
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Scan a buffered `multipart/form-data` body for a named text field.
///
/// Returns the field value as a `&str` slice into `bytes`, or `None` when the
/// field is absent or the body is malformed / truncated.  Callers pre-limit the
/// buffer via `max_scan_bytes` so we never allocate more than that.
fn scan_multipart_field<'a>(bytes: &'a [u8], boundary: &str, field_name: &str) -> Option<&'a str> {
    let delimiter = format!("--{boundary}");
    let delim = delimiter.as_bytes();
    let end_marker = format!("\r\n{delimiter}");
    let end_bytes = end_marker.as_bytes();
    let mut pos = 0;

    loop {
        let rel = find_bytes(&bytes[pos..], delim)?;
        pos += rel + delim.len();

        // After the boundary: \r\n begins a part; anything else ends the multipart.
        match bytes.get(pos..pos + 2) {
            Some(b"\r\n") => pos += 2,
            _ => break, // final boundary (--), truncated, or malformed
        }

        let header_end = find_bytes(&bytes[pos..], b"\r\n\r\n")?;
        let headers = std::str::from_utf8(&bytes[pos..pos + header_end]).ok()?;
        let value_start = pos + header_end + 4;

        let is_match = headers.lines().any(|line| {
            if !line
                .to_ascii_lowercase()
                .starts_with("content-disposition:")
            {
                return false;
            }
            line.split(';').skip(1).any(|attr| {
                attr.trim()
                    .strip_prefix("name=")
                    .map(|v| v.trim_matches('"'))
                    == Some(field_name)
            })
        });

        if is_match {
            let end = find_bytes(&bytes[value_start..], end_bytes)
                .map_or(bytes.len(), |i| value_start + i);
            return std::str::from_utf8(&bytes[value_start..end]).ok();
        }

        let next = find_bytes(&bytes[value_start..], end_bytes)?;
        // Advance to the start of the boundary delimiter (skip only the leading
        // \r\n of end_bytes so the next loop iteration finds --boundary at
        // rel=0 and processes it normally).
        pos = value_start + next + 2;
    }

    None
}

async fn verify_csrf_token(
    req: &mut Request<axum::body::Body>,
    settings: &CsrfSettings,
    cookie_token: Option<&str>,
) -> CsrfVerdict {
    let mut token_found = false;

    // 1. Check header
    let header_token = req
        .headers()
        .get(&settings.token_header)
        .and_then(|v| v.to_str().ok());

    if let (Some(c), Some(h)) = (cookie_token, header_token)
        && !c.is_empty()
        && !h.is_empty()
        && validate_cookie_token_hmac(c, settings)
        && constant_time_eq(c, h)
    {
        token_found = true;
    }

    if token_found {
        return CsrfVerdict::Valid;
    }

    // 1b. Check query parameter (e.g. `_csrf`) before falling back to body
    let query_token = req.uri().query().and_then(|q| {
        url::form_urlencoded::parse(q.as_bytes())
            .find(|(key, _)| key == "_csrf" || key == settings.form_field.as_str())
            .map(|(_, val)| val.into_owned())
    });

    if let (Some(c), Some(q)) = (cookie_token, &query_token)
        && !c.is_empty()
        && !q.is_empty()
        && validate_cookie_token_hmac(c, settings)
        && constant_time_eq(c, q)
    {
        token_found = true;
    }

    if token_found {
        return CsrfVerdict::Valid;
    }

    // 2. Check form field (if not found in header)
    let content_type = req
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();

    let is_urlencoded = content_type.starts_with("application/x-www-form-urlencoded");
    let multipart_boundary = if content_type.starts_with("multipart/form-data") {
        extract_multipart_boundary(content_type).map(str::to_owned)
    } else {
        None
    };
    // NLL: content_type borrow ends here; req.body_mut() is safe to call below.

    if !is_urlencoded && multipart_boundary.is_none() {
        return CsrfVerdict::Missing;
    }

    // Temporarily take ownership of the body
    let body = std::mem::replace(req.body_mut(), axum::body::Body::empty());

    // Limit body size to avoid DoS when extracting form field.
    let bytes = match axum::body::to_bytes(body, settings.max_scan_bytes).await {
        Ok(bytes) => bytes,
        Err(err) if is_body_length_limit_error(&err) => {
            // The form body carrying `_csrf` is larger than the scan cap (which
            // tracks `upload.max_request_size_bytes`). Surface this as
            // `413 Payload Too Large` instead of falling through to a misleading
            // `403` "missing token" — the token isn't absent, the body is simply
            // over the request-size limit (see issue #1866).
            return CsrfVerdict::BodyTooLarge;
        }
        Err(_) => {
            // A non-length body read error (e.g. the client disconnected
            // mid-body). This module has no dedicated malformed-body path, so we
            // preserve the pre-existing behavior of treating an unreadable body
            // as a missing token (→ 403) rather than inventing a new 400 here.
            return CsrfVerdict::Missing;
        }
    };

    if is_urlencoded {
        for (key, value) in url::form_urlencoded::parse(&bytes) {
            if key == settings.form_field {
                if let Some(c) = cookie_token
                    && !c.is_empty()
                    && !value.is_empty()
                    && validate_cookie_token_hmac(c, settings)
                    && constant_time_eq(c, value.as_ref())
                {
                    token_found = true;
                }
                break;
            }
        }
    } else if let Some(ref boundary) = multipart_boundary {
        #[allow(clippy::collapsible_if)]
        if let Some(value) = scan_multipart_field(&bytes, boundary, &settings.form_field) {
            if let Some(c) = cookie_token
                && !c.is_empty()
                && !value.is_empty()
                && validate_cookie_token_hmac(c, settings)
                && constant_time_eq(c, value)
            {
                token_found = true;
            }
        }
    }

    // Restore request body so downstream handlers (e.g. Multipart extractor) can read it.
    *req.body_mut() = axum::body::Body::from(bytes);

    if token_found {
        CsrfVerdict::Valid
    } else {
        CsrfVerdict::Missing
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn post_with_url_encoded_token_passes() {
        let raw_token = "abc+123/xyz=456";
        let encoded_token = "abc%2B123%2Fxyz%3D456";
        let app = Router::new()
            .route("/submit", post(|| async { "created" }))
            .layer(CsrfLayer::from_config(&default_csrf_config()));

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/submit")
                    .header("Cookie", format!("autumn-csrf={raw_token}"))
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .body(Body::from(format!("_csrf={encoded_token}")))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn post_with_query_param_token_passes() {
        let raw_token = "abc+123/xyz=456";
        let encoded_token = "abc%2B123%2Fxyz%3D456";
        let app = Router::new()
            .route("/submit", post(|| async { "created" }))
            .layer(CsrfLayer::from_config(&default_csrf_config()));

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/submit?_csrf={encoded_token}"))
                    .header("Cookie", format!("autumn-csrf={raw_token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::routing::{get, post};
    use std::fmt::Write as _;
    use tower::ServiceExt;

    fn default_csrf_config() -> CsrfConfig {
        CsrfConfig {
            enabled: true,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn safe_method_passes_without_token() {
        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(CsrfLayer::from_config(&default_csrf_config()));

        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn safe_method_sets_csrf_cookie() {
        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(CsrfLayer::from_config(&default_csrf_config()));

        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let set_cookie = response
            .headers()
            .get("set-cookie")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(set_cookie.starts_with("autumn-csrf="));
        assert!(set_cookie.contains("HttpOnly"));
    }

    #[tokio::test]
    async fn post_without_token_returns_403() {
        let app = Router::new()
            .route("/submit", post(|| async { "created" }))
            .layer(CsrfLayer::from_config(&default_csrf_config()));

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/submit")
                    .header(http::header::ACCEPT, "text/html")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn forbidden_response_has_clear_error_body() {
        let app = Router::new()
            .route("/submit", post(|| async { "created" }))
            .layer(CsrfLayer::from_config(&default_csrf_config()));

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/submit")
                    .header(http::header::ACCEPT, "text/html")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_TYPE)
                .map(|v| v.to_str().unwrap_or_default()),
            Some("text/plain; charset=utf-8")
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = std::str::from_utf8(&body).unwrap();
        assert!(
            text.contains("CSRF"),
            "expected CSRF error message, got: {text:?}"
        );
    }

    #[tokio::test]
    async fn exempt_path_skips_csrf_validation() {
        let config = CsrfConfig {
            enabled: true,
            exempt_paths: vec!["/api/".to_string()],
            ..Default::default()
        };
        let app = Router::new()
            .route("/api/items", post(|| async { "created" }))
            .route("/form/submit", post(|| async { "created" }))
            .layer(CsrfLayer::from_config(&config));

        // Exempt API path: POST with no token should succeed.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/items")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // Non-exempt form path: POST with no token should still be blocked.
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/form/submit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn exempt_path_exact_or_subtree_only() {
        let config = CsrfConfig {
            enabled: true,
            exempt_paths: vec!["/webhooks/stripe".to_string()],
            ..Default::default()
        };
        let app = Router::new()
            .route("/webhooks/stripe", post(|| async { "stripe" }))
            .route(
                "/webhooks/stripe/events",
                post(|| async { "stripe events" }),
            )
            .route("/webhooks/stripe-admin", post(|| async { "stripe admin" }))
            .layer(CsrfLayer::from_config(&config));

        // Exact match of exempt path should skip CSRF validation
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhooks/stripe")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // Slash-delimited subtree of exempt path should skip CSRF validation
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhooks/stripe/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // Unrelated path starting with same prefix should NOT skip CSRF validation
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhooks/stripe-admin")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn post_with_valid_token_passes() {
        let token = Uuid::new_v4().to_string();
        let app = Router::new()
            .route("/submit", post(|| async { "created" }))
            .layer(CsrfLayer::from_config(&default_csrf_config()));

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/submit")
                    .header("Cookie", format!("autumn-csrf={token}"))
                    .header("X-CSRF-Token", &token)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn post_with_mismatched_token_returns_403() {
        let cookie_token = Uuid::new_v4().to_string();
        let header_token = Uuid::new_v4().to_string();
        let app = Router::new()
            .route("/submit", post(|| async { "created" }))
            .layer(CsrfLayer::from_config(&default_csrf_config()));

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/submit")
                    .header("Cookie", format!("autumn-csrf={cookie_token}"))
                    .header("X-CSRF-Token", &header_token)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn csrf_token_extractor_works() {
        async fn handler(csrf: CsrfToken) -> String {
            csrf.token().to_owned()
        }

        let app = Router::new()
            .route("/", get(handler))
            .layer(CsrfLayer::from_config(&default_csrf_config()));

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let token_str = String::from_utf8(body.to_vec()).unwrap();
        assert!(Uuid::parse_str(&token_str).is_ok());
    }

    #[test]
    fn extract_cookie_from_header() {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::COOKIE,
            "autumn-csrf=abc123; other=xyz".parse().unwrap(),
        );
        assert_eq!(
            extract_cookie_token(&headers, "autumn-csrf"),
            Some("abc123".to_owned())
        );
    }

    #[test]
    fn missing_cookie_returns_none() {
        let headers = http::HeaderMap::new();
        assert_eq!(extract_cookie_token(&headers, "autumn-csrf"), None);
    }

    #[test]
    fn extract_cookie_rejects_multiple_cookies() {
        // Multiple cookies with the same name in a single header
        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::COOKIE,
            "autumn-csrf=abc123; autumn-csrf=xyz456".parse().unwrap(),
        );
        assert_eq!(extract_cookie_token(&headers, "autumn-csrf"), None);

        // Multiple headers with the same cookie
        let mut headers2 = http::HeaderMap::new();
        headers2.append(http::header::COOKIE, "autumn-csrf=abc123".parse().unwrap());
        headers2.append(http::header::COOKIE, "autumn-csrf=xyz456".parse().unwrap());
        assert_eq!(extract_cookie_token(&headers2, "autumn-csrf"), None);
    }

    #[test]
    fn extract_cookie_ignores_malformed_cookies() {
        let mut headers = http::HeaderMap::new();
        // Missing '='
        headers.insert(http::header::COOKIE, "autumn-csrf abc123".parse().unwrap());
        assert_eq!(extract_cookie_token(&headers, "autumn-csrf"), None);

        // Multiple spaces
        headers.insert(
            http::header::COOKIE,
            "   autumn-csrf  =  abc123  ; other=xyz".parse().unwrap(),
        );
        assert_eq!(
            extract_cookie_token(&headers, "autumn-csrf"),
            Some("abc123".to_owned())
        );
    }

    #[test]
    fn test_constant_time_eq() {
        assert!(super::constant_time_eq("abc", "abc"));
        assert!(!super::constant_time_eq("abc", "ab"));
        assert!(!super::constant_time_eq("abc", "abd"));
        assert!(super::constant_time_eq("", ""));
        assert!(!super::constant_time_eq("a", "b"));
        assert!(!super::constant_time_eq("a", "A"));
    }

    #[tokio::test]
    async fn post_with_empty_cookie_but_valid_header() {
        let token = Uuid::new_v4().to_string();
        let app = Router::new()
            .route("/submit", post(|| async { "created" }))
            .layer(CsrfLayer::from_config(&default_csrf_config()));

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/submit")
                    .header("Cookie", "autumn-csrf=")
                    .header("X-CSRF-Token", &token)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn post_with_valid_cookie_but_empty_header() {
        let token = Uuid::new_v4().to_string();
        let app = Router::new()
            .route("/submit", post(|| async { "created" }))
            .layer(CsrfLayer::from_config(&default_csrf_config()));

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/submit")
                    .header("Cookie", format!("autumn-csrf={token}"))
                    .header("X-CSRF-Token", "")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn post_with_empty_cookie_but_valid_form_field() {
        let token = Uuid::new_v4().to_string();
        let app = Router::new()
            .route("/submit", post(|| async { "created" }))
            .layer(CsrfLayer::from_config(&default_csrf_config()));

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/submit")
                    .header("Cookie", "autumn-csrf=")
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .body(Body::from(format!("_csrf={token}")))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn post_with_valid_cookie_but_empty_form_field() {
        let token = Uuid::new_v4().to_string();
        let app = Router::new()
            .route("/submit", post(|| async { "created" }))
            .layer(CsrfLayer::from_config(&default_csrf_config()));

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/submit")
                    .header("Cookie", format!("autumn-csrf={token}"))
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .body(Body::from("_csrf="))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn post_with_large_body_returns_413() {
        let token = Uuid::new_v4().to_string();
        let app = Router::new()
            .route("/submit", post(|| async { "created" }))
            .layer(CsrfLayer::from_config(&default_csrf_config()));

        // Create a body just over the 2 MiB default scan cap. `to_bytes` cannot
        // buffer it to locate `_csrf`, so the CSRF layer now returns 413 Payload
        // Too Large (the token isn't missing — the body is oversized; see #1866).
        let large_padding = "a".repeat(2 * 1024 * 1024 + 10);
        let body_content = format!("_csrf={token}&pad={large_padding}");

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/submit")
                    .header("Cookie", format!("autumn-csrf={token}"))
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .body(Body::from(body_content))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn post_with_empty_tokens_returns_403() {
        let app = Router::new()
            .route("/submit", post(|| async { "created" }))
            .layer(CsrfLayer::from_config(&CsrfConfig {
                enabled: true,
                ..Default::default()
            }));

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/submit")
                    .header("Cookie", "autumn-csrf=")
                    .header("X-CSRF-Token", "")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn post_with_empty_form_tokens_returns_403() {
        let app = Router::new()
            .route("/submit", post(|| async { "created" }))
            .layer(CsrfLayer::from_config(&CsrfConfig {
                enabled: true,
                ..Default::default()
            }));

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/submit")
                    .header("Cookie", "autumn-csrf=")
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .body(Body::from("_csrf="))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn from_config_filters_invalid_methods() {
        let config = CsrfConfig {
            safe_methods: vec![
                "GET".to_string(),
                "INVALID METHOD".to_string(),
                "POST".to_string(),
            ],
            ..Default::default()
        };
        let layer = CsrfLayer::from_config(&config);
        assert_eq!(layer.settings.safe_methods.len(), 2);
        assert!(layer.settings.safe_methods.contains(&http::Method::GET));
        assert!(layer.settings.safe_methods.contains(&http::Method::POST));
    }

    #[test]
    fn from_config_handles_invalid_header_name() {
        let config = CsrfConfig {
            token_header: "Invalid Header Name\n".to_string(),
            ..Default::default()
        };
        let layer = CsrfLayer::from_config(&config);
        assert_eq!(layer.settings.token_header.as_str(), "x-csrf-token");
    }

    // ── Signed CSRF tokens (RED phase) ────────────────────────────────────

    #[tokio::test]
    async fn csrf_token_is_hmac_signed_when_keys_set() {
        use crate::security::config::{SigningSecretConfig, resolve_signing_keys};
        use std::sync::Arc;

        let keys = Arc::new(resolve_signing_keys(&SigningSecretConfig {
            secret: Some("k".repeat(32)),
            previous_secrets: vec![],
        }));
        let layer = CsrfLayer::from_config(&default_csrf_config()).with_signing_keys(keys);

        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(layer);

        let resp = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        let set_cookie = resp
            .headers()
            .get("set-cookie")
            .expect("should set CSRF cookie")
            .to_str()
            .unwrap();
        let cookie_value = set_cookie
            .split('=')
            .nth(1)
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .trim();

        assert!(
            cookie_value.contains('.'),
            "signed CSRF cookie must be {{uuid}}.{{hmac}}, got: {cookie_value}"
        );
        let (_uuid_part, sig_part) = cookie_value.split_once('.').unwrap();
        assert_eq!(sig_part.len(), 64, "HMAC hex must be 64 chars");
    }

    #[tokio::test]
    async fn csrf_signed_token_validates_on_post() {
        use crate::security::config::{SigningSecretConfig, resolve_signing_keys};
        use std::sync::Arc;

        let keys = Arc::new(resolve_signing_keys(&SigningSecretConfig {
            secret: Some("k".repeat(32)),
            previous_secrets: vec![],
        }));
        let layer = CsrfLayer::from_config(&default_csrf_config()).with_signing_keys(keys);

        let app = Router::new()
            .route("/", post(|| async { "created" }))
            .layer(layer);

        // Mint a valid signed token
        let config = SigningSecretConfig {
            secret: Some("k".repeat(32)),
            previous_secrets: vec![],
        };
        let signing_keys = resolve_signing_keys(&config);
        let uuid = uuid::Uuid::new_v4().to_string();
        let sig = signing_keys.sign(uuid.as_bytes());
        let signed_token = format!("{uuid}.{sig}");

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/")
                    .header("Cookie", format!("autumn-csrf={signed_token}"))
                    .header("X-CSRF-Token", &signed_token)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn csrf_unsigned_token_rejected_when_signing_active() {
        use crate::security::config::{SigningSecretConfig, resolve_signing_keys};
        use std::sync::Arc;

        let keys = Arc::new(resolve_signing_keys(&SigningSecretConfig {
            secret: Some("k".repeat(32)),
            previous_secrets: vec![],
        }));
        let layer = CsrfLayer::from_config(&default_csrf_config()).with_signing_keys(keys);

        let app = Router::new()
            .route("/", post(|| async { "created" }))
            .layer(layer);

        // Raw UUID without HMAC — should be rejected when signing is active
        let raw_token = uuid::Uuid::new_v4().to_string();
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/")
                    .header("Cookie", format!("autumn-csrf={raw_token}"))
                    .header("X-CSRF-Token", &raw_token)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            resp.status(),
            StatusCode::FORBIDDEN,
            "unsigned CSRF token must be rejected when signing is active"
        );
    }

    #[tokio::test]
    async fn csrf_previous_key_signed_token_accepted() {
        use crate::security::config::{
            ResolvedSigningKeys, SigningSecretConfig, resolve_signing_keys,
        };
        use std::sync::Arc;

        let old_secret = "old-key".repeat(5); // 35 bytes
        let old_keys = resolve_signing_keys(&SigningSecretConfig {
            secret: Some(old_secret.clone()),
            previous_secrets: vec![],
        });

        let uuid = uuid::Uuid::new_v4().to_string();
        let old_sig = old_keys.sign(uuid.as_bytes());
        let old_signed_token = format!("{uuid}.{old_sig}");

        let new_keys = Arc::new(ResolvedSigningKeys::new(
            "new-key".repeat(5).into_bytes(),
            vec![old_secret.into_bytes()],
        ));
        let layer = CsrfLayer::from_config(&default_csrf_config()).with_signing_keys(new_keys);

        let app = Router::new()
            .route("/", post(|| async { "created" }))
            .layer(layer);

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/")
                    .header("Cookie", format!("autumn-csrf={old_signed_token}"))
                    .header("X-CSRF-Token", &old_signed_token)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "previous-key-signed CSRF token must pass during grace window"
        );
    }

    fn multipart_body(boundary: &str, fields: &[(&str, &str)]) -> String {
        let mut body = String::new();
        for (name, value) in fields {
            let _ = write!(
                body,
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            );
        }
        let _ = write!(body, "--{boundary}--\r\n");
        body
    }

    #[tokio::test]
    async fn post_multipart_with_csrf_field_passes() {
        let token = "test-csrf-token-uuid-1234";
        let boundary = "----WebKitFormBoundaryABC123";
        let body = multipart_body(boundary, &[("_csrf", token), ("name", "alice")]);
        let app = Router::new()
            .route("/upload", post(|| async { "ok" }))
            .layer(CsrfLayer::from_config(&default_csrf_config()));

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/upload")
                    .header("Cookie", format!("autumn-csrf={token}"))
                    .header(
                        "Content-Type",
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn post_multipart_csrf_field_after_other_field_passes() {
        // Regression: skipping a non-matching part must not advance pos past
        // the next part's headers (the +2 fix in scan_multipart_field).
        let token = "test-csrf-token-uuid-after";
        let boundary = "----WebKitFormBoundaryORDER";
        let body = multipart_body(boundary, &[("name", "alice"), ("_csrf", token)]);
        let app = Router::new()
            .route("/upload", post(|| async { "ok" }))
            .layer(CsrfLayer::from_config(&default_csrf_config()));

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/upload")
                    .header("Cookie", format!("autumn-csrf={token}"))
                    .header(
                        "Content-Type",
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn post_multipart_csrf_field_first_with_file_passes() {
        let token = "test-csrf-token-uuid-5678";
        let boundary = "----WebKitFormBoundaryDEF456";
        // _csrf first, then binary file field
        let mut body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"_csrf\"\r\n\r\n{token}\r\n"
        );
        let _ = write!(
            body,
            "--{boundary}\r\nContent-Disposition: form-data; name=\"avatar\"; filename=\"photo.jpg\"\r\nContent-Type: image/jpeg\r\n\r\nFAKEJPEGDATA\r\n"
        );
        let _ = write!(body, "--{boundary}--\r\n");

        let app = Router::new()
            .route("/upload", post(|| async { "ok" }))
            .layer(CsrfLayer::from_config(&default_csrf_config()));

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/upload")
                    .header("Cookie", format!("autumn-csrf={token}"))
                    .header(
                        "Content-Type",
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn post_multipart_without_csrf_field_rejected() {
        let boundary = "----WebKitFormBoundaryGHI789";
        let body = multipart_body(boundary, &[("file", "fakebytes")]);
        let app = Router::new()
            .route("/upload", post(|| async { "ok" }))
            .layer(CsrfLayer::from_config(&default_csrf_config()));

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/upload")
                    .header("Cookie", "autumn-csrf=sometoken")
                    .header(http::header::ACCEPT, "text/html")
                    .header(
                        "Content-Type",
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn post_multipart_with_wrong_csrf_token_rejected() {
        let boundary = "----WebKitFormBoundaryJKL012";
        let body = multipart_body(boundary, &[("_csrf", "wrong-token")]);
        let app = Router::new()
            .route("/upload", post(|| async { "ok" }))
            .layer(CsrfLayer::from_config(&default_csrf_config()));

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/upload")
                    .header("Cookie", "autumn-csrf=correct-token")
                    .header(http::header::ACCEPT, "text/html")
                    .header(
                        "Content-Type",
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    /// Build a multipart body larger than the legacy 2 MiB scan cap: a valid
    /// `_csrf` field positioned first, followed by a padded binary file field.
    fn large_multipart_body(boundary: &str, token: &str, pad_bytes: usize) -> Vec<u8> {
        let mut body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"_csrf\"\r\n\r\n{token}\r\n"
        )
        .into_bytes();
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"avatar\"; \
                 filename=\"photo.bin\"\r\nContent-Type: application/octet-stream\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend(std::iter::repeat_n(b'A', pad_bytes));
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        body
    }

    /// Regression test for issue #1866. A zero-JS multipart upload larger than the
    /// legacy hard-clamped 2 MiB scan window — but within the configured scan cap —
    /// must still have its `_csrf` field located, so it is NOT rejected with 403.
    #[tokio::test]
    async fn post_large_multipart_within_scan_cap_finds_token_passes() {
        let token = "test-csrf-token-uuid-1866";
        let boundary = "----WebKitFormBoundary1866PASS";
        // 3 MiB of padding => whole body exceeds the legacy 2 MiB clamp.
        let body = large_multipart_body(boundary, token, 3 * 1024 * 1024);
        assert!(body.len() > 2 * 1024 * 1024);

        // Router wires `with_max_scan_bytes(upload.max_request_size_bytes)`; here we
        // model a configured 4 MiB request-body limit (> the legacy 2 MiB clamp).
        let app = Router::new()
            .route("/upload", post(|| async { "ok" }))
            .layer(
                CsrfLayer::from_config(&default_csrf_config()).with_max_scan_bytes(4 * 1024 * 1024),
            );

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/upload")
                    .header("Cookie", format!("autumn-csrf={token}"))
                    .header(http::header::ACCEPT, "text/html")
                    .header(
                        "Content-Type",
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    /// Issue #1866: when the form body carrying `_csrf` exceeds the scan cap, the
    /// token cannot be buffered, so it must NOT be misreported as a missing token
    /// (403). The body is genuinely over the request-size limit, so the CSRF layer
    /// now returns `413 Payload Too Large` — the response issue #1866 wanted to be
    /// observable, rather than a misleading 403. Here the scan cap is left at the
    /// 2 MiB default (no `with_max_scan_bytes` override) and the body is > 2 MiB.
    #[tokio::test]
    async fn post_large_multipart_exceeding_scan_cap_returns_413() {
        let token = "test-csrf-token-uuid-1866b";
        let boundary = "----WebKitFormBoundary1866FAIL";
        let body = large_multipart_body(boundary, token, 3 * 1024 * 1024);
        assert!(body.len() > 2 * 1024 * 1024);

        // Default cap is 2 MiB (from_config, no with_max_scan_bytes override).
        let app = Router::new()
            .route("/upload", post(|| async { "ok" }))
            .layer(CsrfLayer::from_config(&default_csrf_config()));

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/upload")
                    .header("Cookie", format!("autumn-csrf={token}"))
                    .header(http::header::ACCEPT, "text/html")
                    .header(
                        "Content-Type",
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    /// The overflow is classified before token presence: a form body over the scan
    /// cap that carries NO `_csrf` field still returns 413 (not the 403 a genuinely
    /// missing token would produce), confirming the size limit is evaluated first.
    #[tokio::test]
    async fn post_large_multipart_over_cap_without_token_returns_413() {
        let boundary = "----WebKitFormBoundary1866NOTOK";
        // Build an oversized body with a file field but no `_csrf` part.
        let mut body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"avatar\"; \
             filename=\"photo.bin\"\r\nContent-Type: application/octet-stream\r\n\r\n"
        )
        .into_bytes();
        body.extend(std::iter::repeat_n(b'A', 3 * 1024 * 1024));
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        assert!(body.len() > 2 * 1024 * 1024);

        // Default cap is 2 MiB (from_config, no with_max_scan_bytes override).
        let app = Router::new()
            .route("/upload", post(|| async { "ok" }))
            .layer(CsrfLayer::from_config(&default_csrf_config()));

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/upload")
                    .header("Cookie", "autumn-csrf=sometoken")
                    .header(http::header::ACCEPT, "text/html")
                    .header(
                        "Content-Type",
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }
}
