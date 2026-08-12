//! HTML form method-override middleware.
//!
//! Native browser HTML forms can only submit `GET` or `POST`. Autumn lets
//! plain `<form method="post">` submissions target `PUT`, `PATCH`, and
//! `DELETE` routes by carrying a hidden `_method` form field whose value
//! names the effective HTTP method.
//!
//! # How it works
//!
//! On every `POST` request whose body is a same-origin form submission
//! (content-type `application/x-www-form-urlencoded`), the middleware
//! looks for the configured override field (default `_method`). If
//! present and the value is one of the documented target methods
//! (`PUT`, `PATCH`, `DELETE`, case-insensitive), the request's HTTP
//! method is rewritten in place before route matching. The body is
//! preserved exactly so downstream extractors (including CSRF
//! validation) read it unchanged.
//!
//! # Same-origin requirement
//!
//! The override is strictly a convention for browser HTML forms
//! originating from the same origin. The layer enforces this rather
//! than relying solely on CSRF, because a route declared as `#[delete]`
//! would otherwise become reachable from any cross-origin form when
//! CSRF protection is disabled or the path is CSRF-exempt — a
//! reachability change browsers' same-origin policy would normally
//! prevent for direct `DELETE` requests.
//!
//! The check uses, in order:
//!
//! 1. `Sec-Fetch-Site: same-origin` or `none` — accept outright.
//! 2. `Sec-Fetch-Site: same-site` — **not** sufficient on its own
//!    because `same-site` accepts sibling origins under the same
//!    registrable domain (e.g. `evil.example.com` -> `app.example.com`).
//!    Fall back to the full `Origin` check before allowing.
//! 3. `Sec-Fetch-Site: cross-site` (or any other value) is rejected.
//! 4. When `Sec-Fetch-Site` is absent, fall back to the full `Origin`
//!    check.
//! 5. If `Origin` is missing too, the override is **not** applied —
//!    the request is forwarded as the original `POST` so route
//!    matching rejects it on its own (fail closed).
//!
//! The full `Origin` check compares scheme, host, and port:
//!
//! - The Origin host:port must match `X-Forwarded-Host` (if surfaced
//!   by the reverse proxy) or `Host`.
//! - If `X-Forwarded-Proto` is set (e.g. by a TLS-terminating proxy),
//!   the Origin's scheme must match it. The leftmost element of a
//!   comma-separated proxy chain is used.
//! - When neither `X-Forwarded-Proto` is set nor the request URI
//!   carries a scheme, the middleware can't observe the underlying
//!   transport, so it falls back to host:port matching alone.
//!   Deployments that need strict scheme enforcement should run
//!   behind a proxy that sets `X-Forwarded-Proto`.
//!
//! # Failure modes
//!
//! - An unrecognised override value (e.g. `_method=BREW`) is rejected
//!   with `400 Bad Request` before reaching the handler.
//! - A form-urlencoded body larger than the scan cap (2 MiB) is
//!   rejected with `413 Payload Too Large` even when it doesn't
//!   carry `_method`. The middleware can't peek at the form fields
//!   without scanning the body, so an oversized POST might be a
//!   `_method=DELETE` request whose intent we'd otherwise demote to
//!   a plain `POST` based purely on body size. Failing closed
//!   preserves the user's operation semantics. Form-urlencoded
//!   payloads near this size are uncommon — `multipart/form-data`
//!   is the conventional encoding for large submissions.
//! - The override only acts on `POST` requests with a form-style body.
//!   Headers like `X-HTTP-Method-Override` are intentionally **not**
//!   honoured: this convention is documented for browser HTML forms
//!   only, not for arbitrary REST tunneling.
//!
//! ## How the rejection flows through middleware
//!
//! The layer is wrapped outside the router so it can rewrite the HTTP
//! method **before route matching**. Returning a `400`/`413` directly
//! from that outer wrapper would bypass every framework layer applied
//! via `Router::layer` — security headers, request IDs, metrics,
//! exception filter, error-page renderer, and so on. Instead, the
//! layer stamps a [`MethodOverrideRejection`] extension on the request
//! and forwards it down the stack untouched. A companion middleware,
//! [`method_override_rejection_filter`], is applied as an inner
//! `Router::layer` so the rejection is converted into a `400`/`413`
//! response **inside** the framework's response middleware stack. The
//! result is the same status the user sees, but with security headers,
//! request IDs, metrics, and error-page rendering all applied.
//!
//! # Interaction with CSRF
//!
//! The CSRF layer wraps this middleware on the outside, so CSRF still
//! validates the original `POST` request — meaning an overridden
//! `DELETE` submission without a valid `_csrf` token is rejected
//! exactly like any other mutating POST.
//!
//! # Examples
//!
//! ```rust,ignore
//! use autumn_web::prelude::*;
//! use autumn_web::security::CsrfToken;
//!
//! #[get("/posts/{id}/edit")]
//! async fn edit_form(csrf: CsrfToken) -> Markup {
//!     html! {
//!         form method="post" action="/posts/42" {
//!             input type="hidden" name="_method" value="PUT";
//!             input type="hidden" name="_csrf" value=(csrf.token());
//!             input type="text" name="title";
//!             button { "Update" }
//!         }
//!     }
//! }
//!
//! #[put("/posts/{id}")]
//! async fn update(/* extractors */) -> Markup { html! { "updated" } }
//! ```

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

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::http::{Request, Response, StatusCode};
use tower::{Layer, Service};

use crate::security::trusted_proxies::ResolvedClientIdentity;

/// Default form field name for the method override.
pub const DEFAULT_METHOD_OVERRIDE_FIELD: &str = "_method";

/// Maximum body size (bytes) considered when parsing the override field.
///
/// Mirrors [`crate::security::csrf`] which uses the same 2 MiB cap for
/// peeking at form-urlencoded bodies. The body itself is unchanged.
const MAX_BODY_SCAN_BYTES: usize = 2 * 1024 * 1024;

/// Configuration for the method-override middleware.
#[derive(Debug, Clone)]
pub struct MethodOverrideConfig {
    /// Form field name to read the override value from.
    pub field_name: String,
}

impl Default for MethodOverrideConfig {
    fn default() -> Self {
        Self {
            field_name: DEFAULT_METHOD_OVERRIDE_FIELD.to_owned(),
        }
    }
}

/// Marker extension inserted on requests whose HTTP method was rewritten
/// by the override middleware. Useful for logs and instrumentation.
#[derive(Clone, Debug)]
pub struct OverriddenMethod {
    /// The transport method the browser actually used (always `POST`).
    pub transport: http::Method,
    /// The effective method the request was rewritten to.
    pub effective: http::Method,
}

/// Reason the override middleware needs to fail the request.
///
/// Stamped on the request extensions by the outer Tower layer so the
/// inner [`method_override_rejection_filter`] can convert it into a
/// proper `400`/`413` response that flows through the framework's
/// response middleware stack (security headers, request IDs, error
/// pages, etc.).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MethodOverrideRejection {
    /// `_method` carried a value that wasn't `PUT`, `PATCH`, or `DELETE`
    /// (case-insensitive). Rendered as `400 Bad Request`.
    InvalidValue,
    /// Form body was too large to buffer for override scanning and the
    /// bytes have already been consumed. Rendered as `413 Payload Too
    /// Large`.
    BodyTooLarge,
}

/// Inner middleware that converts a [`MethodOverrideRejection`] into a
/// `400`/`413` response.
///
/// Applied via `Router::layer` so the rejection is rendered inside the
/// per-route layer chain. Running here means the response inherits the
/// framework's outer middleware (security headers, request IDs,
/// metrics, error-page filter, exception filter) rather than bypassing
/// them.
///
/// The filter is a no-op when no rejection extension is present, so it
/// is safe to apply to every route in the router.
pub async fn method_override_rejection_filter(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use crate::middleware::exception_filter::AutumnErrorInfo;
    use axum::response::IntoResponse;

    if let Some(rejection) = request
        .extensions()
        .get::<MethodOverrideRejection>()
        .copied()
    {
        let (status, message) = match rejection {
            MethodOverrideRejection::InvalidValue => (
                StatusCode::BAD_REQUEST,
                "Invalid method override value: must be PUT, PATCH, or DELETE.",
            ),
            MethodOverrideRejection::BodyTooLarge => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "Form body too large for method-override scanning.",
            ),
        };
        let mut response = (
            status,
            [(
                http::header::CONTENT_TYPE,
                http::HeaderValue::from_static("text/plain; charset=utf-8"),
            )],
            message,
        )
            .into_response();
        // Surface this as a framework error so the exception filter
        // chain (problem-details normalization, custom HTML error-page
        // rendering) processes it the same as any handler-generated
        // error response. Without this extension, `ExceptionFilterFuture`
        // treats the response as pre-formed and skips renegotiation.
        response.extensions_mut().insert(AutumnErrorInfo {
            status,
            message: message.to_owned(),
            details: None,
            problem_type: None,
            backtrace_string: None,
        });
        return response;
    }
    next.run(request).await
}

/// Tower [`Layer`] that applies the HTML form method override convention.
#[derive(Clone, Debug)]
pub struct MethodOverrideLayer {
    config: Arc<MethodOverrideConfig>,
    max_scan_bytes: usize,
}

impl MethodOverrideLayer {
    /// Construct a layer with the default field name (`_method`).
    #[must_use]
    pub fn new() -> Self {
        Self::from_config(MethodOverrideConfig::default())
    }

    /// Construct a layer with the given configuration.
    #[must_use]
    pub fn from_config(config: MethodOverrideConfig) -> Self {
        Self {
            config: Arc::new(config),
            max_scan_bytes: MAX_BODY_SCAN_BYTES,
        }
    }

    /// Limit the body bytes read when scanning for a `_method` field.
    /// The effective limit is `min(n, MAX_BODY_SCAN_BYTES)`.
    #[must_use]
    pub(crate) fn with_max_scan_bytes(mut self, n: usize) -> Self {
        self.max_scan_bytes = n.min(MAX_BODY_SCAN_BYTES);
        self
    }
}

impl Default for MethodOverrideLayer {
    fn default() -> Self {
        Self::new()
    }
}

impl<S> Layer<S> for MethodOverrideLayer {
    type Service = MethodOverrideService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        MethodOverrideService {
            inner,
            config: Arc::clone(&self.config),
            max_scan_bytes: self.max_scan_bytes,
        }
    }
}

/// Tower [`Service`] produced by [`MethodOverrideLayer`].
#[derive(Clone, Debug)]
pub struct MethodOverrideService<S> {
    inner: S,
    config: Arc<MethodOverrideConfig>,
    max_scan_bytes: usize,
}

/// Returns `Some(method)` when `value` is a recognised override target.
fn parse_override_value(value: &str) -> Option<http::Method> {
    let trimmed = value.trim();
    if trimmed.eq_ignore_ascii_case("PUT") {
        Some(http::Method::PUT)
    } else if trimmed.eq_ignore_ascii_case("PATCH") {
        Some(http::Method::PATCH)
    } else if trimmed.eq_ignore_ascii_case("DELETE") {
        Some(http::Method::DELETE)
    } else {
        None
    }
}

#[derive(Debug)]
enum OverrideOutcome {
    /// No override field present — leave the request alone.
    None,
    /// Override field present and value is a recognised target method.
    Replace(http::Method),
    /// Override field present but value is not a recognised method.
    Invalid,
}

fn scan_form_for_override(bytes: &[u8], field: &str) -> OverrideOutcome {
    let mut outcome = OverrideOutcome::None;
    for (key, value) in url::form_urlencoded::parse(bytes) {
        if key == field {
            outcome = parse_override_value(&value)
                .map_or(OverrideOutcome::Invalid, OverrideOutcome::Replace);
            break;
        }
    }
    outcome
}

fn is_form_urlencoded(headers: &http::HeaderMap) -> bool {
    headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("application/x-www-form-urlencoded"))
}

/// Is the request a same-origin browser form submission?
///
/// The override convention is documented for browser HTML forms, so we
/// only honour it when the request really did originate from the same
/// origin. Without this check a CSRF-disabled (or CSRF-exempt) route
/// declared as `#[delete]` could be reached by a cross-origin form
/// using `_method=DELETE` even though the equivalent direct `DELETE`
/// request would be blocked by the browser's same-origin policy.
///
/// Decision matrix (returns `true` to allow the override):
///
/// 1. `Sec-Fetch-Site: same-origin` or `none` — accept outright.
///    `same-origin` is exactly what we want. `none` is user-initiated
///    with no opener (typed URL, bookmark, redirect chain origin) and
///    is documented as safe by Fetch Metadata Request Headers.
/// 2. `Sec-Fetch-Site: same-site` — **not** sufficient on its own,
///    because `same-site` accepts any origin under the same
///    registrable domain (`evil.example.com` -> `app.example.com`).
///    Fall back to the `Origin` vs `Host` comparison to confirm a
///    true same-origin match before honouring the override.
/// 3. `Sec-Fetch-Site: cross-site` — reject outright.
/// 4. `Sec-Fetch-Site` absent — fall back to comparing the `Origin`
///    header against `Host`. Modern browsers always send `Origin`
///    for `POST`, so a mismatch here means the request really is
///    cross-origin.
/// 5. When both `Sec-Fetch-Site` and `Origin` are absent, deny the
///    override (fail closed). Such requests are either an extremely
///    old browser or a non-browser client; in either case the
///    documented browser-form convention does not apply, so the safe
///    thing is to forward the request as the original `POST` and let
///    route matching reject it.
///
/// Wrapper that consults [`ResolvedClientIdentity`] extensions when available,
/// so that the same-origin check uses the resolver-validated host and scheme
/// rather than trusting raw `X-Forwarded-*` headers unconditionally.
fn is_same_origin_form_request(req: &Request<axum::body::Body>) -> bool {
    let identity = req.extensions().get::<ResolvedClientIdentity>();
    let headers = req.headers();

    let Some(origin) = headers
        .get(http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
    else {
        if let Some(site) = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) {
            return matches!(site, "same-origin" | "none");
        }
        return false;
    };

    if let Some(site) = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) {
        return match site {
            "same-origin" | "none" => true,
            "same-site" => origin_matches_request_with_identity(origin, headers, identity),
            _ => false,
        };
    }

    origin_matches_request_with_identity(origin, headers, identity)
}

/// Same-origin check with optional [`ResolvedClientIdentity`] for host/scheme.
///
/// Uses the resolved identity when available (respects the trusted-proxy
/// policy), otherwise falls back to direct `X-Forwarded-*` header reads for
/// backwards compatibility (e.g. standalone tests without the framework
/// proxy middleware).
fn origin_matches_request_with_identity(
    origin: &str,
    headers: &http::HeaderMap,
    identity: Option<&ResolvedClientIdentity>,
) -> bool {
    let Some((origin_scheme, origin_authority)) = parse_origin(origin) else {
        return false;
    };

    // When a resolved identity is present, use it exclusively — never fall
    // back to raw X-Forwarded-Host, because the resolver has already applied
    // the trusted-proxy policy.  Only when no identity extension exists (e.g.,
    // a standalone test without the proxy middleware) do we read headers
    // directly for backwards compatibility.
    let expected_host: Option<std::borrow::Cow<str>> = identity.map_or_else(
        || {
            headers
                .get("x-forwarded-host")
                .and_then(|v| v.to_str().ok())
                .or_else(|| {
                    headers
                        .get(http::header::HOST)
                        .and_then(|v| v.to_str().ok())
                })
                .map(std::borrow::Cow::Borrowed)
        },
        |id| {
            // Identity stamp present: use resolver-validated host, or fall back
            // to Host header only (never raw X-Forwarded-Host).
            id.host
                .as_deref()
                .or_else(|| {
                    headers
                        .get(http::header::HOST)
                        .and_then(|v| v.to_str().ok())
                })
                .map(std::borrow::Cow::Borrowed)
        },
    );

    let Some(expected_host) = expected_host else {
        return false;
    };
    if !origin_authority.eq_ignore_ascii_case(expected_host.as_ref()) {
        return false;
    }

    // Prefer the resolver-validated scheme; fall back to header reads.
    let resolved_scheme: Option<String> = identity.map_or_else(
        || {
            headers
                .get("x-forwarded-proto")
                .and_then(|v| v.to_str().ok())
                .map(|s| {
                    // Multiple `X-Forwarded-Proto` values chained by intermediaries;
                    // the leftmost (client-facing) is the one that matters.
                    s.split(',').next().unwrap_or(s).trim().to_ascii_lowercase()
                })
        },
        |id| id.scheme.clone(),
    );

    if let Some(scheme) = resolved_scheme {
        return scheme.eq_ignore_ascii_case(origin_scheme);
    }

    // No scheme signal: host:port match is the best we can do.
    true
}

/// Split a serialized `Origin` value into `(scheme, authority)`.
///
/// Returns `None` for malformed values (missing scheme, opaque
/// origins like `null`, or unrecognised schemes). Only `http` and
/// `https` are accepted — the override convention is documented for
/// browser HTML forms only.
fn parse_origin(origin: &str) -> Option<(&str, &str)> {
    let (scheme, rest) = origin.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return None;
    }
    // Defensive: `Origin` is just scheme://authority with no path.
    let authority = rest.split('/').next()?;
    if authority.is_empty() {
        return None;
    }
    Some((scheme, authority))
}

impl<S, ResBody> Service<Request<axum::body::Body>> for MethodOverrideService<S>
where
    S: Service<Request<axum::body::Body>, Response = Response<ResBody>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
    ResBody: From<&'static str> + Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request<axum::body::Body>) -> Self::Future {
        // Only POST form-urlencoded submissions originating from the
        // same origin are eligible. Same-origin form semantics: nothing
        // else is allowed to silently morph methods, because a request
        // that browsers won't make directly (e.g. a cross-origin
        // `<form>` submitting to a `#[delete]` route) must not gain
        // reachability via the override convention.
        if req.method() != http::Method::POST
            || !is_form_urlencoded(req.headers())
            || !is_same_origin_form_request(&req)
        {
            let mut inner = self.inner.clone();
            std::mem::swap(&mut self.inner, &mut inner);
            return Box::pin(async move { inner.call(req).await });
        }

        let config = Arc::clone(&self.config);
        let max_scan_bytes = self.max_scan_bytes;
        let mut inner = self.inner.clone();
        std::mem::swap(&mut self.inner, &mut inner);

        Box::pin(async move {
            // Temporarily take ownership of the body so we can buffer it
            // for parsing without losing the bytes the handler will need.
            //
            // We do NOT short-circuit on `Content-Length > limit` and
            // forward the body untouched — that would let a form with
            // `_method=DELETE` and an oversized body silently fall
            // through to the inner router as a `POST`, demoting the
            // user's intended mutating operation based purely on body
            // size. Instead, we always attempt the scan. `to_bytes`
            // checks the body's `size_hint` first and errors out
            // immediately (no buffering) for bodies advertised larger
            // than the cap, so the cost on the fast-failure path is
            // negligible; we mark the rejection and let the inner
            // filter render `413` so the user is told plainly that
            // the form is too large to support the override.
            let body = std::mem::replace(req.body_mut(), axum::body::Body::empty());
            let Ok(bytes) = axum::body::to_bytes(body, max_scan_bytes).await else {
                // Body exceeded the scan cap. The bytes are gone, so
                // we can't forward a faithful request — and we don't
                // want to silently turn an intended `DELETE` into a
                // `POST` either. Stamp the rejection and pass the
                // request through with an empty body; the inner
                // `method_override_rejection_filter` short-circuits to
                // `413` before any handler is called.
                req.extensions_mut()
                    .insert(MethodOverrideRejection::BodyTooLarge);
                let res = inner.call(req).await?;
                if res.status() == StatusCode::METHOD_NOT_ALLOWED
                    || res.status() == StatusCode::NOT_FOUND
                {
                    use crate::middleware::exception_filter::AutumnErrorInfo;
                    let (parts, _body) = res.into_parts();
                    let mut res = Response::from_parts(
                        parts,
                        ResBody::from("Form body too large for method-override scanning."),
                    );
                    *res.status_mut() = StatusCode::PAYLOAD_TOO_LARGE;
                    res.headers_mut().insert(
                        http::header::CONTENT_TYPE,
                        http::HeaderValue::from_static("text/plain; charset=utf-8"),
                    );
                    res.extensions_mut().insert(AutumnErrorInfo {
                        status: StatusCode::PAYLOAD_TOO_LARGE,
                        message: "Form body too large for method-override scanning.".to_owned(),
                        details: None,
                        problem_type: None,
                        backtrace_string: None,
                    });
                    return Ok(res);
                }
                return Ok(res);
            };

            let outcome = scan_form_for_override(&bytes, &config.field_name);

            // Restore the body verbatim regardless of outcome.
            *req.body_mut() = axum::body::Body::from(bytes);

            match outcome {
                OverrideOutcome::None => inner.call(req).await,
                OverrideOutcome::Replace(method) => {
                    let transport = req.method().clone();
                    *req.method_mut() = method.clone();
                    req.extensions_mut().insert(OverriddenMethod {
                        transport,
                        effective: method,
                    });
                    inner.call(req).await
                }
                OverrideOutcome::Invalid => {
                    // Stamp the rejection and forward as POST. The
                    // inner rejection filter converts this to `400`
                    // from inside the framework's response middleware
                    // stack, so security headers, request IDs, metrics,
                    // and error-page rendering all apply.
                    // If route matching fails and returns 404 or 405, we
                    // intercept and rewrite to 400 Bad Request, keeping
                    // headers/extensions (like RequestId and security headers).
                    req.extensions_mut()
                        .insert(MethodOverrideRejection::InvalidValue);
                    let res = inner.call(req).await?;
                    if res.status() == StatusCode::METHOD_NOT_ALLOWED
                        || res.status() == StatusCode::NOT_FOUND
                    {
                        use crate::middleware::exception_filter::AutumnErrorInfo;
                        let (parts, _body) = res.into_parts();
                        let mut res = Response::from_parts(
                            parts,
                            ResBody::from(
                                "Invalid method override value: must be PUT, PATCH, or DELETE.",
                            ),
                        );
                        *res.status_mut() = StatusCode::BAD_REQUEST;
                        res.headers_mut().insert(
                            http::header::CONTENT_TYPE,
                            http::HeaderValue::from_static("text/plain; charset=utf-8"),
                        );
                        res.extensions_mut().insert(AutumnErrorInfo {
                            status: StatusCode::BAD_REQUEST,
                            message:
                                "Invalid method override value: must be PUT, PATCH, or DELETE."
                                    .to_owned(),
                            details: None,
                            problem_type: None,
                            backtrace_string: None,
                        });
                        return Ok(res);
                    }
                    Ok(res)
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::routing::{delete, get, patch, post, put};
    use tower::ServiceExt;

    /// Build a router and wrap it with [`MethodOverrideLayer`] as a top-level
    /// Service.
    ///
    /// `Router::layer` in axum 0.8 applies middleware **per registered
    /// method handler**, which means a layer added that way cannot rewrite
    /// `POST` into `PUT`/`PATCH`/`DELETE` — the inner `MethodRouter`
    /// returns `405` before the layer ever runs. The override convention
    /// requires running before route matching, so wrap the router as a
    /// `tower::Service`.
    ///
    /// The inner `method_override_rejection_filter` is applied via
    /// `Router::layer` so that `MethodOverrideRejection` extensions
    /// stamped by the outer layer are converted into `400`/`413`
    /// responses inside the per-route middleware chain — mirroring the
    /// production wiring in `router::apply_middleware`.
    fn layered_router() -> MethodOverrideService<Router> {
        let router = Router::new()
            .route("/items", post(|| async { "created" }))
            .route("/items/{id}", put(|| async { "put-ok" }))
            .route("/items/{id}", patch(|| async { "patch-ok" }))
            .route("/items/{id}", delete(|| async { "delete-ok" }))
            .route("/items/{id}", get(|| async { "show" }))
            .layer(axum::middleware::from_fn(method_override_rejection_filter));
        MethodOverrideLayer::new().layer(router)
    }

    #[tokio::test]
    async fn post_without_override_field_reaches_post_handler() {
        let app = layered_router();
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/items")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .header("sec-fetch-site", "same-origin")
                    .body(Body::from("title=hello"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(&body[..], b"created");
    }

    #[tokio::test]
    async fn post_with_method_put_reaches_put_handler() {
        let app = layered_router();
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/items/1")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .header("sec-fetch-site", "same-origin")
                    .body(Body::from("_method=PUT&title=hi"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(&body[..], b"put-ok");
    }

    #[tokio::test]
    async fn post_with_method_patch_reaches_patch_handler() {
        let app = layered_router();
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/items/1")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .header("sec-fetch-site", "same-origin")
                    .body(Body::from("_method=patch"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(&body[..], b"patch-ok");
    }

    #[tokio::test]
    async fn post_with_method_delete_reaches_delete_handler() {
        let app = layered_router();
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/items/1")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .header("sec-fetch-site", "same-origin")
                    .body(Body::from("_method=DELETE"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(&body[..], b"delete-ok");
    }

    #[tokio::test]
    async fn invalid_override_value_returns_400() {
        let app = layered_router();
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/items/1")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .header("sec-fetch-site", "same-origin")
                    .body(Body::from("_method=BREW"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn override_ignored_without_form_content_type() {
        // JSON POST should NOT have its method rewritten even if a `_method`
        // string happens to appear in the body. The convention is documented
        // for browser HTML form submissions only.
        let app = layered_router();
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/items")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"_method":"DELETE","title":"hi"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(&body[..], b"created");
    }

    #[tokio::test]
    async fn override_ignored_for_get_requests() {
        let app = layered_router();
        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/items/1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(&body[..], b"show");
    }

    #[tokio::test]
    async fn override_preserves_body_for_handler() {
        // Override consumes the body to peek at `_method`; it MUST restore
        // the bytes so handler extractors (Form, ChangesetForm, etc.) work.
        async fn echo(body: String) -> String {
            body
        }
        let router = Router::new().route("/echo", put(echo));
        let app = MethodOverrideLayer::new().layer(router);

        let payload = "_method=PUT&title=hello&count=3";
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/echo")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .header("sec-fetch-site", "same-origin")
                    .body(Body::from(payload))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(std::str::from_utf8(&body).unwrap(), payload);
    }

    #[tokio::test]
    async fn override_marks_request_extension() {
        async fn marker(axum::Extension(o): axum::Extension<OverriddenMethod>) -> String {
            format!("{}->{}", o.transport, o.effective)
        }
        let router = Router::new().route("/x", delete(marker));
        let app = MethodOverrideLayer::new().layer(router);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/x")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .header("sec-fetch-site", "same-origin")
                    .body(Body::from("_method=DELETE"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(&body[..], b"POST->DELETE");
    }

    #[tokio::test]
    async fn custom_field_name_is_respected() {
        let router = Router::new().route("/items/{id}", delete(|| async { "gone" }));
        let app = MethodOverrideLayer::from_config(MethodOverrideConfig {
            field_name: "x-method".into(),
        })
        .layer(router);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/items/1")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .header("sec-fetch-site", "same-origin")
                    .body(Body::from("x-method=DELETE"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(&body[..], b"gone");
    }

    /// Acceptance criterion: "CSRF protection still treats the effective
    /// overridden method as unsafe, and tests prove an overridden DELETE
    /// without a valid CSRF token is rejected."
    ///
    /// The CSRF layer wraps method-override on the outside. CSRF sees the
    /// browser's `POST` (already an unsafe method) and demands a token.
    /// A `_method=DELETE` body with no `_csrf` field is rejected with
    /// `403 Forbidden`, exactly like any other mutating POST.
    #[tokio::test]
    async fn overridden_delete_without_csrf_token_is_rejected() {
        let csrf_config = crate::security::CsrfConfig {
            enabled: true,
            ..Default::default()
        };
        let router = Router::new()
            .route("/items/{id}", delete(|| async { "deleted" }))
            .layer(crate::security::CsrfLayer::from_config(&csrf_config));
        let app = MethodOverrideLayer::new().layer(router);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/items/1")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .header("sec-fetch-site", "same-origin")
                    .header(http::header::ACCEPT, "text/html")
                    .header("Cookie", "autumn-csrf=valid-cookie-token")
                    .body(Body::from("_method=DELETE"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    /// Companion: an overridden DELETE with a matching `_csrf` token passes
    /// CSRF and reaches the DELETE handler.
    #[tokio::test]
    async fn overridden_delete_with_csrf_token_reaches_handler() {
        let csrf_config = crate::security::CsrfConfig {
            enabled: true,
            ..Default::default()
        };
        let router = Router::new()
            .route("/items/{id}", delete(|| async { "deleted" }))
            .layer(crate::security::CsrfLayer::from_config(&csrf_config));
        let app = MethodOverrideLayer::new().layer(router);

        let token = "valid-cookie-token";
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/items/1")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .header("sec-fetch-site", "same-origin")
                    .header("Cookie", format!("autumn-csrf={token}"))
                    .body(Body::from(format!("_csrf={token}&_method=DELETE")))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(&body[..], b"deleted");
    }

    /// A form-urlencoded POST whose body exceeds the scan cap is
    /// rejected with `413` even when it carries no `_method` field.
    /// We can't tell whether the body contains an override without
    /// scanning, and forwarding it untouched would let a request with
    /// `_method=DELETE` and an oversized body silently fall through
    /// to the POST handler instead. Failing closed preserves the
    /// user's intended operation semantics.
    #[tokio::test]
    async fn oversized_form_post_without_override_field_is_rejected() {
        async fn measure(body: bytes::Bytes) -> String {
            format!("{}", body.len())
        }
        let router = Router::new()
            .route("/upload", post(measure))
            .layer(axum::extract::DefaultBodyLimit::max(8 * 1024 * 1024))
            .layer(axum::middleware::from_fn(method_override_rejection_filter));
        let app = MethodOverrideLayer::new().layer(router);

        // 3 MiB payload — over MAX_BODY_SCAN_BYTES (2 MiB).
        let big = "x".repeat(3 * 1024 * 1024);
        let payload = format!("title={big}");
        let payload_len = payload.len();

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/upload")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .header("sec-fetch-site", "same-origin")
                    .header("content-length", payload_len.to_string())
                    .body(Body::from(payload))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    /// A POST whose body actually exceeds the scan cap during buffering
    /// (e.g. chunked transfer encoding without an accurate Content-Length)
    /// is stamped with [`MethodOverrideRejection::BodyTooLarge`] and the
    /// inner `method_override_rejection_filter` converts it into an
    /// explicit `413` — inside the per-route layer chain — rather than
    /// the outer layer short-circuiting and bypassing framework
    /// middleware.
    #[tokio::test]
    async fn unbounded_oversized_form_post_returns_413() {
        async fn handler(body: String) -> String {
            body
        }
        let router = Router::new()
            .route("/x", post(handler))
            .layer(axum::middleware::from_fn(method_override_rejection_filter));
        let app = MethodOverrideLayer::new().layer(router);

        // No Content-Length header -> content_length_exceeds returns
        // false and we attempt to buffer. The body itself is larger
        // than the scan cap so `to_bytes` will error.
        let payload = "x".repeat(3 * 1024 * 1024);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/x")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .header("sec-fetch-site", "same-origin")
                    .body(Body::from(payload))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    /// When the outer `MethodOverrideLayer` is composed without the
    /// inner `method_override_rejection_filter`, the rejection
    /// extension is still stamped on the request — but no handler
    /// short-circuits, so the request continues to flow normally.
    /// This matches how the framework wires the two pieces, and
    /// proves that the outer layer doesn't generate `400`/`413`
    /// responses on its own.
    use std::sync::Mutex;
    use std::sync::OnceLock;

    static OBSERVED_REJECTION: OnceLock<Mutex<Option<MethodOverrideRejection>>> = OnceLock::new();

    async fn capture_rejection(
        request: axum::extract::Request,
        next: axum::middleware::Next,
    ) -> axum::response::Response {
        if let Some(rej) = request
            .extensions()
            .get::<MethodOverrideRejection>()
            .copied()
        {
            *OBSERVED_REJECTION
                .get_or_init(|| Mutex::new(None))
                .lock()
                .unwrap() = Some(rej);
        }
        next.run(request).await
    }

    #[tokio::test]
    async fn outer_layer_stamps_extension_without_short_circuiting() {
        let cell = OBSERVED_REJECTION.get_or_init(|| Mutex::new(None));
        cell.lock().unwrap().take();

        let router = Router::new()
            .route("/x", post(|| async { "post-ok" }))
            .layer(axum::middleware::from_fn(capture_rejection));
        let app = MethodOverrideLayer::new().layer(router);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/x")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .header("sec-fetch-site", "same-origin")
                    .body(Body::from("_method=BREW"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            *cell.lock().unwrap(),
            Some(MethodOverrideRejection::InvalidValue)
        );
    }

    /// A POST carrying `_method=DELETE` from a cross-site context must
    /// NOT be honoured as a DELETE. Without this check, a route declared
    /// as `#[delete]` only — which native browser forms can never reach
    /// directly — would become reachable from any third-party site that
    /// renders a form pointing at it (when CSRF is disabled or the path
    /// is CSRF-exempt).
    #[tokio::test]
    async fn cross_site_form_is_not_honoured_as_override() {
        // Inner /items/{id} only has DELETE; no POST handler. If the
        // override were applied, we'd see 200 "delete-ok". If the
        // override is correctly skipped, we should see 405 (POST not
        // allowed for that route).
        let app = layered_router();
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/items/1")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .header("sec-fetch-site", "cross-site")
                    .body(Body::from("_method=DELETE"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            response.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "cross-site override must not be applied; expected the inner \
             router to reject the underlying POST"
        );
    }

    /// When `Sec-Fetch-Site` is absent we fall back to comparing
    /// `Origin` with `Host`. A mismatch means the request is
    /// cross-origin and the override must not apply.
    #[tokio::test]
    async fn origin_host_mismatch_is_not_honoured_as_override() {
        let app = layered_router();
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/items/1")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .header("origin", "https://evil.example")
                    .header("host", "app.example")
                    .body(Body::from("_method=DELETE"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    /// `Origin` and `Host` match (both `app.example`) — same origin via
    /// the fallback path; the override should apply.
    #[tokio::test]
    async fn origin_host_match_is_honoured_as_override() {
        let app = layered_router();
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/items/1")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .header("origin", "https://app.example")
                    .header("host", "app.example")
                    .body(Body::from("_method=DELETE"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    /// Regression test from the second Codex same-origin review:
    /// `Sec-Fetch-Site: same-site` includes sibling subdomains under
    /// the same registrable domain (e.g. `evil.example.com` ->
    /// `app.example.com`). The override must NOT be honoured purely on
    /// the `same-site` signal — without a matching `Origin`/`Host`
    /// pair the request must be forwarded as the original `POST` and
    /// route matching is left to reject it.
    #[tokio::test]
    async fn same_site_sibling_origin_is_not_honoured_as_override() {
        let app = layered_router();
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/items/1")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .header("sec-fetch-site", "same-site")
                    .header("origin", "https://evil.example")
                    .header("host", "app.example")
                    .body(Body::from("_method=DELETE"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            response.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "same-site with mismatched Origin/Host must not be honoured as override"
        );
    }

    /// Neither `Sec-Fetch-Site` nor `Origin` present: fail closed. The
    /// override is not applied — the request flows through as POST.
    #[tokio::test]
    async fn missing_origin_signals_fail_closed() {
        let app = layered_router();
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/items/1")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from("_method=DELETE"))
                    .unwrap(),
            )
            .await
            .unwrap();

        // /items/1 has no POST handler, so a non-overridden POST yields 405.
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    /// Regression: when `X-Forwarded-Proto: https` is set by the reverse
    /// proxy, an `Origin: http://...` carries a different scheme and
    /// therefore is NOT same-origin, even if the host matches. The
    /// override must not be honoured.
    #[tokio::test]
    async fn origin_scheme_mismatch_via_forwarded_proto_is_rejected() {
        let app = layered_router();
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/items/1")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .header("origin", "http://app.example")
                    .header("host", "app.example")
                    .header("x-forwarded-proto", "https")
                    .body(Body::from("_method=DELETE"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            response.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "different Origin scheme is not same-origin"
        );
    }

    /// `X-Forwarded-Proto` may carry a chained list (e.g. through
    /// nested proxies). The leftmost (client-facing) value is what
    /// the browser saw; that's the value we must compare against.
    #[tokio::test]
    async fn origin_scheme_match_via_chained_forwarded_proto() {
        let app = layered_router();
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/items/1")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .header("origin", "https://app.example")
                    .header("host", "app.example")
                    // First hop saw HTTPS; later hops were HTTP between
                    // proxy and app. Only the client-facing scheme
                    // matters for the same-origin comparison.
                    .header("x-forwarded-proto", "https, http")
                    .body(Body::from("_method=DELETE"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    /// When the proxy surfaces a different `X-Forwarded-Host`, that's
    /// the host the browser saw — Origin must match the forwarded host,
    /// not the internal `Host` header from the proxy.
    #[tokio::test]
    async fn forwarded_host_takes_precedence_over_host_header() {
        let app = layered_router();
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/items/1")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .header("origin", "https://app.example")
                    .header("host", "internal.cluster.local")
                    .header("x-forwarded-host", "app.example")
                    .header("x-forwarded-proto", "https")
                    .body(Body::from("_method=DELETE"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    /// `parse_origin` unit cases.
    #[test]
    fn parse_origin_accepts_http_and_https_only() {
        assert_eq!(
            parse_origin("https://app.example"),
            Some(("https", "app.example"))
        );
        assert_eq!(
            parse_origin("http://app.example:8080"),
            Some(("http", "app.example:8080"))
        );
        // Unknown / opaque origins are rejected.
        assert_eq!(parse_origin("null"), None);
        assert_eq!(parse_origin("file:///etc/passwd"), None);
        assert_eq!(parse_origin("javascript:alert(1)"), None);
        // Missing scheme/authority.
        assert_eq!(parse_origin("app.example"), None);
        assert_eq!(parse_origin("https://"), None);
    }

    fn req_from_headers(headers: http::HeaderMap) -> axum::extract::Request<Body> {
        let mut req = axum::http::Request::builder().body(Body::empty()).unwrap();
        *req.headers_mut() = headers;
        req
    }

    /// Helper-level cases that don't require the full router wiring.
    #[test]
    fn is_same_origin_form_decision_matrix() {
        // `same-origin` and `none` are accepted on the Sec-Fetch-Site
        // signal alone.
        for site in ["same-origin", "none"] {
            let mut h = http::HeaderMap::new();
            h.insert("sec-fetch-site", http::HeaderValue::from_str(site).unwrap());
            assert!(
                is_same_origin_form_request(&req_from_headers(h)),
                "sec-fetch-site={site} should be allowed"
            );
        }

        // `same-site` is NOT sufficient alone: it allows sibling
        // origins under the same registrable domain. Require an
        // Origin/Host match before honouring it.
        let mut h = http::HeaderMap::new();
        h.insert(
            "sec-fetch-site",
            http::HeaderValue::from_static("same-site"),
        );
        assert!(
            !is_same_origin_form_request(&req_from_headers(h)),
            "same-site alone must not be accepted"
        );

        // `same-site` plus a matching Origin/Host pair: accepted.
        let mut h = http::HeaderMap::new();
        h.insert(
            "sec-fetch-site",
            http::HeaderValue::from_static("same-site"),
        );
        h.insert(
            http::header::ORIGIN,
            http::HeaderValue::from_static("https://app.example"),
        );
        h.insert(
            http::header::HOST,
            http::HeaderValue::from_static("app.example"),
        );
        assert!(is_same_origin_form_request(&req_from_headers(h)));

        // `same-site` with a sibling Origin: the registrable domain is
        // the same so the browser sends `same-site`, but the Origin
        // differs and we must reject.
        let mut h = http::HeaderMap::new();
        h.insert(
            "sec-fetch-site",
            http::HeaderValue::from_static("same-site"),
        );
        h.insert(
            http::header::ORIGIN,
            http::HeaderValue::from_static("https://evil.example"),
        );
        h.insert(
            http::header::HOST,
            http::HeaderValue::from_static("app.example"),
        );
        assert!(
            !is_same_origin_form_request(&req_from_headers(h)),
            "same-site with mismatched Origin/Host must be rejected"
        );

        let mut h = http::HeaderMap::new();
        h.insert(
            "sec-fetch-site",
            http::HeaderValue::from_static("cross-site"),
        );
        assert!(!is_same_origin_form_request(&req_from_headers(h)));

        // Unknown / spoofed Sec-Fetch-Site value: reject.
        let mut h = http::HeaderMap::new();
        h.insert(
            "sec-fetch-site",
            http::HeaderValue::from_static("undefined"),
        );
        assert!(!is_same_origin_form_request(&req_from_headers(h)));

        // No Sec-Fetch-Site: Origin matches Host.
        let mut h = http::HeaderMap::new();
        h.insert(
            http::header::ORIGIN,
            http::HeaderValue::from_static("https://app.example"),
        );
        h.insert(
            http::header::HOST,
            http::HeaderValue::from_static("app.example"),
        );
        assert!(is_same_origin_form_request(&req_from_headers(h)));

        // No Sec-Fetch-Site: Origin does not match Host.
        let mut h = http::HeaderMap::new();
        h.insert(
            http::header::ORIGIN,
            http::HeaderValue::from_static("https://evil.example"),
        );
        h.insert(
            http::header::HOST,
            http::HeaderValue::from_static("app.example"),
        );
        assert!(!is_same_origin_form_request(&req_from_headers(h)));

        // Neither signal present: fail closed.
        let h = http::HeaderMap::new();
        assert!(!is_same_origin_form_request(&req_from_headers(h)));
    }

    /// Override rejection responses carry an `AutumnErrorInfo` extension
    /// so the framework exception filter chain — problem-details
    /// negotiation, custom HTML error-page rendering — processes them
    /// the same as a handler-generated error response.
    #[tokio::test]
    async fn rejection_response_carries_autumn_error_info() {
        use crate::middleware::exception_filter::AutumnErrorInfo;

        let app = layered_router();
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/items/1")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .header("sec-fetch-site", "same-origin")
                    .body(Body::from("_method=BREW"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let info = response
            .extensions()
            .get::<AutumnErrorInfo>()
            .expect("override rejection must carry AutumnErrorInfo");
        assert_eq!(info.status, StatusCode::BAD_REQUEST);
        assert!(info.message.contains("PUT, PATCH, or DELETE"));
    }

    #[tokio::test]
    async fn with_max_scan_bytes_rejects_body_exceeding_custom_cap() {
        let router = Router::new()
            .route("/items", post(|| async { "ok" }))
            .layer(axum::middleware::from_fn(method_override_rejection_filter));

        // Set a very small cap (10 bytes) — the 20-byte body should be rejected.
        let service =
            tower::Layer::layer(&MethodOverrideLayer::new().with_max_scan_bytes(10), router);

        let response = service
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/items")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .header("sec-fetch-site", "same-origin")
                    .body(Body::from("_method=DELETE&x=123456789012"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    // ── AC #5(d): PR #791 PoC regression — method-override spoofing stays rejected
    // after migration to the centralized proxy resolver.

    /// Regression for PR #791: an attacker who controls `X-Forwarded-Host` and
    /// `X-Forwarded-Proto` must NOT be able to spoof the same-origin check when
    /// those values come from an untrusted source.
    ///
    /// When `ResolvedClientIdentity` is NOT in extensions (resolver not active),
    /// the middleware falls back to direct header reads — the existing tests
    /// above already cover that path and show the spoofing is rejected when the
    /// Origin/Host pair does not match.
    ///
    /// When `ResolvedClientIdentity` IS in extensions (resolver active), the
    /// middleware uses the *resolver-validated* host and scheme, which are
    /// derived from the trusted proxy policy — not from attacker-controlled
    /// headers.
    #[tokio::test]
    async fn pr791_poc_with_resolved_identity_uses_validated_host() {
        use crate::security::trusted_proxies::ResolvedClientIdentity;

        // The app's "real" origin is https://app.example.
        // An attacker injects X-Forwarded-Host: app.example with matching Origin
        // trying to make the middleware see a same-origin match.
        //
        // When the proxy resolver has determined the real host is "app.example"
        // (legitimate — request is really from app.example), the override is allowed.
        let app = layered_router();
        let mut req = Request::builder()
            .method("POST")
            .uri("/items/1")
            .header("content-type", "application/x-www-form-urlencoded")
            .header("origin", "https://app.example")
            .header("host", "internal.cluster.local")
            .header("x-forwarded-host", "app.example")
            .header("x-forwarded-proto", "https")
            .body(Body::from("_method=DELETE"))
            .unwrap();

        // Inject resolved identity as the framework would (host = app.example, scheme = https).
        req.extensions_mut().insert(ResolvedClientIdentity {
            addr: None,
            host: Some("app.example".to_owned()),
            scheme: Some("https".to_owned()),
        });

        let response = app.oneshot(req).await.unwrap();
        // Legitimate same-origin request with resolver-validated identity: allowed.
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn pr791_poc_resolved_identity_different_host_is_rejected() {
        use crate::security::trusted_proxies::ResolvedClientIdentity;

        // The resolver determined the real host is "internal.cluster.local" (not
        // what the attacker put in X-Forwarded-Host). The Origin says "app.example",
        // which doesn't match the resolver-validated host, so the override is rejected.
        let app = layered_router();
        let mut req = Request::builder()
            .method("POST")
            .uri("/items/1")
            .header("content-type", "application/x-www-form-urlencoded")
            .header("origin", "https://app.example")
            .header("host", "internal.cluster.local")
            .header("x-forwarded-host", "app.example")
            .header("x-forwarded-proto", "https")
            .body(Body::from("_method=DELETE"))
            .unwrap();

        // Resolver says the real host is the internal cluster hostname, not the
        // attacker-injected X-Forwarded-Host value.
        req.extensions_mut().insert(ResolvedClientIdentity {
            addr: None,
            host: Some("internal.cluster.local".to_owned()),
            scheme: Some("http".to_owned()),
        });

        let response = app.oneshot(req).await.unwrap();
        // Origin (app.example) does not match resolver-validated host
        // (internal.cluster.local): override must NOT apply.
        assert_eq!(
            response.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "resolver-validated host mismatch must reject the override"
        );
    }
}
