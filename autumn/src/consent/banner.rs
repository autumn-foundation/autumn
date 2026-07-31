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

use axum::body::Body;
use axum::http::header::{
    CACHE_CONTROL, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, SET_COOKIE, VARY,
};
use axum::http::{HeaderValue, Request, Response};
use axum::middleware::Next;
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
const MAX_SPLICE_BODY_BYTES: usize = 2 * 1024 * 1024;

/// Inline style reserving room at the bottom of the viewport so the
/// fixed-position banner never permanently hides page content (e.g. a
/// `<footer>`) underneath it. Emitted only alongside the banner itself, so it
/// never affects layout once consent has been decided. Uses an inline
/// `<style>` tag rather than a JS height measurement, matching
/// `crate::error_pages::dev_badge`'s own inline-CSS-only convention.
const BANNER_SPACE_RESERVATION_CSS: &str = "<style>body{padding-bottom:6.5rem}</style>";

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
/// only when CSRF protection is disabled entirely.
#[must_use]
pub fn consent_banner_markup(csrf_token: Option<&str>) -> maud::Markup {
    maud::html! {
        (PreEscaped(BANNER_SPACE_RESERVATION_CSS))
        section class="autumn-consent-banner" role="region" aria-label="Cookie consent" {
            p class="autumn-consent-banner__message" {
                "This site uses cookies. Strictly-necessary cookies (login, security) are always on. "
                "Others, like analytics, only run if you accept them."
            }
            form method="post" action="/consent/accept" class="autumn-consent-banner__actions" {
                @if let Some(token) = csrf_token {
                    input type="hidden" name="_csrf" value=(token);
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
/// Whenever it actually injects the banner (and therefore a live, per-visitor
/// CSRF token) into the body, it also stamps the response
/// `Cache-Control: private, no-store` and `Vary: Cookie` — otherwise a page
/// the app marked publicly cacheable (e.g. via `cache_for(..).public()`)
/// could have one visitor's CSRF token served to every other visitor (and
/// any CDN/proxy in between) until the cache entry expires.
pub async fn inject_consent_banner(
    request: Request<Body>,
    next: Next,
    policy_version: u32,
    csrf_cookie_name: &str,
) -> Response<Body> {
    let consent = Consent::from_headers(request.headers());
    let request_csrf_cookie = find_cookie(request.headers(), csrf_cookie_name);

    let response = next.run(request).await;

    if !consent.needs_prompt(policy_version) || !is_html_response(&response) {
        return response;
    }

    let csrf_token =
        extract_response_csrf_cookie(&response, csrf_cookie_name).or(request_csrf_cookie);
    let banner_html = consent_banner_markup(csrf_token.as_deref()).into_string();
    splice_into_response(response, &banner_html).await
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

async fn splice_into_response(response: Response<Body>, snippet: &str) -> Response<Body> {
    let (mut parts, body) = response.into_parts();
    let Ok(body) = axum::body::to_bytes(body, MAX_SPLICE_BODY_BYTES).await else {
        // Body exceeds MAX_SPLICE_BODY_BYTES (or failed to buffer). Bounding
        // memory use here matters: unlike the dev-only live-reload injector
        // this mirrors, this middleware runs unconditionally in production
        // for every undecided visitor, so an unbounded `usize::MAX` buffer
        // would be a per-request memory-exhaustion vector. The bytes read so
        // far are already discarded by this point, so the visitor gets an
        // empty body this one time (an exceedingly rare combination: a
        // multi-megabyte uncompressed HTML page from a visitor who hasn't
        // yet decided on consent) rather than an unbounded buffer.
        parts.headers.remove(CONTENT_LENGTH);
        return Response::from_parts(parts, Body::empty());
    };
    let updated = splice_before_body_close(&body, snippet);

    if updated == body {
        return Response::from_parts(parts, Body::from(body));
    }

    parts
        .headers
        .insert(CONTENT_LENGTH, HeaderValue::from(updated.len()));
    // A live, per-visitor CSRF token was just embedded in this body — never
    // let a CDN/proxy (or the browser) cache and replay it to someone else.
    parts
        .headers
        .insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    parts
        .headers
        .append(VARY, HeaderValue::from_static("Cookie"));
    Response::from_parts(parts, Body::from(updated))
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
        let html = consent_banner_markup(None).into_string();
        assert!(html.contains(r#"role="region""#));
        assert!(html.contains(r#"aria-label="Cookie consent""#));
    }

    #[test]
    fn banner_reject_and_accept_share_the_same_base_button_class() {
        let html = consent_banner_markup(None).into_string();
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
        let html = consent_banner_markup(None).into_string();
        assert!(!html.contains("_csrf"));
    }

    #[test]
    fn banner_includes_csrf_hidden_field_when_token_given() {
        let html = consent_banner_markup(Some("tok-123")).into_string();
        assert!(html.contains(r#"name="_csrf""#));
        assert!(html.contains(r#"value="tok-123""#));
    }

    #[test]
    fn banner_needs_no_script_tag() {
        let html = consent_banner_markup(Some("tok")).into_string();
        assert!(!html.contains("<script"), "banner must need no JS: {html}");
    }

    #[test]
    fn banner_buttons_are_keyboard_reachable_native_submit_buttons() {
        let html = consent_banner_markup(None).into_string();
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
    async fn oversized_body_is_bounded_not_buffered_without_limit() {
        // A body far larger than MAX_SPLICE_BODY_BYTES must not be fully
        // buffered into memory; the middleware falls back to an empty body
        // rather than holding an unbounded allocation.
        let oversized =
            "<html><body>".to_owned() + &"x".repeat(MAX_SPLICE_BODY_BYTES + 1) + "</body></html>";
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
        assert!(
            body.len() < MAX_SPLICE_BODY_BYTES,
            "oversized body must not pass through fully buffered/spliced: got {} bytes",
            body.len()
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
        let html = consent_banner_markup(None).into_string();
        assert!(
            html.contains("padding-bottom"),
            "banner must reserve viewport space so a fixed-position banner \
             doesn't permanently cover page content like a footer: {html}"
        );
    }
}
