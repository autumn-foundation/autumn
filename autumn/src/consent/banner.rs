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
use axum::http::{HeaderValue, Method, Request, Response};
use axum::middleware::Next;
use futures::StreamExt as _;

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

/// The literal opening tag [`consent_banner_markup`] always renders for its
/// outermost `<section>`, used by [`inject_consent_banner`] to detect
/// whether a handler already rendered the banner itself before splicing in
/// another copy.
///
/// Deliberately the *entire* opening tag rather than just the
/// `autumn-consent-banner` class name: ordinary page content (documentation
/// mentioning the class, or user-authored text containing that identifier)
/// could plausibly contain the bare class name, which would wrongly be
/// treated as an already-rendered banner and silently skip a required
/// prompt. Reproducing this whole tag verbatim, byte-for-byte, is not
/// something incidental prose does.
///
/// Must be kept in sync with `consent_banner_markup`'s `section` line — a
/// test (`rendered_banner_carries_the_detection_marker_verbatim`) asserts
/// this rather than relying on the two staying in sync by hand.
const RENDERED_BANNER_MARKER: &str =
    r#"<section class="autumn-consent-banner" role="region" aria-label="Cookie consent">"#;

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
/// customized `security.csrf.form_field`.
#[must_use]
pub fn consent_banner_markup(csrf_token: Option<&str>, csrf_field_name: &str) -> maud::Markup {
    maud::html! {
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
///             autumn_web::consent::DEFAULT_CSRF_FORM_FIELD,
///         ).await
///     }));
/// ```
///
/// Reads the visitor's consent cookie and the CSRF cookie directly off the
/// incoming request's `Cookie` header before calling `next`, so it has no
/// dependency on where `CsrfLayer` sits in the layer stack. `csrf_form_field`
/// is likewise taken as an explicit parameter rather than read from a
/// `CsrfFormField` request extension: the documented layer stack always
/// places user (`AppBuilder::layer`) middleware like this one outside
/// `CsrfLayer`, so that extension does not exist yet on the request side —
/// pass `security.csrf.form_field`'s configured value (or
/// [`super::DEFAULT_CSRF_FORM_FIELD`] if unconfigured). Non-HTML and
/// `Content-Encoding`-bearing responses (compressed bodies) pass through
/// untouched, exactly like the dev-mode live-reload injector.
///
/// An internal `autumn build` / ISR background-regeneration render — tagged
/// with a [`crate::static_gen::RenderDeadlineExempt`] request extension, per
/// that type's own docs — is passed through untouched without even
/// evaluating consent state: these renders have no real visitor and no
/// consent cookie, so injecting here would bake a banner (and a
/// build-time-only CSRF token) directly into the static HTML file written to
/// `dist/`. Every future visitor, including ones who have already consented,
/// would then receive that frozen-in-time banner forever, since it becomes
/// literal page content rather than something this middleware could
/// conditionally show or hide again at request time.
///
/// Also, while a visitor needs (re-)prompting: strips `If-None-Match` /
/// `If-Modified-Since` from the request before calling `next`, so an inner
/// `EtagLayer` can't short-circuit to a bodyless `304` — which would replay a
/// browser-cached, banner-less page and silently skip the required prompt
/// after a policy bump or a withdrawn consent.
///
/// Whenever it actually injects the banner (and therefore a live, per-visitor
/// CSRF token) into the body, it also stamps the response
/// `Cache-Control: private, no-store` and `Vary: Cookie` — otherwise a page
/// the app marked publicly cacheable (e.g. via `cache_for(..).public()`)
/// could have one visitor's CSRF token served to every other visitor (and
/// any CDN/proxy in between) until the cache entry expires. For an HTML
/// response where the visitor has already decided (so nothing is injected),
/// `Vary: Cookie` is still appended — the app's own handler can render
/// differently based on the same Consent cookie (e.g.
/// `consent.allows("analytics", ...)`-gated markup), so a shared cache must
/// never serve that decided visitor's exact representation to a different
/// one. `Cache-Control` is left alone in that case, since nothing per-visitor
/// was freshly embedded and the app's own caching choice should stand.
///
/// If the response already contains the banner's own marker class (e.g. a
/// "manage cookie preferences" handler rendered [`consent_banner_markup`]
/// itself so an already-decided visitor can change their choice), this
/// middleware skips injection rather than adding a second, identical copy.
///
/// A `HEAD` request needs no special-casing here: this middleware always
/// runs *inside* the Axum `Route` wrapper that actually turns a `GET`
/// handler's response into a `HEAD` one, so `response` still carries the
/// real, full body (and no `Content-Length` yet) when this function inspects
/// or splices into it, regardless of the request's method. Whatever this
/// middleware returns — banner spliced in, `Content-Length` updated, or left
/// untouched — is exactly what that outer wrapper uses to compute the final
/// `Content-Length` before emptying the body for `HEAD`, so the `HEAD`
/// response's metadata always matches the equivalent `GET`'s.
///
/// # Known limitation: `#[static_get]` pages and CSRF
///
/// A first-time visitor whose very first hit lands on a pre-rendered
/// `#[static_get]` page is served that page by the static-first middleware
/// before the dynamic router (and therefore `CsrfLayer`) ever runs, so no
/// CSRF cookie exists yet. This middleware then has no token to embed in the
/// banner's hidden field, and the resulting `/consent/accept` /
/// `/consent/reject` submission is rejected by `CsrfLayer` with a `403`. This
/// does not affect the default `autumn new` scaffold (which has no
/// `#[static_get]` routes); an app that adds one should exempt its consent
/// routes' path prefix, or route visitors through a dynamic page at least
/// once before relying on the static-served banner's buttons.
///
/// # Known limitation: no true streaming for an undecided visitor
///
/// Deliberately mirroring `crate::middleware::dev::inject_live_reload`'s
/// own buffer-then-splice design (see the module docs), this middleware
/// waits for the *entire* HTML body (up to `MAX_SPLICE_BODY_BYTES`) before
/// sending any bytes to the client, rather than forwarding chunks as they
/// arrive. A page that streams progressively (or simply takes a long time to
/// fully render) therefore loses that streaming behavior for any visitor who
/// still needs prompting — the visitor sees nothing until the handler
/// finishes, even though the eventual response comfortably fits the cap.
/// True incremental splicing would require scanning the stream chunk-by-chunk
/// for `</body>` without buffering it whole; that is a meaningfully more
/// complex streaming transform than this scaffold's cookie-consent banner
/// warrants, so — like the buffer-then-splice dev-mode injector it mirrors —
/// it is accepted as a tradeoff for a large majority of ordinary pages rather
/// than solved here.
pub async fn inject_consent_banner(
    mut request: Request<Body>,
    next: Next,
    policy_version: u32,
    csrf_cookie_name: &str,
    csrf_form_field: &str,
) -> Response<Body> {
    if request
        .extensions()
        .get::<crate::static_gen::RenderDeadlineExempt>()
        .is_some()
    {
        return next.run(request).await;
    }

    let consent = Consent::from_headers(request.headers());
    let request_csrf_cookie = find_cookie(request.headers(), csrf_cookie_name);

    let needs_prompt = consent.needs_prompt(policy_version);
    if needs_prompt {
        request.headers_mut().remove(IF_NONE_MATCH);
        request.headers_mut().remove(IF_MODIFIED_SINCE);
    }

    let mut response = next.run(request).await;

    if !is_html_response(&response) {
        return response;
    }

    // No `HEAD`-specific handling is needed here, even though this can
    // splice a banner into the body below: Axum's own per-route wrapper
    // (added by `.layer()`, wrapping this entire middleware) is what
    // actually converts a `GET` handler's response into a `HEAD` one — it
    // computes `Content-Length` from *this* middleware's returned body and
    // *then* empties it for `HEAD`, strictly after this function returns.
    // So by the time this code runs, `response` still carries the real,
    // full body (and no `Content-Length` yet) regardless of request method;
    // splicing into it here and letting `Content-Length` reflect the
    // spliced body is exactly the metadata Axum will carry through to the
    // final `HEAD` response once it empties the body.
    if !needs_prompt {
        // A handler can still render differently based on the visitor's
        // Consent cookie even when this middleware injects nothing (e.g.
        // `consent.allows("analytics", ...)`-gated markup) — this visitor
        // simply happens to have already decided. If the app marked the
        // page publicly cacheable, a shared cache must never serve this
        // visitor's exact representation to a different visitor (one who's
        // undecided, or decided under different categories), so it has to
        // vary on the same header this middleware itself reads consent
        // from. `append` (not `insert`) preserves any `Vary` the app's own
        // handler already set (e.g. `Accept-Language`).
        response
            .headers_mut()
            .append(VARY, HeaderValue::from_static("Cookie"));
        return response;
    }

    let csrf_token =
        extract_response_csrf_cookie(&response, csrf_cookie_name).or(request_csrf_cookie);
    let banner_html = consent_banner_markup(csrf_token.as_deref(), csrf_form_field).into_string();
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
        .is_some_and(|content_type| {
            // The media-type *essence* is only the part before the first
            // `;` (parameters like `charset=utf-8` follow) — matching
            // against the whole header value would also match an unrelated
            // type that merely contains this substring, e.g.
            // `text/html-patch+json` or `application/json;
            // profile="text/html"`, and wrongly splice banner markup into a
            // non-HTML payload. HTTP media-type tokens are case-insensitive
            // (RFC 9110 8.3.1) — a handler returning `Text/HTML` is exactly
            // as valid as `text/html`.
            let essence = content_type
                .split(';')
                .next()
                .unwrap_or(content_type)
                .trim();
            essence.eq_ignore_ascii_case("text/html")
        })
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
    /// The body stream errored before EOF. Carries the bytes read so far and
    /// the underlying error, so the caller can replay the prefix and then
    /// end the reconstructed body with the same error — an honest abnormal
    /// close — rather than silently discarding everything and returning a
    /// well-formed, misleadingly-complete-looking empty response.
    Errored { prefix: Bytes, error: axum::Error },
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
            Some(Err(error)) => {
                return CollectedBody::Errored {
                    prefix: Bytes::from(buf),
                    error,
                };
            }
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
            // A handler (e.g. a "manage cookie preferences" page) may have
            // already rendered the banner widget itself to let an
            // already-decided visitor change their choice. Splicing another
            // copy in here would give that response two identical forms —
            // check for the literal rendered marker (see
            // `RENDERED_BANNER_MARKER`'s doc for why the whole opening tag,
            // not just the bare class name) rather than assuming any
            // particular route.
            if contains_ascii_case_insensitive(&bytes, RENDERED_BANNER_MARKER.as_bytes()) {
                return Response::from_parts(parts, Body::from(bytes));
            }

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
        // Content-Length stays correct and no Cache-Control change is needed
        // (nothing per-visitor was embedded). This path is only reached for a
        // visitor who still needs prompting, and the app's own handler can
        // still gate markup on the same Consent cookie regardless of whether
        // the banner itself got spliced in, so — exactly like the
        // decided-but-not-injected case above — a shared cache must still
        // vary on it rather than conflate this undecided visitor's oversized
        // representation with a decided visitor's.
        CollectedBody::Oversized(body) => {
            parts
                .headers
                .append(VARY, HeaderValue::from_static("Cookie"));
            Response::from_parts(parts, body)
        }
        // The body stream errored before EOF. Rather than silently discarding
        // everything and returning a well-formed, misleadingly-complete
        // empty `200` — the page did not actually load successfully —
        // replay whatever bytes were already read and then end the
        // reconstructed stream with the same error, mirroring
        // `crate::etag::apply_etag`'s identical handling of a body read
        // failure: the connection aborts abnormally, an honest signal (to
        // the client and any caching proxy) that the transfer failed rather
        // than completed.
        CollectedBody::Errored { prefix, error } => {
            parts.headers.remove(CONTENT_LENGTH);
            let frames: Vec<Result<Bytes, axum::Error>> = if prefix.is_empty() {
                vec![Err(error)]
            } else {
                vec![Ok(prefix), Err(error)]
            };
            Response::from_parts(parts, Body::from_stream(futures::stream::iter(frames)))
        }
    }
}

/// Insert `snippet` just before the last `</body>` tag, or append it at the
/// very end otherwise. The caller ([`splice_into_response`], via
/// [`is_html_response`]) only ever invokes this on a response whose
/// `Content-Type` is already validated as `text/html`, so a document that
/// omits its `<html>`/`<body>` wrapper tags entirely — valid HTML5 tag
/// omission, e.g. `<!doctype html><main>...</main>` — is still a real page a
/// browser parses and implicitly wraps in `<body>`; appending at the end
/// still lands the banner where a browser renders it, rather than silently
/// sending no banner just because no explicit wrapper tag was present.
///
/// Operates directly on the raw bytes rather than decoding through
/// `String::from_utf8_lossy`: an HTML page using a legacy single-byte charset
/// (e.g. ISO-8859-1) is not valid UTF-8, and lossy-decoding it would silently
/// replace those bytes with U+FFFD, corrupting page content while its
/// `<meta charset>` declaration kept claiming the original encoding. Tag
/// matching is ASCII case-insensitive (`<HTML><BODY>...` is exactly as valid
/// HTML as lowercase), which a byte-for-byte comparison must do explicitly.
/// `snippet` is always plain ASCII-safe markup, so splicing it in at an
/// ASCII tag's byte offset can never straddle a multi-byte UTF-8 sequence.
fn splice_before_body_close(body: &[u8], snippet: &str) -> Vec<u8> {
    if let Some(index) = rfind_ascii_case_insensitive(body, b"</body>") {
        let mut out = Vec::with_capacity(body.len() + snippet.len());
        out.extend_from_slice(&body[..index]);
        out.extend_from_slice(snippet.as_bytes());
        out.extend_from_slice(&body[index..]);
        return out;
    }

    let mut out = body.to_vec();
    out.extend_from_slice(snippet.as_bytes());
    out
}

/// Byte offset of the last case-insensitive (ASCII-only) match of `needle`
/// in `haystack`. See [`splice_before_body_close`] for why this operates on
/// raw bytes instead of a decoded `&str`.
fn rfind_ascii_case_insensitive(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    (0..=haystack.len() - needle.len())
        .rev()
        .find(|&i| haystack[i..i + needle.len()].eq_ignore_ascii_case(needle))
}

/// Whether `haystack` contains a case-insensitive (ASCII-only) match of
/// `needle` anywhere.
fn contains_ascii_case_insensitive(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if haystack.len() < needle.len() {
        return false;
    }
    (0..=haystack.len() - needle.len())
        .any(|i| haystack[i..i + needle.len()].eq_ignore_ascii_case(needle))
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
        let html = consent_banner_markup(None, "_csrf").into_string();
        assert!(html.contains(r#"role="region""#));
        assert!(html.contains(r#"aria-label="Cookie consent""#));
    }

    #[test]
    fn banner_reject_and_accept_share_the_same_base_button_class() {
        let html = consent_banner_markup(None, "_csrf").into_string();
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
        let html = consent_banner_markup(None, "_csrf").into_string();
        assert!(!html.contains("_csrf"));
    }

    #[test]
    fn banner_includes_csrf_hidden_field_when_token_given() {
        let html = consent_banner_markup(Some("tok-123"), "_csrf").into_string();
        assert!(html.contains(r#"name="_csrf""#));
        assert!(html.contains(r#"value="tok-123""#));
    }

    #[test]
    fn banner_needs_no_script_tag() {
        let html = consent_banner_markup(Some("tok"), "_csrf").into_string();
        assert!(!html.contains("<script"), "banner must need no JS: {html}");
    }

    #[test]
    fn banner_buttons_are_keyboard_reachable_native_submit_buttons() {
        let html = consent_banner_markup(None, "_csrf").into_string();
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
    fn splice_appends_when_no_recognizable_html_wrapper_tag_is_present() {
        // The caller only ever invokes this on a response whose Content-Type
        // is already validated as text/html (is_html_response), so a
        // document that omits its <html>/<body> wrapper tags entirely --
        // valid HTML5 tag omission, e.g. `<!doctype html><main>...</main>`
        // -- is still a real page a browser renders; it must still get the
        // banner appended rather than being silently skipped.
        let out = splice_before_body_close(b"<!doctype html><main>ok</main>", "<snip>");
        let s = String::from_utf8(out).unwrap();
        assert!(s.ends_with("<snip>"), "{s}");
    }

    #[test]
    fn splice_matches_uppercase_and_mixed_case_tags() {
        let out = splice_before_body_close(b"<HTML><BODY><main>ok</main></BODY></HTML>", "<snip>");
        let s = String::from_utf8(out).unwrap();
        assert!(
            s.contains("<snip></BODY>"),
            "an uppercase `</BODY>` is exactly as valid HTML as lowercase: {s}"
        );
    }

    #[test]
    fn splice_appends_when_only_uppercase_html_shell_present() {
        let out = splice_before_body_close(b"<HTML><main>ok</main></HTML>", "<snip>");
        let s = String::from_utf8(out).unwrap();
        assert!(s.ends_with("<snip>"), "{s}");
    }

    #[test]
    fn splice_preserves_non_utf8_bytes_in_a_legacy_charset_document() {
        // A byte sequence that is not valid UTF-8 (0xE9 alone is an ISO-8859-1
        // 'é', but an invalid/incomplete UTF-8 sequence on its own).
        // `String::from_utf8_lossy` would replace it with U+FFFD; this must
        // pass it through byte-for-byte instead, since the raw bytes are
        // spliced around, never decoded.
        let mut body = b"<html><body>caf\xE9".to_vec();
        body.extend_from_slice(b"</body></html>");
        let out = splice_before_body_close(&body, "<snip>");
        let out_str = String::from_utf8_lossy(&out);
        assert!(
            out.windows(4).any(|w| w == b"caf\xE9"),
            "non-UTF-8 bytes must survive splicing untouched, not become U+FFFD: {out_str}"
        );
        assert!(out_str.contains("<snip></body>"));
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
    fn html_response_detected_regardless_of_media_type_case() {
        // HTTP media-type tokens are case-insensitive (RFC 9110 8.3.1).
        let response = Response::builder()
            .header(CONTENT_TYPE, "Text/HTML; charset=utf-8")
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
    fn media_type_that_merely_contains_text_html_as_a_substring_is_not_html() {
        // `text/html-patch+json` and `application/json; profile="text/html"`
        // both contain the substring "text/html" but are not the `text/html`
        // media type — matching on the whole header value would wrongly
        // treat either as HTML and splice banner markup into a non-HTML
        // payload, corrupting it for its actual consumer.
        let response = Response::builder()
            .header(CONTENT_TYPE, "text/html-patch+json")
            .body(Body::empty())
            .unwrap();
        assert!(!is_html_response(&response));

        let response = Response::builder()
            .header(CONTENT_TYPE, r#"application/json; profile="text/html""#)
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
                inject_consent_banner(req, next, version, "autumn-csrf", "_csrf").await
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
                    inject_consent_banner(req, next, POLICY_VERSION, "autumn-csrf", "_csrf").await
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
                inject_consent_banner(req, next, 1, "autumn-csrf", "_csrf").await
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
                inject_consent_banner(req, next, 1, "autumn-csrf", "_csrf").await
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
    async fn no_cache_control_added_when_consent_already_decided() {
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
            "no banner injected, so this middleware must leave the app's own Cache-Control alone"
        );
    }

    #[tokio::test]
    async fn varies_on_cookie_even_when_consent_already_decided_and_nothing_is_injected() {
        // The app's own handler can render differently based on the same
        // Consent cookie (e.g. `consent.allows("analytics", ...)`-gated
        // markup) even when this middleware itself injects nothing for an
        // already-decided visitor. A shared cache that stored this
        // visitor's exact representation must never replay it to a
        // different visitor (undecided, or decided under different
        // categories), so the response must still vary on `Cookie`.
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
        assert_eq!(
            response.headers().get(VARY).and_then(|v| v.to_str().ok()),
            Some("Cookie"),
            "a decided-but-not-injected HTML response must still vary on Cookie"
        );
    }

    #[tokio::test]
    async fn undecided_visitors_head_request_matches_the_spliced_gets_content_length_and_cache_control()
     {
        // This middleware runs inside the Axum `Route` wrapper that turns a
        // `GET` handler's response into a `HEAD` one, so it still sees (and
        // splices into) the real, full body here regardless of request
        // method — the outer wrapper computes `Content-Length` from
        // whatever this middleware returns and only empties the body
        // afterward. So an undecided visitor's `HEAD` response must carry
        // the same post-splice `Content-Length` and `Cache-Control` its
        // `GET` counterpart would.
        let unspliced_get_len = axum::body::to_bytes(html_page().into_body(), usize::MAX)
            .await
            .unwrap()
            .len();
        let banner_len = consent_banner_markup(None, "_csrf").into_string().len();

        let app = app_with_policy_version(1);
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::HEAD)
                    .uri("/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.headers().get(VARY).and_then(|v| v.to_str().ok()),
            Some("Cookie"),
            "an undecided visitor's HEAD response must still vary on Cookie"
        );
        assert_eq!(
            response
                .headers()
                .get(CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some("private, no-store"),
            "an undecided visitor's HEAD response must match the GET path's uncacheable directive"
        );
        assert_eq!(
            response
                .headers()
                .get(CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok()),
            Some((unspliced_get_len + banner_len).to_string().as_str()),
            "an undecided visitor's HEAD response must report the post-splice Content-Length, \
             not the pre-splice one"
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(
            body.is_empty(),
            "a HEAD response must never carry a spliced-in body"
        );
    }

    #[tokio::test]
    async fn decided_visitors_head_request_leaves_representation_metadata_untouched() {
        // Once a visitor has already decided, the GET path injects nothing
        // and leaves Content-Length/Cache-Control alone — so the equivalent
        // HEAD response has no reason to touch them either, beyond adding
        // Vary: Cookie like every other HTML response here.
        let unspliced_get_len = axum::body::to_bytes(html_page().into_body(), usize::MAX)
            .await
            .unwrap()
            .len();
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
                    .method(Method::HEAD)
                    .uri("/")
                    .header("cookie", format!("autumn.consent={raw_value}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.headers().get(VARY).and_then(|v| v.to_str().ok()),
            Some("Cookie")
        );
        assert!(response.headers().get(CACHE_CONTROL).is_none());
        assert_eq!(
            response
                .headers()
                .get(CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok()),
            Some(unspliced_get_len.to_string().as_str())
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
                inject_consent_banner(req, next, 1, "autumn-csrf", "_csrf").await
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
    async fn oversized_body_still_varies_on_cookie() {
        // This path is only reached for a visitor who still needs
        // prompting. Even though the banner itself isn't spliced in (the
        // body is too large), the app's own handler can still gate markup
        // on the same Consent cookie, so a shared cache must not conflate
        // this undecided visitor's oversized representation with a decided
        // visitor's.
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
                inject_consent_banner(req, next, 1, "autumn-csrf", "_csrf").await
            }));
        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            response.headers().get(VARY).and_then(|v| v.to_str().ok()),
            Some("Cookie"),
            "an oversized undecided-visitor response must still vary on Cookie"
        );
    }

    #[tokio::test]
    async fn does_not_splice_a_second_banner_when_the_handler_already_rendered_one() {
        // A "manage cookie preferences" handler may render the banner widget
        // itself so an already-decided visitor can change their choice. If
        // that visitor is undecided (or on a stale policy version),
        // `needs_prompt` is still true and this middleware must not splice a
        // second, identical copy into the same response.
        let app = Router::new()
            .route(
                "/",
                get(|| async {
                    let banner = consent_banner_markup(None, "_csrf").into_string();
                    Response::builder()
                        .header(CONTENT_TYPE, "text/html; charset=utf-8")
                        .body(Body::from(format!("<html><body>{banner}</body></html>")))
                        .unwrap()
                }),
            )
            .layer(axum::middleware::from_fn(move |req, next| async move {
                inject_consent_banner(req, next, 1, "autumn-csrf", "_csrf").await
            }));
        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert_eq!(
            html.matches(RENDERED_BANNER_MARKER).count(),
            1,
            "must not splice a second banner when the response already contains one: {html}"
        );
    }

    #[tokio::test]
    async fn still_injects_when_page_merely_mentions_the_banner_class_in_prose() {
        // Documentation or user-authored content mentioning the
        // `autumn-consent-banner` class name in passing (without actually
        // rendering the widget's specific opening tag) must NOT be mistaken
        // for an already-rendered banner — that would silently withhold the
        // required prompt from an undecided visitor.
        let app = Router::new()
            .route(
                "/",
                get(|| async {
                    Response::builder()
                        .header(CONTENT_TYPE, "text/html; charset=utf-8")
                        .body(Body::from(
                            "<html><body><p>Style the banner via the \
                             <code>autumn-consent-banner</code> class.</p></body></html>"
                                .to_owned(),
                        ))
                        .unwrap()
                }),
            )
            .layer(axum::middleware::from_fn(move |req, next| async move {
                inject_consent_banner(req, next, 1, "autumn-csrf", "_csrf").await
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
            html.contains(RENDERED_BANNER_MARKER),
            "an undecided visitor must still get a real, rendered prompt: {html}"
        );
    }

    #[test]
    fn rendered_banner_carries_the_detection_marker_verbatim() {
        let html = consent_banner_markup(None, "_csrf").into_string();
        assert!(
            html.contains(RENDERED_BANNER_MARKER),
            "RENDERED_BANNER_MARKER must be kept in sync with consent_banner_markup's \
             actual rendered output: {html}"
        );
    }

    #[tokio::test]
    async fn body_stream_error_replays_buffered_prefix_then_ends_with_the_same_error() {
        // The bytes read so far must not be silently discarded in favor of a
        // clean-looking empty `200` — that would misrepresent a genuine
        // transport/body failure as a successful (if blank) page. Instead
        // the reconstructed body must replay the prefix and then end
        // abnormally with the same error, mirroring
        // `crate::etag::apply_etag`'s identical handling.
        let prefix = Bytes::from_static(b"<html><body>partial");
        let error = axum::Error::new(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "simulated upstream body error",
        ));
        let frames: Vec<Result<Bytes, axum::Error>> = vec![Ok(prefix.clone()), Err(error)];
        let body = Body::from_stream(futures::stream::iter(frames));
        let response = Response::builder()
            .header(CONTENT_TYPE, "text/html; charset=utf-8")
            .body(body)
            .unwrap();

        let spliced = splice_into_response(response, "<snip>").await;
        let mut stream = spliced.into_body().into_data_stream();
        let first = stream
            .next()
            .await
            .expect("the buffered prefix must be replayed")
            .expect("the prefix chunk must be Ok");
        assert_eq!(
            first, prefix,
            "bytes read before the error must be replayed, not discarded"
        );
        let second = stream
            .next()
            .await
            .expect("the reconstructed body must end with an error frame, not silently end");
        assert!(
            second.is_err(),
            "the reconstructed body must end abnormally with the original error"
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
                inject_consent_banner(req, next, 1, "my-csrf", "_csrf").await
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

    // ── configured CSRF form-field name ──────────────────────────────

    #[test]
    fn banner_uses_default_csrf_field_name() {
        let html = consent_banner_markup(Some("tok"), "_csrf").into_string();
        assert!(html.contains(r#"name="_csrf""#));
    }

    #[test]
    fn banner_honors_custom_csrf_field_name() {
        let html = consent_banner_markup(Some("tok"), "authenticity_token").into_string();
        assert!(html.contains(r#"name="authenticity_token""#));
        assert!(!html.contains(r#"name="_csrf""#));
    }

    #[tokio::test]
    async fn banner_honors_configured_csrf_form_field_name_end_to_end() {
        // The form-field name is config-derived, not request-derived (see
        // `inject_consent_banner`'s doc: `CsrfLayer` sits inner to user
        // layers, so a `CsrfFormField` request extension is never visible
        // here) -- so it must be threaded in as an explicit parameter, not
        // read back off the request or response.
        let app = Router::new()
            .route("/", get(|| async { html_page() }))
            .layer(axum::middleware::from_fn(move |req, next| async move {
                inject_consent_banner(req, next, 1, "autumn-csrf", "authenticity_token").await
            }));

        let request = Request::builder()
            .uri("/")
            .header("cookie", "autumn-csrf=tok-abc")
            .body(Body::empty())
            .unwrap();

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
                inject_consent_banner(req, next, 1, "autumn-csrf", "_csrf").await
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
                inject_consent_banner(req, next, 1, "autumn-csrf", "_csrf").await
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

    // ── build/ISR internal renders are exempt ─────────────────────────

    #[tokio::test]
    async fn skips_injection_entirely_for_a_render_deadline_exempt_request() {
        // `RenderDeadlineExempt` marks an internal `autumn build` / ISR
        // background-regeneration render, driven directly via `oneshot` with
        // no real visitor and no consent cookie. Injecting the banner here
        // would bake it (plus a build-time-only CSRF token) into the static
        // HTML file written to `dist/`, and every future visitor -- including
        // ones who have already consented -- would then receive that frozen
        // banner forever, since it becomes literal page content this
        // middleware can no longer conditionally hide. A live inbound request
        // never carries this marker, so this exemption cannot be reached by
        // an ordinary visitor.
        let app = Router::new()
            .route("/", get(|| async { html_page() }))
            .layer(axum::middleware::from_fn(move |req, next| async move {
                inject_consent_banner(req, next, 1, "autumn-csrf", "_csrf").await
            }));

        let request = Request::builder()
            .uri("/")
            .extension(crate::static_gen::RenderDeadlineExempt)
            .body(Body::empty())
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            !html.contains("autumn-consent-banner"),
            "an internal build/ISR render must never have the banner baked in: {html}"
        );
    }
}
