//! Consent-banner widget and the middleware that auto-injects it.
//!
//! [`inject_consent_banner`] is a response-body-splice middleware — it
//! detects an HTML response and, when the visitor's [`super::Consent`] needs
//! (re-)prompting, inserts the banner markup right before `</body>`. This
//! mirrors [`crate::middleware::dev::inject_live_reload`]'s proven
//! detect-HTML / splice-before-`</body>` / fix-`Content-Length` pattern, so
//! every HTML page in the app shows the banner automatically — no per-handler
//! wiring, and no change to the app's shared `layout()` function signature
//! (which `autumn generate scaffold` depends on staying a stable 4-arg call).

use axum::body::{Body, Bytes};
use axum::http::header::{
    CACHE_CONTROL, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, IF_MODIFIED_SINCE,
    IF_NONE_MATCH, SET_COOKIE, VARY,
};
use axum::http::{HeaderName, HeaderValue, Request, Response};
use axum::middleware::Next;
use futures::StreamExt as _;
use maud::PreEscaped;

use super::{Consent, find_cookie};

/// Upper bound on how much of an HTML response body [`inject_consent_banner`]
/// will buffer in order to splice the banner in.
///
/// Unlike the dev-only `crate::middleware::dev::inject_live_reload` this
/// mirrors, this middleware runs unconditionally in production for every
/// undecided visitor — an attacker-triggerable, unauthenticated request path
/// — so buffering must be bounded rather than `usize::MAX`. Matches the CSRF
/// body-scan cap precedent (`security.csrf.token_scan_bytes`, 2 MiB default).
/// A body over this cap is served **unmodified** (see [`collect_body_prefix`])
/// rather than dropped — a large page without the banner beats an empty one.
const MAX_SPLICE_BODY_BYTES: usize = 2 * 1024 * 1024;

/// Bare CSS for [`BANNER_SPACE_RESERVATION_CSS`]'s `<style>` element — reserves
/// room at the bottom of the viewport so the fixed-position banner never
/// permanently hides page content (e.g. a `<footer>`) underneath it. Emitted
/// only alongside the banner itself, so it never affects layout once consent
/// has been decided. Uses an inline `<style>` tag rather than a JS height
/// measurement, matching `crate::error_pages::dev_badge`'s own
/// inline-CSS-only convention; carries the request's CSP nonce (see
/// [`inject_consent_banner`]) so it isn't dropped under
/// `security.headers.csp_nonce.enabled = true`.
const BANNER_SPACE_RESERVATION_CSS: &str = "body{padding-bottom:6.5rem}";

/// Render the consent-banner markup.
///
/// Offers "Reject non-essential" and "Accept all" as two `type="submit"`
/// buttons sharing one CSS class (`autumn-consent-banner__button`) so neither
/// is visually more prominent than the other — rejecting must be as easy as
/// accepting. Both buttons live in a single `<form>` (default action
/// `/consent/accept`; the reject button overrides via `formaction`) so only
/// one CSRF hidden field is needed. Needs no JavaScript: plain HTML forms,
/// native keyboard/tab reachability, `role="region"` + `aria-label` for
/// screen readers.
///
/// `csrf_token` should be the value of the app's CSRF cookie (whatever name
/// it's configured under — see `security.csrf.cookie_name`); pass `None`
/// only when CSRF protection is disabled entirely. `csrf_field_name` is the
/// form field the token is submitted under — `"_csrf"` unless the app
/// customized `security.csrf.form_field`. `nonce` is the request's CSP nonce
/// (see `security.headers.csp_nonce`) applied to the reservation `<style>`;
/// pass `None` when CSP nonces are disabled (the default).
#[must_use]
pub fn consent_banner_markup(
    csrf_token: Option<&str>,
    csrf_field_name: &str,
    nonce: Option<&str>,
) -> maud::Markup {
    maud::html! {
        style nonce=[nonce] { (PreEscaped(BANNER_SPACE_RESERVATION_CSS)) }
        section class="autumn-consent-banner" role="region" aria-label="Cookie consent" {
            p class="autumn-consent-banner__message" {
                "This site uses cookies. Strictly-necessary cookies (login, security) are always on. "
                "Others, like analytics, only run if you accept them."
            }
            form method="post" action="/consent/accept" class="autumn-consent-banner__actions" {
                @if let Some(token) = csrf_token {
                    input type="hidden" name=(csrf_field_name) value=(token);
                }
                button
                    type="submit"
                    formaction="/consent/reject"
                    class="autumn-consent-banner__button autumn-consent-banner__button--reject"
                {
                    "Reject non-essential"
                }
                button
                    type="submit"
                    class="autumn-consent-banner__button autumn-consent-banner__button--accept"
                {
                    "Accept all"
                }
            }
        }
    }
}

/// Middleware: inject the consent banner into every HTML response for a
/// visitor who needs (re-)prompting (see [`super::Consent::needs_prompt`]).
///
/// Register it with the app's configured policy version and CSRF cookie
/// name (`autumn-csrf` unless `security.csrf.cookie_name` was customized —
/// see [`super::DEFAULT_CSRF_COOKIE_NAME`]), e.g.:
///
/// ```rust,ignore
/// const CONSENT_POLICY_VERSION: u32 = 1;
///
/// let app = autumn_web::app()
///     .routes(routes![index])
///     .layer(axum::middleware::from_fn(move |req, next| async move {
///         autumn_web::consent::inject_consent_banner(
///             req,
///             next,
///             CONSENT_POLICY_VERSION,
///             autumn_web::consent::DEFAULT_CSRF_COOKIE_NAME,
///         ).await
///     }));
/// ```
///
/// Reads the visitor's consent cookie and the CSRF cookie directly off the
/// incoming request's `Cookie` header before calling `next`, so it has no
/// dependency on where `CsrfLayer` sits in the layer stack. Non-HTML and
/// `Content-Encoding`-bearing responses (compressed bodies) pass through
/// untouched, exactly like the dev-mode live-reload injector.
///
/// Also, while a visitor needs (re-)prompting: strips `If-None-Match` /
/// `If-Modified-Since` from the request before calling `next`, so an inner
/// `EtagLayer` can't short-circuit to a bodyless `304` — which would replay a
/// browser-cached, banner-less page and silently skip the required prompt
/// after a policy bump or a withdrawn consent. And reads the CSRF form-field
/// name (`security.csrf.form_field`, via `CsrfFormField` in request
/// extensions) and the CSP nonce (`security.headers.csp_nonce`, via
/// `CspNonce` in request extensions, or parsed back out of the response's
/// `Content-Security-Policy` header if `CspNonce` wasn't already in request
/// extensions when this ran) so the injected banner and its style stay valid
/// under those configurations too.
///
/// Whenever it actually injects the banner (and therefore a live, per-visitor
/// CSRF token) into the body, it also stamps the response
/// `Cache-Control: private, no-store` and `Vary: Cookie` — otherwise a page
/// the app marked publicly cacheable (e.g. via `cache_for(..).public()`)
/// could have one visitor's CSRF token served to every other visitor (and
/// any CDN/proxy in between) until the cache entry expires.
pub async fn inject_consent_banner(
    mut request: Request<Body>,
    next: Next,
    policy_version: u32,
    csrf_cookie_name: &str,
) -> Response<Body> {
    let consent = Consent::from_headers(request.headers());
    let request_csrf_cookie = find_cookie(request.headers(), csrf_cookie_name);
    let csrf_field_name = request
        .extensions()
        .get::<crate::security::CsrfFormField>()
        .map_or_else(|| "_csrf".to_owned(), |field| field.0.clone());
    let request_nonce = request
        .extensions()
        .get::<crate::security::CspNonce>()
        .map(|nonce| nonce.value().to_owned());

    let needs_prompt = consent.needs_prompt(policy_version);
    if needs_prompt {
        request.headers_mut().remove(IF_NONE_MATCH);
        request.headers_mut().remove(IF_MODIFIED_SINCE);
    }

    let response = next.run(request).await;

    if !needs_prompt || !is_html_response(&response) {
        return response;
    }

    let csrf_token =
        extract_response_csrf_cookie(&response, csrf_cookie_name).or(request_csrf_cookie);
    let nonce = request_nonce.or_else(|| extract_response_csp_nonce(&response));
    let banner_html =
        consent_banner_markup(csrf_token.as_deref(), &csrf_field_name, nonce.as_deref())
            .into_string();
    splice_into_response(response, &banner_html).await
}

/// Recover the per-request CSP nonce from the response's
/// `Content-Security-Policy` header, for when `CspNonce` wasn't yet in
/// request extensions (i.e. `SecurityHeadersLayer` sits inner to this
/// middleware rather than outer). Mirrors the same `'nonce-...'` extraction
/// the framework's own tests use.
fn extract_response_csp_nonce(response: &Response<Body>) -> Option<String> {
    response
        .headers()
        .get(HeaderName::from_static("content-security-policy"))
        .and_then(|value| value.to_str().ok())
        .and_then(|csp| csp.split("'nonce-").nth(1))
        .and_then(|rest| rest.split('\'').next())
        .map(str::to_owned)
}

/// Find a fresh CSRF cookie value the wrapped handler's response is about to
/// set (e.g. the very first request from a visitor, before any CSRF cookie
/// existed on the way in).
fn extract_response_csrf_cookie(
    response: &Response<Body>,
    csrf_cookie_name: &str,
) -> Option<String> {
    response
        .headers()
        .get_all(SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find_map(|set_cookie| {
            let rest = set_cookie
                .strip_prefix(csrf_cookie_name)?
                .strip_prefix('=')?;
            let value = rest.split(';').next().unwrap_or(rest);
            Some(value.to_owned())
        })
}

fn is_html_response(response: &Response<Body>) -> bool {
    if response.headers().contains_key(CONTENT_ENCODING) {
        return false;
    }
    response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|content_type| content_type.contains("text/html"))
}

/// Outcome of bounding how much of a body [`collect_body_prefix`] buffers.
/// Mirrors `security::csrf`'s own `CollectedBody` shape (the same problem —
/// bound memory use without dropping an oversized body — solved once there
/// for a request body, adapted here for a response body).
enum CollectedBody {
    /// The whole body fit within the cap and is fully buffered.
    Full(Bytes),
    /// The body exceeded the cap. Replays the complete, unmodified body — the
    /// buffered prefix plus the over-limit chunk and the remaining stream —
    /// so the client receives every byte with the tail streamed rather than
    /// buffered. This middleware does not attempt to splice into an oversized
    /// body (finding `</body>` would require buffering arbitrarily more), so
    /// unlike the CSRF version there is no separate scan-prefix to return.
    Oversized(Body),
    /// The body stream errored before EOF. The bytes read so far are
    /// discarded.
    Errored,
}

/// Buffer `body` up to `limit` bytes without failing when the body is larger,
/// reconstructing the full body for pass-through in that case. See
/// `security::csrf::collect_body_prefix` for the request-body sibling this
/// mirrors.
async fn collect_body_prefix(body: Body, limit: usize) -> CollectedBody {
    let mut buf = Vec::<u8>::new();
    let mut stream = body.into_data_stream();
    loop {
        match stream.next().await {
            None => break,
            Some(Err(_)) => return CollectedBody::Errored,
            Some(Ok(chunk)) => {
                let remaining = limit.saturating_sub(buf.len());
                if chunk.len() > remaining {
                    let mut leading = Vec::with_capacity(2);
                    if !buf.is_empty() {
                        leading.push(Ok::<Bytes, axum::Error>(Bytes::from(buf)));
                    }
                    leading.push(Ok::<Bytes, axum::Error>(chunk));
                    let body = Body::from_stream(futures::stream::iter(leading).chain(stream));
                    return CollectedBody::Oversized(body);
                }
                buf.extend_from_slice(&chunk);
            }
        }
    }
    CollectedBody::Full(Bytes::from(buf))
}

async fn splice_into_response(response: Response<Body>, snippet: &str) -> Response<Body> {
    let (mut parts, body) = response.into_parts();
    match collect_body_prefix(body, MAX_SPLICE_BODY_BYTES).await {
        CollectedBody::Full(bytes) => {
            let updated = splice_before_body_close(&bytes, snippet);
            if updated == bytes.as_ref() {
                return Response::from_parts(parts, Body::from(bytes));
            }

            parts
                .headers
                .insert(CONTENT_LENGTH, HeaderValue::from(updated.len()));
            // A live, per-visitor CSRF token was just embedded in this body —
            // never let a CDN/proxy (or the browser) cache and replay it to
            // someone else.
            parts
                .headers
                .insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
            parts
                .headers
                .append(VARY, HeaderValue::from_static("Cookie"));
            Response::from_parts(parts, Body::from(updated))
        }
        // Too large to safely buffer and splice into (MAX_SPLICE_BODY_BYTES).
        // Serve the page unmodified — no banner this one time — rather than
        // dropping it: a large report/streamed page without the banner is far
        // better than an empty page. The bytes are unchanged, so any existing
        // Content-Length stays correct and no cache-control change is needed
        // (nothing per-visitor was embedded).
        CollectedBody::Oversized(body) => Response::from_parts(parts, body),
        // The body stream errored before EOF; nothing to reconstruct.
        CollectedBody::Errored => {
            parts.headers.remove(CONTENT_LENGTH);
            Response::from_parts(parts, Body::empty())
        }
    }
}

/// Insert `snippet` just before the last `</body>` tag, or append it if the
/// document has an `<html>`/`</html>` shell but no `</body>`. Leaves `body`
/// unchanged if neither is present (mirrors
/// `crate::middleware::dev::inject_snippet`).
fn splice_before_body_close(body: &[u8], snippet: &str) -> Vec<u8> {
    let html = String::from_utf8_lossy(body);

    if let Some(index) = html.rfind("</body>") {
        let mut html = html.into_owned();
        html.insert_str(index, snippet);
        return html.into_bytes();
    }

    if html.contains("<html") || html.contains("</html>") {
        let mut html = html.into_owned();
        html.push_str(snippet);
        return html.into_bytes();
    }

    body.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::http::StatusCode;
    use axum::routing::get;
    use tower::ServiceExt;

    // ── consent_banner_markup ────────────────────────────────────────

    #[test]
    fn banner_has_accessible_region_and_label() {
        let html = consent_banner_markup(None, "_csrf", None).into_string();
        assert!(html.contains(r#"role="region""#));
        assert!(html.contains(r#"aria-label="Cookie consent""#));
    }

    #[test]
    fn banner_reject_and_accept_share_the_same_base_button_class() {
        let html = consent_banner_markup(None, "_csrf", None).into_string();
        // Reject-as-easy-as-accept: both are `type="submit"` sharing the base
        // `autumn-consent-banner__button` class — neither gets a differently
        // weighted/emphasized class the other lacks.
        assert!(html.contains("Reject non-essential"));
        assert!(html.contains("Accept all"));
        let button_count = html.matches("autumn-consent-banner__button\"").count()
            + html.matches("autumn-consent-banner__button ").count();
        assert!(
            button_count >= 2,
            "both buttons must carry the shared base class: {html}"
        );
    }

    #[test]
    fn banner_omits_csrf_field_when_no_token_given() {
        let html = consent_banner_markup(None, "_csrf", None).into_string();
        assert!(!html.contains("_csrf"));
    }

    #[test]
    fn banner_includes_csrf_hidden_field_when_token_given() {
        let html = consent_banner_markup(Some("tok-123"), "_csrf", None).into_string();
        assert!(html.contains(r#"name="_csrf""#));
        assert!(html.contains(r#"value="tok-123""#));
    }

    #[test]
    fn banner_needs_no_script_tag() {
        let html = consent_banner_markup(Some("tok"), "_csrf", None).into_string();
        assert!(!html.contains("<script"), "banner must need no JS: {html}");
    }

    #[test]
    fn banner_buttons_are_keyboard_reachable_native_submit_buttons() {
        let html = consent_banner_markup(None, "_csrf", None).into_string();
        assert!(html.contains(r#"type="submit""#));
        assert!(!html.contains("tabindex=\"-1\""));
    }

    // ── splice_before_body_close ──────────────────────────────────────

    #[test]
    fn splice_inserts_before_last_body_close_tag() {
        let out = splice_before_body_close(b"<html><body><main>ok</main></body></html>", "<snip>");
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("<snip></body>"));
    }

    #[test]
    fn splice_appends_when_no_body_tag_but_html_shell_present() {
        let out = splice_before_body_close(b"<html><main>ok</main></html>", "<snip>");
        let s = String::from_utf8(out).unwrap();
        assert!(s.ends_with("<snip>"));
    }

    #[test]
    fn splice_leaves_non_html_untouched() {
        let out = splice_before_body_close(b"not html at all", "<snip>");
        assert_eq!(out, b"not html at all");
    }

    // ── is_html_response ────────────────────────────────────────────

    #[test]
    fn html_response_detected_by_content_type() {
        let response = Response::builder()
            .header(CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Body::empty())
            .unwrap();
        assert!(is_html_response(&response));
    }

    #[test]
    fn json_response_is_not_html() {
        let response = Response::builder()
            .header(CONTENT_TYPE, "application/json")
            .body(Body::empty())
            .unwrap();
        assert!(!is_html_response(&response));
    }

    #[test]
    fn encoded_html_response_is_skipped() {
        let response = Response::builder()
            .header(CONTENT_TYPE, "text/html")
            .header(CONTENT_ENCODING, "gzip")
            .body(Body::empty())
            .unwrap();
        assert!(!is_html_response(&response));
    }

    // ── extract_response_csrf_cookie ─────────────────────────────────

    #[test]
    fn extracts_csrf_value_from_fresh_set_cookie() {
        let response = Response::builder()
            .header(SET_COOKIE, "autumn-csrf=fresh-token; Path=/; HttpOnly")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            extract_response_csrf_cookie(&response, "autumn-csrf"),
            Some("fresh-token".to_owned())
        );
    }

    #[test]
    fn no_csrf_set_cookie_yields_none() {
        let response = Response::builder()
            .header(SET_COOKIE, "autumn.sid=abc; Path=/")
            .body(Body::empty())
            .unwrap();
        assert_eq!(extract_response_csrf_cookie(&response, "autumn-csrf"), None);
    }

    #[test]
    fn extract_response_csrf_cookie_honors_custom_cookie_name() {
        let response = Response::builder()
            .header(SET_COOKIE, "my-csrf=custom-token; Path=/")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            extract_response_csrf_cookie(&response, "my-csrf"),
            Some("custom-token".to_owned())
        );
        // Must not match under the default name once a custom name is configured.
        assert_eq!(extract_response_csrf_cookie(&response, "autumn-csrf"), None);
    }

    // ── inject_consent_banner (full middleware, via a tiny router) ────

    fn html_page() -> Response<Body> {
        Response::builder()
            .header(CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Body::from(
                "<html><body><main>hello</main></body></html>".to_owned(),
            ))
            .unwrap()
    }

    fn app_with_policy_version(version: u32) -> Router {
        Router::new()
            .route("/", get(|| async { html_page() }))
            .layer(axum::middleware::from_fn(move |req, next| async move {
                inject_consent_banner(req, next, version, "autumn-csrf").await
            }))
    }

    // End-to-end proof for two acceptance criteria at once: (1) the gate is
    // real enforcement, not just UI — with no consent recorded, a
    // hypothetical "analytics" cookie a handler would otherwise set is
    // actually withheld; (2) strictly-necessary cookies (here, the real
    // session cookie via `SessionLayer`) are completely unaffected by
    // consent state — they are set on every request regardless.
    #[tokio::test]
    async fn gate_withholds_non_essential_cookie_while_session_cookie_is_unaffected() {
        use crate::session::{MemoryStore, Session, SessionConfig, SessionLayer};

        const POLICY_VERSION: u32 = 1;

        async fn handler(session: Session, consent: Consent) -> Response<Body> {
            session.insert("visited", "true").await;
            let mut response = html_page();
            if consent.allows("analytics", POLICY_VERSION) {
                response
                    .headers_mut()
                    .append(SET_COOKIE, HeaderValue::from_static("analytics=on; Path=/"));
            }
            response
        }

        let app = || {
            Router::new()
                .route("/", get(handler))
                .layer(axum::middleware::from_fn(move |req, next| async move {
                    inject_consent_banner(req, next, POLICY_VERSION, "autumn-csrf").await
                }))
                .layer(SessionLayer::new(
                    MemoryStore::new(),
                    SessionConfig::default(),
                ))
        };

        // First visit: no consent recorded at all.
        let response = app()
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let set_cookies: Vec<String> = response
            .headers()
            .get_all(SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok().map(str::to_owned))
            .collect();
        assert!(
            set_cookies.iter().any(|c| c.starts_with("autumn.sid=")),
            "strictly-necessary session cookie must still be set: {set_cookies:?}"
        );
        assert!(
            !set_cookies.iter().any(|c| c.starts_with("analytics=")),
            "non-essential cookie must NOT be set without consent: {set_cookies:?}"
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("autumn-consent-banner"), "{html}");

        // Second visit: consent accepted for "analytics" under the current version.
        let cookie = super::super::accept_all_cookie(&["analytics"], POLICY_VERSION);
        let raw_value = cookie
            .split(';')
            .next()
            .unwrap()
            .strip_prefix("autumn.consent=")
            .unwrap();
        let response = app()
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header("cookie", format!("autumn.consent={raw_value}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let set_cookies: Vec<String> = response
            .headers()
            .get_all(SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok().map(str::to_owned))
            .collect();
        assert!(
            set_cookies.iter().any(|c| c.starts_with("analytics=")),
            "analytics cookie must be set once accepted: {set_cookies:?}"
        );
    }

    #[tokio::test]
    async fn injects_banner_when_no_consent_cookie_present() {
        let app = app_with_policy_version(1);
        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("autumn-consent-banner"), "{html}");
    }

    #[tokio::test]
    async fn omits_banner_when_consent_already_decided_under_current_version() {
        let app = app_with_policy_version(1);
        let cookie = super::super::accept_all_cookie(&["analytics"], 1);
        let raw_value = cookie
            .split(';')
            .next()
            .unwrap()
            .strip_prefix("autumn.consent=")
            .unwrap();

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header("cookie", format!("autumn.consent={raw_value}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(!html.contains("autumn-consent-banner"), "{html}");
    }

    #[tokio::test]
    async fn reprompts_when_recorded_consent_is_from_an_older_policy_version() {
        let app = app_with_policy_version(2);
        let cookie = super::super::accept_all_cookie(&["analytics"], 1);
        let raw_value = cookie
            .split(';')
            .next()
            .unwrap()
            .strip_prefix("autumn.consent=")
            .unwrap();

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header("cookie", format!("autumn.consent={raw_value}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            html.contains("autumn-consent-banner"),
            "policy bump must re-show the banner: {html}"
        );
    }

    #[tokio::test]
    async fn non_html_response_is_left_untouched() {
        let app = Router::new()
            .route(
                "/api",
                get(|| async { ([(CONTENT_TYPE, "application/json")], r#"{"status":"ok"}"#) }),
            )
            .layer(axum::middleware::from_fn(move |req, next| async move {
                inject_consent_banner(req, next, 1, "autumn-csrf").await
            }));
        let response = app
            .oneshot(Request::builder().uri("/api").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(&body[..], br#"{"status":"ok"}"#);
    }

    #[tokio::test]
    async fn banner_carries_csrf_token_freshly_set_by_a_csrf_layer_style_response() {
        let app = Router::new()
            .route(
                "/",
                get(|| async {
                    let mut response = html_page();
                    response.headers_mut().insert(
                        SET_COOKIE,
                        HeaderValue::from_static("autumn-csrf=minted-token; Path=/"),
                    );
                    response
                }),
            )
            .layer(axum::middleware::from_fn(move |req, next| async move {
                inject_consent_banner(req, next, 1, "autumn-csrf").await
            }));
        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains(r#"value="minted-token""#), "{html}");
    }

    #[tokio::test]
    async fn content_length_updated_after_injection() {
        let app = app_with_policy_version(1);
        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let content_length: usize = response
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok())
            .expect("content-length must be set after injection");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(content_length, body.len());
    }

    #[tokio::test]
    async fn injection_marks_response_uncacheable_and_varying_on_cookie() {
        let app = app_with_policy_version(1);
        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            response
                .headers()
                .get(CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some("private, no-store"),
            "a live per-visitor CSRF token was just embedded; this response must never be shared via a cache"
        );
        assert_eq!(
            response.headers().get(VARY).and_then(|v| v.to_str().ok()),
            Some("Cookie")
        );
    }

    #[tokio::test]
    async fn no_cache_headers_added_when_consent_already_decided() {
        let app = app_with_policy_version(1);
        let cookie = super::super::accept_all_cookie(&["analytics"], 1);
        let raw_value = cookie
            .split(';')
            .next()
            .unwrap()
            .strip_prefix("autumn.consent=")
            .unwrap();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header("cookie", format!("autumn.consent={raw_value}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            response.headers().get(CACHE_CONTROL).is_none(),
            "no banner injected, so this middleware must not touch caching headers"
        );
    }

    #[tokio::test]
    async fn oversized_body_is_bounded_in_memory_but_served_intact_unmodified() {
        // A body far larger than MAX_SPLICE_BODY_BYTES must never be fully
        // buffered into memory (bounding the splice buffer matters: this
        // middleware runs unconditionally in production for every undecided,
        // unauthenticated visitor). But the visitor must still receive the
        // real, complete page — a large report/streamed page without the
        // banner is far better than an empty one — so the response body must
        // come through byte-for-byte intact, not truncated or dropped.
        let oversized =
            "<html><body>".to_owned() + &"x".repeat(MAX_SPLICE_BODY_BYTES + 1) + "</body></html>";
        let expected_len = oversized.len();
        let app = Router::new()
            .route(
                "/",
                get(move || {
                    let oversized = oversized.clone();
                    async move {
                        Response::builder()
                            .header(CONTENT_TYPE, "text/html; charset=utf-8")
                            .body(Body::from(oversized))
                            .unwrap()
                    }
                }),
            )
            .layer(axum::middleware::from_fn(move |req, next| async move {
                inject_consent_banner(req, next, 1, "autumn-csrf").await
            }));
        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            body.len(),
            expected_len,
            "an oversized body must be served intact, not truncated or emptied"
        );
        assert!(
            !String::from_utf8_lossy(&body).contains("autumn-consent-banner"),
            "an oversized body is served as-is without the banner spliced in \
             (splicing would require buffering arbitrarily more)"
        );
    }

    #[tokio::test]
    async fn honors_custom_csrf_cookie_name_end_to_end() {
        let app = Router::new()
            .route(
                "/",
                get(|| async {
                    let mut response = html_page();
                    response.headers_mut().insert(
                        SET_COOKIE,
                        HeaderValue::from_static("my-csrf=minted-under-custom-name; Path=/"),
                    );
                    response
                }),
            )
            .layer(axum::middleware::from_fn(move |req, next| async move {
                inject_consent_banner(req, next, 1, "my-csrf").await
            }));
        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            html.contains(r#"value="minted-under-custom-name""#),
            "{html}"
        );
    }

    #[test]
    fn banner_reserves_space_for_fixed_positioning_so_it_cannot_permanently_hide_content() {
        let html = consent_banner_markup(None, "_csrf", None).into_string();
        assert!(
            html.contains("padding-bottom"),
            "banner must reserve viewport space so a fixed-position banner \
             doesn't permanently cover page content like a footer: {html}"
        );
    }

    // ── configured CSRF form-field name ──────────────────────────────

    #[test]
    fn banner_uses_default_csrf_field_name() {
        let html = consent_banner_markup(Some("tok"), "_csrf", None).into_string();
        assert!(html.contains(r#"name="_csrf""#));
    }

    #[test]
    fn banner_honors_custom_csrf_field_name() {
        let html = consent_banner_markup(Some("tok"), "authenticity_token", None).into_string();
        assert!(html.contains(r#"name="authenticity_token""#));
        assert!(!html.contains(r#"name="_csrf""#));
    }

    #[tokio::test]
    async fn banner_honors_configured_csrf_form_field_name_from_request_extensions() {
        let app = Router::new()
            .route("/", get(|| async { html_page() }))
            .layer(axum::middleware::from_fn(move |req, next| async move {
                inject_consent_banner(req, next, 1, "autumn-csrf").await
            }));

        let mut request = Request::builder()
            .uri("/")
            .header("cookie", "autumn-csrf=tok-abc")
            .body(Body::empty())
            .unwrap();
        request
            .extensions_mut()
            .insert(crate::security::CsrfFormField(
                "authenticity_token".to_owned(),
            ));

        let response = app.oneshot(request).await.unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            html.contains(r#"name="authenticity_token" value="tok-abc""#),
            "{html}"
        );
        assert!(!html.contains(r#"name="_csrf""#), "{html}");
    }

    // ── CSP nonce ─────────────────────────────────────────────────────

    #[test]
    fn extract_response_csp_nonce_parses_nonce_from_header() {
        let response = Response::builder()
            .header(
                "content-security-policy",
                "script-src 'self' 'nonce-abc123'; style-src 'self' 'nonce-abc123'",
            )
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            extract_response_csp_nonce(&response),
            Some("abc123".to_owned())
        );
    }

    #[test]
    fn extract_response_csp_nonce_none_when_header_absent() {
        let response = Response::builder().body(Body::empty()).unwrap();
        assert_eq!(extract_response_csp_nonce(&response), None);
    }

    #[test]
    fn banner_style_omits_nonce_attribute_when_none() {
        let html = consent_banner_markup(None, "_csrf", None).into_string();
        assert!(!html.contains("nonce="), "{html}");
    }

    #[test]
    fn banner_style_carries_nonce_when_given() {
        let html = consent_banner_markup(None, "_csrf", Some("abc123")).into_string();
        assert!(html.contains(r#"<style nonce="abc123">"#), "{html}");
    }

    #[tokio::test]
    async fn banner_nonce_matches_a_real_security_headers_layer_via_request_extensions() {
        // My middleware runs INNER to `SecurityHeadersLayer` here (it's applied
        // last, so it wraps outermost), meaning by the time my middleware
        // reads request extensions, `SecurityHeadersLayer` has already
        // inserted `CspNonce` on the way in — the primary (request-extension)
        // resolution path.
        use crate::security::{CspNonceConfig, HeadersConfig, SecurityHeadersLayer};

        let headers_config = HeadersConfig {
            csp_nonce: CspNonceConfig { enabled: true },
            ..HeadersConfig::default()
        };

        let app = Router::new()
            .route("/", get(|| async { html_page() }))
            .layer(axum::middleware::from_fn(move |req, next| async move {
                inject_consent_banner(req, next, 1, "autumn-csrf").await
            }))
            .layer(SecurityHeadersLayer::from_config(&headers_config));

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        let csp = response
            .headers()
            .get("content-security-policy")
            .and_then(|v| v.to_str().ok())
            .expect("CSP header must be set")
            .to_owned();
        let nonce = csp
            .split("'nonce-")
            .nth(1)
            .and_then(|rest| rest.split('\'').next())
            .expect("CSP header must advertise a nonce")
            .to_owned();

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            html.contains(&format!(r#"nonce="{nonce}""#)),
            "banner's <style> must carry the same nonce as the response's CSP header: {html}"
        );
    }

    #[tokio::test]
    async fn banner_nonce_falls_back_to_response_header_when_layer_runs_after() {
        // Here `SecurityHeadersLayer` is applied FIRST (innermost), so my
        // middleware — outermost — sees no `CspNonce` in request extensions
        // yet when it runs; it must fall back to parsing the nonce back out
        // of the response's `Content-Security-Policy` header.
        use crate::security::{CspNonceConfig, HeadersConfig, SecurityHeadersLayer};

        let headers_config = HeadersConfig {
            csp_nonce: CspNonceConfig { enabled: true },
            ..HeadersConfig::default()
        };

        let app = Router::new()
            .route("/", get(|| async { html_page() }))
            .layer(SecurityHeadersLayer::from_config(&headers_config))
            .layer(axum::middleware::from_fn(move |req, next| async move {
                inject_consent_banner(req, next, 1, "autumn-csrf").await
            }));

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        let csp = response
            .headers()
            .get("content-security-policy")
            .and_then(|v| v.to_str().ok())
            .expect("CSP header must be set")
            .to_owned();
        let nonce = csp
            .split("'nonce-")
            .nth(1)
            .and_then(|rest| rest.split('\'').next())
            .expect("CSP header must advertise a nonce")
            .to_owned();

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            html.contains(&format!(r#"nonce="{nonce}""#)),
            "banner's <style> must carry the nonce recovered from the CSP header: {html}"
        );
    }

    // ── conditional-request headers stripped while prompting ─────────

    #[tokio::test]
    async fn strips_conditional_request_headers_while_prompting_so_etag_cannot_shortcut_to_304() {
        let app = Router::new()
            .route(
                "/",
                get(|headers: axum::http::HeaderMap| async move {
                    let has_inm = headers.contains_key(axum::http::header::IF_NONE_MATCH);
                    let has_ims = headers.contains_key(axum::http::header::IF_MODIFIED_SINCE);
                    Response::builder()
                        .header(CONTENT_TYPE, "text/html; charset=utf-8")
                        .body(Body::from(format!(
                            "<html><body>inm={has_inm} ims={has_ims}</body></html>"
                        )))
                        .unwrap()
                }),
            )
            .layer(axum::middleware::from_fn(move |req, next| async move {
                inject_consent_banner(req, next, 1, "autumn-csrf").await
            }));

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header(axum::http::header::IF_NONE_MATCH, "\"abc\"")
                    .header(
                        axum::http::header::IF_MODIFIED_SINCE,
                        "Wed, 21 Oct 2015 07:28:00 GMT",
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("inm=false"), "{text}");
        assert!(text.contains("ims=false"), "{text}");
    }

    #[tokio::test]
    async fn preserves_conditional_request_headers_when_consent_already_decided() {
        let app = Router::new()
            .route(
                "/",
                get(|headers: axum::http::HeaderMap| async move {
                    let has_inm = headers.contains_key(axum::http::header::IF_NONE_MATCH);
                    Response::builder()
                        .header(CONTENT_TYPE, "text/html; charset=utf-8")
                        .body(Body::from(format!(
                            "<html><body>inm={has_inm}</body></html>"
                        )))
                        .unwrap()
                }),
            )
            .layer(axum::middleware::from_fn(move |req, next| async move {
                inject_consent_banner(req, next, 1, "autumn-csrf").await
            }));

        let cookie = super::super::accept_all_cookie(&["analytics"], 1);
        let raw_value = cookie
            .split(';')
            .next()
            .unwrap()
            .strip_prefix("autumn.consent=")
            .unwrap();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header("cookie", format!("autumn.consent={raw_value}"))
                    .header(axum::http::header::IF_NONE_MATCH, "\"abc\"")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            text.contains("inm=true"),
            "no need to force-bust caching when the banner won't show anyway: {text}"
        );
    }
}
