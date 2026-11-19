//! Consent-banner widget and the middleware that auto-injects it.
//!
//! [`inject_consent_banner`] is a response-body-splice middleware — it
//! detects an HTML response and, when the visitor's [`super::Consent`] needs
//! (re-)prompting, inserts the banner markup right before `</body>`. This
//! mirrors [`crate::middleware::dev::inject_live_reload`]'s proven
//! detect-HTML / splice-before-`</body>` / fix-`Content-Length` pattern, so
//! HTML pages in the app show the banner automatically — no per-handler
//! wiring, and no change to the app's shared `layout()` function signature
//! (which `autumn generate scaffold` depends on staying a stable 4-arg call).
//!
//! Six cases are deliberately **not** injected into, so "the layer is
//! registered" is not the same as "every page prompts":
//!
//! 1. an htmx *fragment* response, which has no `</body>` to splice before and
//!    whose swap would put a second banner on the page;
//! 2. a *static cache hit* where CSRF is enforced but no token is obtainable,
//!    since the banner's buttons would `403`;
//! 3. an **encoded** body (`Content-Encoding`), which cannot be spliced without
//!    decoding it first — see [`is_html_response`]. Whether this happens is
//!    decided by layer order: injection sees plain HTML only if it runs
//!    *inside* the compression layer;
//! 4. a `206 Partial Content` body (or any response carrying `Content-Range`),
//!    whose bytes are a slice of a larger representation that `Content-Range`
//!    describes — splicing would corrupt the range;
//! 5. a **download** (`Content-Disposition: attachment`), whose bytes become a
//!    file on disk rather than a page — splicing would edit the export and
//!    write the visitor's live CSRF token into it;
//! 6. a body over [`MAX_SPLICE_BODY_BYTES`].
//!
//! All six still get `Vary: Cookie`, because the representation depends on the
//! consent cookie whether or not anything was injected. Each is documented on
//! [`inject_consent_banner`] and summarized for app authors in
//! `docs/guide/cookie-consent.md`.

use axum::body::{Body, Bytes};
use axum::http::header::{
    ACCEPT_RANGES, CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_ENCODING, CONTENT_LENGTH,
    CONTENT_RANGE, CONTENT_TYPE, ETAG, IF_MODIFIED_SINCE, IF_NONE_MATCH, SET_COOKIE, VARY,
};
use axum::http::{HeaderValue, Request, Response};
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
///             Some(autumn_web::consent::DEFAULT_CSRF_COOKIE_NAME),
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
/// Both headers are instructions to *HTTP* caches — the browser, a CDN, a
/// reverse proxy. They do not reach a cache living inside the application:
/// [`CacheResponseLayer`](crate::cache::CacheResponseLayer) keys its entries
/// on the request URI alone and reads neither header, so a consent-varying
/// route registered behind it is unsafe in either stacking order. Inside
/// this middleware, the layer stores the handler's pre-injection body, and a
/// visitor who allowed a category populates it with
/// `consent.allows(..)`-gated markup that a visitor who refused then gets on
/// a cache hit — this middleware injects nothing for them, because they have
/// decided, so nothing corrects it. Outside this middleware, it stores the
/// *injected* body, CSRF token and all, and replays that. Keep
/// consent-varying routes out of `CacheResponseLayer`, or give it a key that
/// includes the consent decision.
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
/// # `#[static_get]` pages: the banner is skipped, not broken
///
/// A first-time visitor whose very first hit lands on a pre-rendered
/// `#[static_get]` page is served that page by the static-first middleware.
/// User `.layer()` middleware — including this one — is applied *outside* that
/// layer so it can process pre-rendered responses, but `CsrfLayer` runs inside
/// the dynamic router and therefore never runs at all on a static cache hit,
/// so no CSRF cookie exists yet.
///
/// When `csrf_cookie_name` is `Some(..)` and no token can be found, this
/// middleware **skips injection** rather than emitting a banner whose
/// `/consent/accept` / `/consent/reject` submission `CsrfLayer` would reject
/// with a `403`. The visitor is simply not prompted on that page; the first
/// dynamic page they reach issues a token and prompts them normally, and the
/// gate stays closed in the meantime, so no non-essential cookie is set while
/// they are undecided.
///
/// Do **not** exempt the consent routes from CSRF to work around this. Those
/// are the state-changing `POST`s the protection exists for, and the skip
/// above already removes the failure that made exemption tempting.
///
/// One residual edge remains, and it is not specific to consent: on a static
/// hit the request's own CSRF cookie is used as the fallback token, and
/// `CsrfLayer` never ran to validate or refresh it. A cookie that is stale
/// (signing key rotated) or tampered is therefore embedded as-is, and the
/// accept/reject `POST` will `403`. This middleware cannot tell the difference
/// — it is given a cookie *name*, not the signing key — and the same stale
/// cookie would fail on any other cached form page in the app. The visitor
/// recovers on their first dynamic page, which reissues a valid token.
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
    csrf_cookie_name: Option<&str>,
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
    let request_csrf_cookie =
        csrf_cookie_name.and_then(|name| find_cookie(request.headers(), name));

    let needs_prompt = consent.needs_prompt(policy_version);
    // Drop the conditional validators so an inner `EtagLayer` cannot answer a bodyless `304`
    // that replays a cached, banner-less page. Only for a client that could be shown a banner:
    // an API client never gets a consent cookie, so stripping its validators would cost it
    // every `304` for good. The response's content type is not known yet, so this goes by
    // the request headers.
    if needs_prompt && could_render_a_banner(request.headers()) {
        request.headers_mut().remove(IF_NONE_MATCH);
        request.headers_mut().remove(IF_MODIFIED_SINCE);
    }

    let mut response = next.run(request).await;

    if !is_html_content_type(&response) {
        return response;
    }

    // A `206` body is a slice of a larger representation that `Content-Range` describes, so
    // splicing would corrupt the reassembled document. The header is checked as well as the
    // status, since a handler can attach `Content-Range` without it.
    //
    // An attachment becomes a file on disk: splicing would edit the export and write the
    // visitor's live CSRF token into it.
    if is_attachment(&response)
        || response.status() == axum::http::StatusCode::PARTIAL_CONTENT
        || response.headers().contains_key(CONTENT_RANGE)
    {
        response
            .headers_mut()
            .append(VARY, HeaderValue::from_static("Cookie"));
        return response;
    }

    // The media type is HTML, so `!is_html_response` means `Content-Encoding`: HTML that
    // cannot be spliced without decoding. It still varies on the consent cookie.
    if !is_html_response(&response) {
        response
            .headers_mut()
            .append(VARY, HeaderValue::from_static("Cookie"));
        return response;
    }

    if !needs_prompt {
        // Nothing is injected, but the handler may render `Consent::allows`-gated markup, so a
        // shared cache must vary on the cookie. `append` keeps any `Vary` the handler set.
        response
            .headers_mut()
            .append(VARY, HeaderValue::from_static("Cookie"));
        return response;
    }

    let csrf_token = csrf_cookie_name
        .and_then(|name| extract_response_csrf_cookie(&response, name))
        .or(request_csrf_cookie);

    // CSRF is enforced but there is no token, so the banner's buttons would `403`. This
    // happens on a static cache hit, where `CsrfLayer` never runs. Skip the banner: the next
    // dynamic page prompts, and the gate stays closed while the visitor is undecided.
    if csrf_cookie_name.is_some() && csrf_token.is_none() {
        tracing::debug!(
            "consent banner skipped: CSRF is enforced but no token was available \
             (typically a pre-rendered static page, where CsrfLayer never runs)"
        );
        // Bannerless only because this visitor has no CSRF cookie. Without `Vary: Cookie` a
        // CDN could replay it to a visitor who has one and suppress their prompt.
        response
            .headers_mut()
            .append(VARY, HeaderValue::from_static("Cookie"));
        return response;
    }

    let banner_html = consent_banner_markup(csrf_token.as_deref(), csrf_form_field).into_string();
    splice_into_response(response, &banner_html).await
}

/// Whether the client offered to accept an HTML document.
///
/// Only such a request can be answered with a page carrying the banner, so
/// only such a request has a reason to lose its conditional validators (see
/// [`inject_consent_banner`]). A missing `Accept` header is treated as
/// accepting HTML: the header is optional, and defaulting the other way would
/// silently skip the prompt for a client that does render pages.
///
/// A bare `*/*` — what `curl` and many API clients send — is deliberately NOT
/// treated as accepting HTML: those callers do not render a banner, and
/// keeping their validators is what preserves their `304`s.
fn could_render_a_banner(headers: &axum::http::HeaderMap) -> bool {
    // Any htmx request counts: no header separates a fragment from a whole-document swap
    // (`hx-get` with `hx-target="body"` sends none), and htmx's XHR sends `Accept: */*`,
    // which `accepts_html` rejects. The cost is a full response instead of a `304` for htmx
    // clients while a prompt is due; API clients keep their `304`s.
    accepts_html(headers) || htmx_header_is_true(headers, "hx-request")
}

/// An htmx boolean header: present and not the literal `false` htmx never sends.
fn htmx_header_is_true(headers: &axum::http::HeaderMap, name: &str) -> bool {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| !value.eq_ignore_ascii_case("false"))
}

/// Drop the headers that describe the exact bytes of a representation.
///
/// Called only where this middleware rewrites the body. `Content-Length` is
/// recomputed at the call site; these cannot be, because they are whatever the
/// inner layer chose to compute, so the honest move is to remove them rather
/// than ship a validator for bytes nobody will receive.
fn drop_stale_representation_metadata(headers: &mut axum::http::HeaderMap) {
    headers.remove(ETAG);
    // Withdraw the range offer: a `Range` request is answered from the original bannerless
    // bytes, so resuming into the longer spliced body would corrupt it.
    headers.remove(ACCEPT_RANGES);
    // Not `http::header` constants, so they are named here. `Content-Digest`
    // and `Repr-Digest` are RFC 9530; `Digest` and `Content-MD5` are the
    // obsolete spellings still emitted by some proxies and SDKs.
    for name in ["content-digest", "repr-digest", "digest", "content-md5"] {
        headers.remove(name);
    }
}

/// Whether the client offered to accept an HTML document.
fn accepts_html(headers: &axum::http::HeaderMap) -> bool {
    // RFC 9110 §5.3: `Accept` may arrive as several field lines forming one list, so scan
    // them all. §12.5.1: the most specific matching range decides, so in
    // `text/*;q=1, text/html;q=0` HTML is refused. Record a verdict per specificity and let
    // the exact one win. The first occurrence of a range wins.
    let mut offered = false;
    let mut exact: Option<bool> = None;
    let mut wildcard: Option<bool> = None;

    for value in headers.get_all(axum::http::header::ACCEPT) {
        offered = true;
        // A non-UTF-8 line tells us nothing; another line may still decide.
        let Ok(accept) = value.to_str() else {
            continue;
        };
        for range in accept.split(',') {
            let mut parts = range.split(';').map(str::trim);
            let Some(media) = parts.next() else {
                continue;
            };
            // `*/*` is excluded: it is the XHR default, and counting it would strip every
            // API client's conditional validators.
            let slot = if media.eq_ignore_ascii_case("text/html") {
                &mut exact
            } else if media.eq_ignore_ascii_case("text/*") {
                &mut wildcard
            } else {
                continue;
            };
            // `q=0` is a refusal. Any other quality, even a malformed one, is accepted.
            let accepted = !parts.any(|param| {
                param
                    .split_once('=')
                    .is_some_and(|(k, v)| k.eq_ignore_ascii_case("q") && is_zero_quality(v))
            });
            slot.get_or_insert(accepted);
        }
    }

    // `text/*` decides only when `text/html` is not named. With neither, a missing `Accept`
    // means yes (the header is optional) and one naming neither means no.
    exact.or(wildcard).unwrap_or(!offered)
}

/// Whether an RFC 9110 quality value is exactly zero (`0`, `0.`, `0.000`).
fn is_zero_quality(v: &str) -> bool {
    let v = v.trim();
    let (int, frac) = v.split_once('.').unwrap_or((v, ""));
    int == "0" && frac.chars().all(|c| c == '0')
}

/// Whether this request came from htmx, and therefore expects a fragment
/// rather than a complete document.
///
/// htmx sets `HX-Request: true` on every request it issues, and additionally
/// `HX-Boosted: true` when the request came from an `hx-boost`ed link or form.
/// A boosted request is a full-page navigation — the response is a complete
/// document that replaces the current body — so only a request that is htmx
/// *and not* boosted is a fragment request.
///
/// Matching the framework's other htmx-aware paths (`crate::htmx`, the
/// tracked-job poll route), a header's presence is what matters — the value is
/// not parsed beyond confirming it is not the literal `false` htmx never
/// actually sends.
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
    // An encoded body cannot be spliced without decoding it. It is still HTML and still
    // varies on the consent cookie; see `is_html_content_type`.
    if response.headers().contains_key(CONTENT_ENCODING) {
        return false;
    }
    is_html_content_type(response)
}

/// Whether the response's media type is `text/html`, **regardless of whether it
/// is encoded**.
///
/// [`is_html_response`] answers "can this be spliced into"; this answers "is
/// this a page whose representation depends on the consent cookie". The two
/// differ for a compressed response, and conflating them meant a
/// `Content-Encoding` HTML response skipped `Vary: Cookie` entirely — so a
/// shared cache could serve one visitor's consent-dependent markup to a visitor
/// who chose differently.
fn is_html_content_type(response: &Response<Body>) -> bool {
    response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|content_type| {
            // Compare only the essence before `;`. A substring match would also hit
            // `text/html-patch+json` or `application/json; profile="text/html"`. Media
            // types are case-insensitive (RFC 9110 §8.3.1).
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
            // An empty body is not a page to prompt in. The reachable case is a `HEAD` hit on
            // a pre-rendered page: the static-first middleware answers it with `Body::empty()`
            // and this layer runs outside it, so no Axum route wrapper fixes up the result.
            // Keyed on the empty body, not the method: on a route Axum wraps, `HEAD` still
            // carries the full body here and must be spliced so its `Content-Length` matches
            // the `GET`.
            if bytes.is_empty() {
                parts
                    .headers
                    .append(VARY, HeaderValue::from_static("Cookie"));
                return Response::from_parts(parts, Body::from(bytes));
            }

            if contains_ascii_case_insensitive(&bytes, RENDERED_BANNER_MARKER.as_bytes()) {
                // The handler rendered the banner itself (a "manage preferences" page), so
                // add no second copy. Its form carries a live CSRF token: apply the same
                // cache guards as the injection path.
                parts
                    .headers
                    .insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
                parts
                    .headers
                    .append(VARY, HeaderValue::from_static("Cookie"));
                return Response::from_parts(parts, Body::from(bytes));
            }

            // `None` means the body does not open like a document: an htmx fragment, which
            // must not get a banner. This is the fragment test because no request header
            // separates a fragment from a whole-document swap. A fragment may still hold
            // consent-gated markup, so it still varies on the cookie.
            let Some(updated) = splice_before_body_close(&bytes, snippet) else {
                parts
                    .headers
                    .append(VARY, HeaderValue::from_static("Cookie"));
                return Response::from_parts(parts, Body::from(bytes));
            };

            parts
                .headers
                .insert(CONTENT_LENGTH, HeaderValue::from(updated.len()));
            // Validators and digests from an inner layer now describe bytes not being sent; a
            // client verifying `Content-Digest` would reject the page. Drop them rather than
            // re-derive the inner layer's scheme.
            drop_stale_representation_metadata(&mut parts.headers);
            // The body now holds a per-visitor CSRF token: no cache may replay it.
            parts
                .headers
                .insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
            parts
                .headers
                .append(VARY, HeaderValue::from_static("Cookie"));
            Response::from_parts(parts, Body::from(updated))
        }
        // Over `MAX_SPLICE_BODY_BYTES`: serve it unmodified, without the banner, rather than
        // drop it. The bytes are unchanged, so `Content-Length` stands and nothing
        // per-visitor needs `Cache-Control`. The handler may still gate markup on the
        // consent cookie, so vary on it.
        CollectedBody::Oversized(body) => {
            parts
                .headers
                .append(VARY, HeaderValue::from_static("Cookie"));
            Response::from_parts(parts, body)
        }
        // Replay the bytes read so far, then end the stream with the same error, so the
        // client and any proxy see a failed transfer rather than a complete empty `200`.
        // Mirrors `crate::etag::apply_etag`.
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
fn splice_before_body_close(body: &[u8], snippet: &str) -> Option<Vec<u8>> {
    // Check the opening first: `</body>` bytes alone are no document test, since a fragment
    // can carry them in a `<script>` string or a comment. This is a fail-safe shape test,
    // not a parser: unrecognised input is left alone, so the failure is a missing banner,
    // never mangled markup.
    if !starts_like_a_document(body) {
        return None;
    }

    // Splice before `</body>` only when nothing but `</html>` and whitespace follows it. The
    // same bytes in a script string or a trailing comment have other content after them;
    // then append instead, as for a document with no `</body>`.
    if let Some(index) = rfind_ascii_case_insensitive(body, b"</body>")
        && tail_is_document_end(&body[index + b"</body>".len()..])
    {
        let mut out = Vec::with_capacity(body.len() + snippet.len());
        out.extend_from_slice(&body[..index]);
        out.extend_from_slice(snippet.as_bytes());
        out.extend_from_slice(&body[index..]);
        return out.into();
    }

    // HTML5 permits omitting `</body>`; such a document still gets the banner.
    let mut out = body.to_vec();
    out.extend_from_slice(snippet.as_bytes());
    out.into()
}

/// Whether everything after a `</body>` is just the document closing out:
/// optional whitespace, an optional `</html>`, optional whitespace.
///
/// This is what separates the real closing tag from the same bytes inside a
/// `<script>` string or a trailing comment, without parsing the document.
fn tail_is_document_end(tail: &[u8]) -> bool {
    let rest = trim_ascii_whitespace(tail);
    let rest = if rest.len() >= 7 && rest[..7].eq_ignore_ascii_case(b"</html>") {
        trim_ascii_whitespace(&rest[7..])
    } else {
        rest
    };
    rest.is_empty()
}

fn trim_ascii_whitespace(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map_or(start, |i| i + 1);
    &bytes[start..end]
}

/// Whether the response is served as a download rather than rendered in place.
///
/// Only the disposition **type** is inspected: `attachment` is a download,
/// `inline` and anything unrecognised are not. A `filename` parameter is
/// irrelevant here and may itself contain the word.
fn is_attachment(response: &Response<Body>) -> bool {
    response
        .headers()
        .get(CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|kind| kind.trim().eq_ignore_ascii_case("attachment"))
        })
}

/// Whether the body opens as a complete document rather than a fragment.
///
/// A document begins with a doctype or an `<html>` tag even when it omits its
/// closing tags; a swap fragment begins with the element being swapped in.
fn starts_like_a_document(body: &[u8]) -> bool {
    // A UTF-8 BOM is an encoding artifact many editors emit; browsers read through it.
    let mut head = body
        .strip_prefix(b"\xEF\xBB\xBF".as_slice())
        .unwrap_or(body);
    // Skip whitespace and leading comments: a generator's `<!-- built at … -->` preamble
    // ahead of the doctype is still a document.
    loop {
        let trimmed = head
            .iter()
            .position(|b| !b.is_ascii_whitespace())
            .map_or(&[][..], |i| &head[i..]);
        let Some(rest) = trimmed.strip_prefix(b"<!--".as_slice()) else {
            head = trimmed;
            break;
        };
        let Some(end) = rfind_first(rest, b"-->") else {
            // An unterminated comment is not something to guess about.
            return false;
        };
        head = &rest[end + 3..];
    }

    let starts_with = |prefix: &[u8]| {
        head.len() >= prefix.len() && head[..prefix.len()].eq_ignore_ascii_case(prefix)
    };
    if starts_with(b"<!doctype") {
        return true;
    }
    // `<html` must be followed by `>` or any ASCII whitespace, so `<html\nlang="en">` counts
    // and `<htmlfoo>` does not.
    starts_with(b"<html")
        && head
            .get(b"<html".len())
            .is_some_and(|b| *b == b'>' || b.is_ascii_whitespace())
}

/// Byte offset of the FIRST occurrence of `needle` in `haystack`.
fn rfind_first(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    (0..=haystack.len() - needle.len()).find(|&i| &haystack[i..i + needle.len()] == needle)
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
    use axum::http::{Method, StatusCode};
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
        // Rejecting must be as easy as accepting: both buttons share the base class.
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
        let out = splice_before_body_close(b"<html><body><main>ok</main></body></html>", "<snip>")
            .expect("a document must be spliceable");
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("<snip></body>"));
    }

    #[test]
    fn splice_appends_when_no_body_tag_but_html_shell_present() {
        let out = splice_before_body_close(b"<html><main>ok</main></html>", "<snip>")
            .expect("a document must be spliceable");
        let s = String::from_utf8(out).unwrap();
        assert!(s.ends_with("<snip>"));
    }

    #[test]
    fn splice_appends_when_no_recognizable_html_wrapper_tag_is_present() {
        // HTML5 allows omitting `<html>` and `<body>`; such a page still gets the banner.
        let out = splice_before_body_close(b"<!doctype html><main>ok</main>", "<snip>")
            .expect("a document must be spliceable");
        let s = String::from_utf8(out).unwrap();
        assert!(s.ends_with("<snip>"), "{s}");
    }

    #[test]
    fn splice_refuses_a_fragment_rather_than_appending_to_it() {
        // Appending to a fragment would put a second banner on the page when htmx swaps it
        // in. A fragment is recognised by its opening, since no request header marks it.
        for fragment in [
            &b"<div id=\"nav-auth\">hi</div>"[..],
            &b"<li>a row</li>"[..],
            &b"  <span>leading whitespace</span>"[..],
        ] {
            assert!(
                splice_before_body_close(fragment, "<snip>").is_none(),
                "a fragment must be left alone: {}",
                String::from_utf8_lossy(fragment)
            );
        }
    }

    #[test]
    fn any_html_whitespace_may_follow_the_opening_tag() {
        for doc in [
            &b"<html>\n<body>hi</body></html>"[..],
            &b"<html lang=\"en\"><body>hi</body></html>"[..],
            &b"<html\nlang=\"en\"><body>hi</body></html>"[..],
            &b"<html\tlang=\"en\"><body>hi</body></html>"[..],
            &b"<html\r\nlang=\"en\"><body>hi</body></html>"[..],
            &b"<HTML\nLANG=\"en\"><BODY>hi</BODY></HTML>"[..],
        ] {
            assert!(
                splice_before_body_close(doc, "<snip>").is_some(),
                "a doctype-less document must still be recognised: {}",
                String::from_utf8_lossy(doc)
            );
        }

        // But the tag name must actually end there — `<htmlfoo>` is not one.
        assert!(splice_before_body_close(b"<htmlfoo><body>x</body>", "<snip>").is_none());
    }

    #[tokio::test]
    async fn a_download_is_never_spliced_into() {
        let app = Router::new()
            .route(
                "/export.html",
                axum::routing::get(|| async {
                    (
                        [
                            (CONTENT_TYPE, "text/html; charset=utf-8"),
                            (CONTENT_DISPOSITION, "attachment; filename=\"export.html\""),
                        ],
                        "<!doctype html><html><body>rows</body></html>",
                    )
                }),
            )
            .layer(axum::middleware::from_fn(move |req, next| async move {
                inject_consent_banner(req, next, 1, None, "_csrf").await
            }));

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/export.html")
                    .header(axum::http::header::ACCEPT, "text/html")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&body),
            "<!doctype html><html><body>rows</body></html>",
            "an exported file must reach the disk exactly as the handler wrote it"
        );
    }

    #[test]
    fn a_body_close_that_is_not_the_document_ending_is_appended_past_not_spliced_into() {
        // The last `</body>` bytes sit in a script string or a trailing comment; splicing there
        // would put the banner inside it.
        for doc in [
            &b"<!doctype html><script>const marker = \"</body>\";</script>"[..],
            &b"<!doctype html><html><body>hi</body></html><!-- </body> -->"[..],
        ] {
            let out = splice_before_body_close(doc, "<snip>").expect("still a document");
            let rendered = String::from_utf8(out).unwrap();
            assert!(
                rendered.ends_with("<snip>"),
                "must append past the false boundary, not splice into it: {rendered}"
            );
            assert!(
                !rendered.contains("<snip>\";</script>") && !rendered.contains("<snip> -->"),
                "the banner must not land inside the script or the comment: {rendered}"
            );
        }
    }

    #[test]
    fn a_real_document_ending_is_still_spliced_before() {
        for doc in [
            &b"<html><body>hi</body></html>"[..],
            &b"<html><body>hi</body></html>\n"[..],
            &b"<html><body>hi</body>\n</html>\n\n"[..],
            &b"<html><body>hi</body>"[..],
            &b"<HTML><BODY>hi</BODY></HTML>"[..],
        ] {
            let rendered =
                String::from_utf8(splice_before_body_close(doc, "<snip>").expect("document"))
                    .unwrap();
            assert!(
                rendered.to_ascii_lowercase().contains("<snip></body>"),
                "a genuine ending must still be spliced before: {rendered}"
            );
        }
    }

    #[test]
    fn a_fragment_that_merely_mentions_body_close_is_still_a_fragment() {
        // Containing `</body>` bytes does not make a body a document.
        for fragment in [
            &b"<div><script>var end = \"</body>\";</script></div>"[..],
            &b"<li><!-- closes with </body> in the source --></li>"[..],
            &b"<pre><code>&lt;/body&gt;</code></pre>"[..],
        ] {
            assert!(
                splice_before_body_close(fragment, "<snip>").is_none(),
                "not a document: {}",
                String::from_utf8_lossy(fragment)
            );
        }
    }

    #[test]
    fn a_utf8_bom_does_not_make_a_document_look_like_a_fragment() {
        for doc in [
            &b"\xEF\xBB\xBF<!doctype html><html><body>hi</body></html>"[..],
            &b"\xEF\xBB\xBF<html><body>hi</body></html>"[..],
            &b"\xEF\xBB\xBF\n  <!-- built -->\n<!doctype html><html><body>hi</body></html>"[..],
        ] {
            let out = splice_before_body_close(doc, "<snip>")
                .expect("a BOM is an encoding artifact, not a fragment marker");
            assert!(String::from_utf8(out).unwrap().contains("<snip></body>"));
        }

        // The BOM must not become a way to smuggle a fragment past the gate.
        assert!(splice_before_body_close(b"\xEF\xBB\xBF<div>row</div>", "<snip>").is_none());
    }

    #[test]
    fn a_document_behind_a_generator_comment_is_still_a_document() {
        let out = splice_before_body_close(
            b"<!-- built at 2026-01-01 -->\n<!doctype html><html><body>hi</body></html>",
            "<snip>",
        )
        .expect("a comment preamble does not make it a fragment");
        assert!(String::from_utf8(out).unwrap().contains("<snip></body>"));
    }

    #[test]
    fn splice_matches_uppercase_and_mixed_case_tags() {
        let out = splice_before_body_close(b"<HTML><BODY><main>ok</main></BODY></HTML>", "<snip>")
            .expect("a document must be spliceable");
        let s = String::from_utf8(out).unwrap();
        assert!(
            s.contains("<snip></BODY>"),
            "an uppercase `</BODY>` is exactly as valid HTML as lowercase: {s}"
        );
    }

    #[test]
    fn splice_appends_when_only_uppercase_html_shell_present() {
        let out = splice_before_body_close(b"<HTML><main>ok</main></HTML>", "<snip>")
            .expect("a document must be spliceable");
        let s = String::from_utf8(out).unwrap();
        assert!(s.ends_with("<snip>"), "{s}");
    }

    #[test]
    fn splice_preserves_non_utf8_bytes_in_a_legacy_charset_document() {
        // 0xE9 is ISO-8859-1 'é' but invalid UTF-8 on its own. Lossy decoding would turn it
        // into U+FFFD; the splice must pass it through byte-for-byte.
        let mut body = b"<html><body>caf\xE9".to_vec();
        body.extend_from_slice(b"</body></html>");
        let out = splice_before_body_close(&body, "<snip>").expect("a document must be spliceable");
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

    /// A bare router with no `CsrfLayer`, so `None` (CSRF disabled) is the
    /// honest posture: there is no token to embed and none is wanted. Tests
    /// that need the *enforced* posture pass `Some(..)` and arrange for a
    /// token, or assert the skip — see
    /// `enforced_csrf_without_a_token_skips_the_banner_rather_than_render_it_broken`.
    fn app_with_policy_version(version: u32) -> Router {
        Router::new()
            .route("/", get(|| async { html_page() }))
            .layer(axum::middleware::from_fn(move |req, next| async move {
                inject_consent_banner(req, next, version, None, "_csrf").await
            }))
    }

    // The gate withholds a non-essential cookie until consent, while the strictly-necessary
    // session cookie is set regardless of consent state.
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
                    inject_consent_banner(req, next, POLICY_VERSION, None, "_csrf").await
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
                inject_consent_banner(req, next, 1, Some("autumn-csrf"), "_csrf").await
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
                inject_consent_banner(req, next, 1, Some("autumn-csrf"), "_csrf").await
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
        // The handler may still render `Consent::allows`-gated markup.
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
        // This middleware runs inside Axum's `Route` wrapper, which computes `Content-Length`
        // from the spliced body and only then empties it for `HEAD`.
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
        // Nothing is injected, so only `Vary: Cookie` is added.
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
        let oversized = format!(
            "<html><body>{}</body></html>",
            "x".repeat(MAX_SPLICE_BODY_BYTES + 1)
        );
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
                inject_consent_banner(req, next, 1, Some("autumn-csrf"), "_csrf").await
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
    async fn enforced_csrf_without_a_token_skips_the_banner_rather_than_render_it_broken() {
        // Models a static cache hit, where `CsrfLayer` never runs: the buttons would `403`.
        let app = Router::new()
            .route("/about", get(|| async { html_page() }))
            .layer(axum::middleware::from_fn(move |req, next| async move {
                // CSRF is enforced, but nothing in this stack issues the cookie.
                inject_consent_banner(req, next, 1, Some("autumn-csrf"), "_csrf").await
            }));

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/about")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let rendered = String::from_utf8_lossy(&body);

        assert!(
            !rendered.contains("autumn-consent-banner"),
            "a banner that cannot submit must not be rendered at all; got: {rendered}"
        );
        assert!(
            rendered.contains("<body>"),
            "the page itself must still be served intact; got: {rendered}"
        );
    }

    #[tokio::test]
    async fn a_csrf_free_app_still_gets_its_banner() {
        // With CSRF disabled (`None`) a missing token is expected, so the skip must not apply.
        let app = app_with_policy_version(1);
        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();

        assert!(
            String::from_utf8_lossy(&body).contains("autumn-consent-banner"),
            "an app with CSRF disabled must still prompt"
        );
    }

    #[tokio::test]
    async fn a_json_client_keeps_its_conditional_validators() {
        // An API client never gets a consent cookie, so it always needs prompting; stripping
        // its validators would cost it every 304.
        let app = Router::new()
            .route(
                "/api/posts",
                get(|headers: axum::http::HeaderMap| async move {
                    let seen = headers.contains_key(IF_NONE_MATCH);
                    Response::builder()
                        .header(CONTENT_TYPE, "application/json")
                        .body(Body::from(format!("{{\"saw_if_none_match\":{seen}}}")))
                        .unwrap()
                }),
            )
            .layer(axum::middleware::from_fn(move |req, next| async move {
                inject_consent_banner(req, next, 1, Some("autumn-csrf"), "_csrf").await
            }));

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/posts")
                    .header(axum::http::header::ACCEPT, "application/json")
                    .header(IF_NONE_MATCH, "\"abc\"")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();

        assert_eq!(
            String::from_utf8_lossy(&body),
            "{\"saw_if_none_match\":true}",
            "a non-HTML client must keep If-None-Match so its ETag path still 304s"
        );
    }

    #[tokio::test]
    async fn an_html_navigation_still_loses_its_validators_while_prompting() {
        // A browser navigation must still bypass a cached 304, or the banner-less page replays.
        let app = Router::new()
            .route(
                "/",
                get(|headers: axum::http::HeaderMap| async move {
                    let seen = headers.contains_key(IF_NONE_MATCH);
                    Response::builder()
                        .header(CONTENT_TYPE, "text/html; charset=utf-8")
                        .body(Body::from(format!("<html><body>{seen}</body></html>")))
                        .unwrap()
                }),
            )
            .layer(axum::middleware::from_fn(move |req, next| async move {
                inject_consent_banner(req, next, 1, None, "_csrf").await
            }));

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header(
                        axum::http::header::ACCEPT,
                        "text/html,application/xhtml+xml",
                    )
                    .header(IF_NONE_MATCH, "\"abc\"")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let rendered = String::from_utf8_lossy(&body);

        assert!(
            rendered.contains("false"),
            "an HTML navigation being prompted must have If-None-Match stripped; got: {rendered}"
        );
        assert!(
            rendered.contains("autumn-consent-banner"),
            "and it must still be prompted; got: {rendered}"
        );
    }

    #[tokio::test]
    async fn an_htmx_fragment_never_receives_a_banner() {
        // htmx would swap an appended banner in next to the one the page already has.
        let app = Router::new()
            .route(
                "/_partials/nav-auth",
                get(|| async {
                    Response::builder()
                        .header(CONTENT_TYPE, "text/html; charset=utf-8")
                        .body(Body::from("<div id=\"nav\">Log in</div>"))
                        .unwrap()
                }),
            )
            .layer(axum::middleware::from_fn(move |req, next| async move {
                inject_consent_banner(req, next, 1, Some("autumn-csrf"), "_csrf").await
            }));

        // No consent cookie, so only the fragment check keeps the banner out.
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/_partials/nav-auth")
                    .header("hx-request", "true")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let rendered = String::from_utf8_lossy(&body);

        assert!(
            !rendered.contains("autumn-consent-banner"),
            "an htmx fragment must never carry the banner; got: {rendered}"
        );
        assert_eq!(
            rendered, "<div id=\"nav\">Log in</div>",
            "the fragment must be passed through byte-for-byte"
        );
    }

    /// Every HTML response this middleware passes through must carry
    /// `Vary: Cookie`, whether or not a banner was injected — its
    /// representation depends on the consent cookie either way.
    ///
    /// Written as a table rather than one test per branch on purpose: the three
    /// `Vary` bugs found in review (fragments, `Content-Encoding` bodies, and
    /// the no-CSRF-token skip) were each a *new* pass-through added without
    /// revisiting the others. A case added here fails until its branch appends
    /// the header.
    #[tokio::test]
    async fn every_html_pass_through_varies_on_cookie() {
        struct Case {
            name: &'static str,
            csrf_cookie_name: Option<&'static str>,
            request: fn(axum::http::request::Builder) -> axum::http::request::Builder,
            encoded: bool,
        }

        let cases = [
            Case {
                name: "htmx fragment",
                csrf_cookie_name: None,
                request: |b| b.header("hx-request", "true"),
                encoded: false,
            },
            Case {
                name: "compressed HTML",
                csrf_cookie_name: None,
                request: |b| b,
                encoded: true,
            },
            Case {
                name: "CSRF enforced but no token obtainable (static hit)",
                csrf_cookie_name: Some("autumn-csrf"),
                request: |b| b,
                encoded: false,
            },
        ];

        for case in cases {
            let encoded = case.encoded;
            let name = case.csrf_cookie_name;
            let app = Router::new()
                .route(
                    "/",
                    get(move || async move {
                        let mut builder = Response::builder()
                            .header(CONTENT_TYPE, "text/html; charset=utf-8")
                            .header(CACHE_CONTROL, "public, max-age=60");
                        if encoded {
                            builder = builder.header(CONTENT_ENCODING, "gzip");
                        }
                        builder
                            .body(Body::from("<html><body>hi</body></html>"))
                            .unwrap()
                    }),
                )
                .layer(axum::middleware::from_fn(move |req, next| async move {
                    inject_consent_banner(req, next, 1, name, "_csrf").await
                }));

            let response = app
                .oneshot(
                    (case.request)(Request::builder().uri("/"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();

            let vary: Vec<_> = response
                .headers()
                .get_all(VARY)
                .iter()
                .filter_map(|v| v.to_str().ok())
                .collect();
            assert!(
                vary.iter().any(|v| v.eq_ignore_ascii_case("Cookie")),
                "{} must vary on Cookie; got: {vary:?}",
                case.name
            );
        }
    }

    #[tokio::test]
    async fn a_compressed_html_page_still_varies_on_cookie() {
        // Cannot be spliced, but its markup can still depend on `Consent::allows`.
        let app = Router::new()
            .route(
                "/",
                get(|| async {
                    Response::builder()
                        .header(CONTENT_TYPE, "text/html; charset=utf-8")
                        .header(CONTENT_ENCODING, "gzip")
                        .header(CACHE_CONTROL, "public, max-age=60")
                        .body(Body::from("<html><body>compressed</body></html>"))
                        .unwrap()
                }),
            )
            .layer(axum::middleware::from_fn(move |req, next| async move {
                inject_consent_banner(req, next, 1, None, "_csrf").await
            }));

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        let vary: Vec<_> = response
            .headers()
            .get_all(VARY)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect();
        assert!(
            vary.iter().any(|v| v.eq_ignore_ascii_case("Cookie")),
            "compressed HTML must still vary on Cookie; got: {vary:?}"
        );

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&body),
            "<html><body>compressed</body></html>",
            "and must be passed through unspliced"
        );
    }

    #[tokio::test]
    async fn a_json_response_does_not_gain_a_cookie_vary() {
        // A JSON API's cacheability is not this middleware's business.
        let app = Router::new()
            .route(
                "/api",
                get(|| async {
                    Response::builder()
                        .header(CONTENT_TYPE, "application/json")
                        .body(Body::from("{}"))
                        .unwrap()
                }),
            )
            .layer(axum::middleware::from_fn(move |req, next| async move {
                inject_consent_banner(req, next, 1, None, "_csrf").await
            }));

        let response = app
            .oneshot(Request::builder().uri("/api").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert!(
            response.headers().get_all(VARY).iter().next().is_none(),
            "a JSON response must be left alone"
        );
    }

    #[tokio::test]
    async fn an_htmx_fragment_still_varies_on_cookie() {
        // No banner, but the handler may have gated markup on `Consent::allows`.
        let app = Router::new()
            .route(
                "/_partials/nav-auth",
                get(|| async {
                    Response::builder()
                        .header(CONTENT_TYPE, "text/html; charset=utf-8")
                        .header(CACHE_CONTROL, "public, max-age=60")
                        .body(Body::from("<div>Log in</div>"))
                        .unwrap()
                }),
            )
            .layer(axum::middleware::from_fn(move |req, next| async move {
                inject_consent_banner(req, next, 1, None, "_csrf").await
            }));

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/_partials/nav-auth")
                    .header("hx-request", "true")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let vary: Vec<_> = response
            .headers()
            .get_all(VARY)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect();
        assert!(
            vary.iter().any(|v| v.eq_ignore_ascii_case("Cookie")),
            "a fragment must still vary on Cookie; got: {vary:?}"
        );

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(
            !String::from_utf8_lossy(&body).contains("autumn-consent-banner"),
            "and must still carry no banner"
        );
    }

    #[tokio::test]
    async fn a_boosted_navigation_loses_its_validators_despite_the_xhr_accept_default() {
        // htmx sends `Accept: */*`, which `accepts_html` rejects, yet a boosted request is a
        // document. With its validators intact an inner `EtagLayer` could answer a bodyless
        // 304 and skip the prompt.
        let app = Router::new()
            .route(
                "/",
                get(|headers: axum::http::HeaderMap| async move {
                    let seen = headers.contains_key(IF_NONE_MATCH);
                    Response::builder()
                        .header(CONTENT_TYPE, "text/html; charset=utf-8")
                        .body(Body::from(format!("<html><body>{seen}</body></html>")))
                        .unwrap()
                }),
            )
            .layer(axum::middleware::from_fn(move |req, next| async move {
                inject_consent_banner(req, next, 1, None, "_csrf").await
            }));

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header("hx-request", "true")
                    .header("hx-boosted", "true")
                    .header(axum::http::header::ACCEPT, "*/*")
                    .header(IF_NONE_MATCH, "\"abc\"")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let rendered = String::from_utf8_lossy(&body);

        assert!(
            rendered.contains("false"),
            "a boosted navigation must have If-None-Match stripped; got: {rendered}"
        );
        assert!(
            rendered.contains("autumn-consent-banner"),
            "and must still be prompted; got: {rendered}"
        );
    }

    #[test]
    fn an_explicit_zero_quality_is_a_refusal_of_html() {
        let accept = |v: &str| {
            let mut h = axum::http::HeaderMap::new();
            h.insert(
                axum::http::header::ACCEPT,
                HeaderValue::from_str(v).unwrap(),
            );
            accepts_html(&h)
        };

        // `text/*` is a range that includes HTML — a document client.
        assert!(accept("text/*"));
        assert!(accept("application/json, text/*;q=0.8"));
        // ...and the same refusal rule applies to it.
        assert!(!accept("text/*;q=0"));
        // `*/*` is the XHR default, not a document offer.
        assert!(!accept("*/*"));

        // Named, but refused.
        assert!(!accept("application/json, text/html;q=0"));
        assert!(!accept("text/html;q=0.0"));
        assert!(!accept("text/html; q=0.000"));

        // Any non-zero quality is accepted: this is a zero check, not a q-ranking.
        assert!(accept("text/html"));
        assert!(accept("text/html;q=0.1"));
        assert!(accept("text/html;q=0.9, application/json"));
        assert!(accept("application/json, text/html"));
        assert!(!accept("*/*"));
    }

    /// A `206` body is a slice of a larger document, and `Content-Range` says
    /// which bytes. Splicing would insert markup into that slice and rewrite
    /// `Content-Length` while `Content-Range` still described the original, so
    /// a range cache or a resumed download would reassemble something corrupt.
    ///
    /// The trap is that a first range of an HTML file passes the document test
    /// perfectly well — it opens `<!doctype html>` — so nothing further down
    /// would have caught it.
    #[tokio::test]
    async fn a_ranged_response_is_never_spliced_into() {
        let app = Router::new()
            .route(
                "/page.html",
                axum::routing::get(|| async {
                    (
                        axum::http::StatusCode::PARTIAL_CONTENT,
                        [
                            (CONTENT_TYPE, "text/html; charset=utf-8"),
                            (CONTENT_RANGE, "bytes 0-31/512"),
                        ],
                        "<!doctype html><html><body>part",
                    )
                }),
            )
            .layer(axum::middleware::from_fn(move |req, next| async move {
                inject_consent_banner(req, next, 1, None, "_csrf").await
            }));

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/page.html")
                    .header(axum::http::header::ACCEPT, "text/html")
                    .header(axum::http::header::RANGE, "bytes=0-31")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert!(
            response
                .headers()
                .get_all(VARY)
                .iter()
                .any(|v| v.as_bytes().eq_ignore_ascii_case(b"Cookie")),
            "a ranged HTML response still varies on the consent cookie"
        );

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let rendered = String::from_utf8_lossy(&body);
        assert_eq!(
            rendered, "<!doctype html><html><body>part",
            "the advertised byte range must be served byte-for-byte"
        );
    }

    /// A pre-rendered `HEAD` hit arrives here already bodyless, and no Axum
    /// per-route wrapper is left to fix up what we return — so splicing would
    /// hand a `HEAD` a body and a `Content-Length` of the banner rather than of
    /// the equivalent `GET`.
    #[tokio::test]
    async fn an_already_empty_html_response_is_not_spliced_into() {
        let app = Router::new()
            .route(
                "/",
                // Stands in for the static-first middleware's `is_head` branch:
                // `text/html`, empty body, produced without Axum wrapping a
                // handler whose body it will later empty.
                axum::routing::get(|| async {
                    ([(CONTENT_TYPE, "text/html; charset=utf-8")], Body::empty())
                }),
            )
            .layer(axum::middleware::from_fn(move |req, next| async move {
                inject_consent_banner(req, next, 1, None, "_csrf").await
            }));

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert!(
            response
                .headers()
                .get_all(VARY)
                .iter()
                .any(|v| v.as_bytes().eq_ignore_ascii_case(b"Cookie")),
            "it is still a consent-dependent representation"
        );

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(
            body.is_empty(),
            "an empty HTML response must stay empty; got {} bytes",
            body.len()
        );
    }

    /// htmx 2.0.4 sends this on a history-cache miss with `HX-Request: true` and
    /// no `HX-Boosted`, then parses the reply as a full document and replaces
    /// `[hx-history-elt]` — normally the body — from it.
    #[tokio::test]
    async fn an_htmx_history_restore_is_a_document_and_is_prompted() {
        let app = Router::new()
            .route(
                "/",
                axum::routing::get(|| async {
                    axum::response::Html("<html><body>hi</body></html>")
                }),
            )
            .layer(axum::middleware::from_fn(move |req, next| async move {
                inject_consent_banner(req, next, 1, None, "_csrf").await
            }));

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header("hx-request", "true")
                    .header("hx-history-restore-request", "true")
                    .header(axum::http::header::ACCEPT, "*/*")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let rendered = String::from_utf8_lossy(&body);

        assert!(
            rendered.contains("autumn-consent-banner"),
            "a history restore replaces the body from a full document, so the \
             visitor must still be prompted; got: {rendered}"
        );
    }

    /// The three shapes an htmx whole-document swap takes, each named as a
    /// literal. The third — an ordinary `hx-get` with `hx-target="body"` — is
    /// why this is no longer decided from request headers: htmx sends
    /// `HX-Request` and nothing else for it (`HX-Target` is omitted when the
    /// target has no id), so it is indistinguishable from a fragment on the
    /// request side. `examples/todo-app` paginates exactly this way.
    ///
    /// All three are recognised by their response carrying `</body>`, and all
    /// three must lose their conditional validators while a prompt is due.
    #[tokio::test]
    async fn every_whole_document_htmx_swap_is_prompted_and_loses_its_validators() {
        for extra in [
            Some("hx-boosted"),
            Some("hx-history-restore-request"),
            None, // plain `hx-get` with `hx-target="body"` — no marker header
        ] {
            let app = Router::new()
                .route(
                    "/",
                    axum::routing::get(|req: axum::extract::Request| async move {
                        let had_validator = req.headers().contains_key(IF_NONE_MATCH);
                        axum::response::Html(format!(
                            "<html><body>had_validator={had_validator}</body></html>"
                        ))
                    }),
                )
                .layer(axum::middleware::from_fn(move |req, next| async move {
                    inject_consent_banner(req, next, 1, None, "_csrf").await
                }));

            let mut builder = Request::builder()
                .uri("/")
                .header("hx-request", "true")
                // htmx swaps run over XHR, whose default `Accept` is `*/*`.
                .header(axum::http::header::ACCEPT, "*/*")
                .header(IF_NONE_MATCH, "\"abc\"");
            if let Some(name) = extra {
                builder = builder.header(name, "true");
            }

            let response = app
                .oneshot(builder.body(Body::empty()).unwrap())
                .await
                .unwrap();
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let rendered = String::from_utf8_lossy(&body);
            let label = extra.unwrap_or("hx-target=body (no marker header)");

            assert!(
                rendered.contains("had_validator=false"),
                "`{label}` may replace the document, so its validators must be \
                 stripped while a prompt is due; got: {rendered}"
            );
            assert!(
                rendered.contains("autumn-consent-banner"),
                "`{label}` returned a complete document and must be prompted; \
                 got: {rendered}"
            );
        }
    }

    #[tokio::test]
    async fn a_plain_xhr_json_client_still_keeps_its_validators() {
        // A `*/*` request without `HX-Request` is an API client and keeps its 304s.
        let app = Router::new()
            .route(
                "/api/posts",
                get(|headers: axum::http::HeaderMap| async move {
                    let seen = headers.contains_key(IF_NONE_MATCH);
                    Response::builder()
                        .header(CONTENT_TYPE, "application/json")
                        .body(Body::from(format!("{{\"saw\":{seen}}}")))
                        .unwrap()
                }),
            )
            .layer(axum::middleware::from_fn(move |req, next| async move {
                inject_consent_banner(req, next, 1, None, "_csrf").await
            }));

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/posts")
                    .header(axum::http::header::ACCEPT, "*/*")
                    .header(IF_NONE_MATCH, "\"abc\"")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();

        assert_eq!(
            String::from_utf8_lossy(&body),
            "{\"saw\":true}",
            "a bare */* XHR that is not boosted must keep its validators"
        );
    }

    #[tokio::test]
    async fn a_boosted_navigation_is_a_document_and_still_gets_the_banner() {
        // A boosted response replaces the whole body, so treating it as a fragment would
        // drop the banner from the new page and remove the old page's copy.
        let app = Router::new()
            .route("/", get(|| async { html_page() }))
            .layer(axum::middleware::from_fn(move |req, next| async move {
                inject_consent_banner(req, next, 1, None, "_csrf").await
            }));

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header("hx-request", "true")
                    .header("hx-boosted", "true")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();

        assert!(
            String::from_utf8_lossy(&body).contains("autumn-consent-banner"),
            "a boosted navigation is a full document and must still be prompted"
        );
    }

    #[tokio::test]
    async fn a_full_document_still_receives_the_banner_without_the_htmx_header() {
        let app = Router::new()
            .route(
                "/",
                get(|| async {
                    Response::builder()
                        .header(CONTENT_TYPE, "text/html; charset=utf-8")
                        .body(Body::from("<html><body><h1>Hi</h1></body></html>"))
                        .unwrap()
                }),
            )
            .layer(axum::middleware::from_fn(move |req, next| async move {
                inject_consent_banner(req, next, 1, None, "_csrf").await
            }));

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();

        assert!(
            String::from_utf8_lossy(&body).contains("autumn-consent-banner"),
            "an ordinary page load must still be prompted"
        );
    }

    #[tokio::test]
    async fn oversized_body_still_varies_on_cookie() {
        // No banner is spliced, but the handler may still gate markup on the consent cookie.
        let oversized = format!(
            "<html><body>{}</body></html>",
            "x".repeat(MAX_SPLICE_BODY_BYTES + 1)
        );
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
                inject_consent_banner(req, next, 1, None, "_csrf").await
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
        // A "manage cookie preferences" page renders the banner itself; an undecided visitor
        // there must not get a second copy.
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
                inject_consent_banner(req, next, 1, Some("autumn-csrf"), "_csrf").await
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
    async fn stamps_cache_guards_when_handler_already_rendered_the_banner() {
        // Nothing is spliced, but the handler's banner carries a live CSRF token, so the
        // injection path's cache guards still apply.
        let app = Router::new()
            .route(
                "/",
                get(|| async {
                    let banner = consent_banner_markup(Some("tok"), "_csrf").into_string();
                    Response::builder()
                        .header(CONTENT_TYPE, "text/html; charset=utf-8")
                        .body(Body::from(format!("<html><body>{banner}</body></html>")))
                        .unwrap()
                }),
            )
            .layer(axum::middleware::from_fn(move |req, next| async move {
                inject_consent_banner(req, next, 1, None, "_csrf").await
            }));
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
            "an already-rendered banner still carries a live CSRF token and must not be cached"
        );
        assert_eq!(
            response.headers().get(VARY).and_then(|v| v.to_str().ok()),
            Some("Cookie"),
            "a shared cache must vary on the visitor's consent/CSRF cookie"
        );
    }

    #[tokio::test]
    async fn still_injects_when_page_merely_mentions_the_banner_class_in_prose() {
        // Prose naming the class is not a rendered banner and must not suppress the prompt.
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
                inject_consent_banner(req, next, 1, None, "_csrf").await
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
        // A clean empty `200` would pass a body failure off as a blank page.
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
                inject_consent_banner(req, next, 1, Some("my-csrf"), "_csrf").await
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
        // `CsrfLayer` sits inside user layers, so its `CsrfFormField` extension is not visible
        // here: the field name has to come in as a parameter.
        let app = Router::new()
            .route("/", get(|| async { html_page() }))
            .layer(axum::middleware::from_fn(move |req, next| async move {
                inject_consent_banner(req, next, 1, Some("autumn-csrf"), "authenticity_token").await
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
                inject_consent_banner(req, next, 1, Some("autumn-csrf"), "_csrf").await
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
                inject_consent_banner(req, next, 1, Some("autumn-csrf"), "_csrf").await
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
        // An `autumn build` / ISR render has no visitor; a banner here would be baked into the
        // static file in `dist/` and shown to every visitor, consented or not. Live requests
        // never carry the marker.
        let app = Router::new()
            .route("/", get(|| async { html_page() }))
            .layer(axum::middleware::from_fn(move |req, next| async move {
                inject_consent_banner(req, next, 1, Some("autumn-csrf"), "_csrf").await
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

    // ── representation metadata and multi-line Accept ────────────────

    #[tokio::test]
    async fn a_split_accept_header_still_counts_as_offering_html() {
        // RFC 9110 §5.3: several field lines form one comma-joined list, and
        // `HeaderMap::get` sees only the first.
        let mut headers = axum::http::HeaderMap::new();
        headers.append(
            axum::http::header::ACCEPT,
            "application/json".parse().unwrap(),
        );
        headers.append(axum::http::header::ACCEPT, "text/html".parse().unwrap());
        assert!(
            accepts_html(&headers),
            "HTML offered on the second Accept field line must still count"
        );

        // `*/*` on a later line is still not a document offer.
        let mut api = axum::http::HeaderMap::new();
        api.append(
            axum::http::header::ACCEPT,
            "application/json".parse().unwrap(),
        );
        api.append(axum::http::header::ACCEPT, "*/*".parse().unwrap());
        assert!(
            !accepts_html(&api),
            "an API client splitting its Accept must keep its validators"
        );

        // And a refusal spread across lines is still a refusal.
        let mut refused = axum::http::HeaderMap::new();
        refused.append(
            axum::http::header::ACCEPT,
            "application/json".parse().unwrap(),
        );
        refused.append(axum::http::header::ACCEPT, "text/html;q=0".parse().unwrap());
        assert!(
            !accepts_html(&refused),
            "`text/html;q=0` on a later line is an explicit refusal"
        );

        assert!(
            accepts_html(&axum::http::HeaderMap::new()),
            "a missing Accept still means yes"
        );
    }

    #[tokio::test]
    async fn a_specific_html_refusal_overrides_a_text_wildcard_offer() {
        // RFC 9110 §12.5.1: the most specific matching media range decides.
        let refuse = |v: &str| {
            let mut h = axum::http::HeaderMap::new();
            h.append(axum::http::header::ACCEPT, v.parse().unwrap());
            accepts_html(&h)
        };

        assert!(
            !refuse("text/*;q=1, text/html;q=0"),
            "an exact `text/html;q=0` must override a `text/*` offer"
        );
        assert!(
            !refuse("text/html;q=0, text/*;q=1"),
            "order must not matter -- specificity decides, not position"
        );

        // The precedence must cut the other way too: a specific ACCEPTANCE
        // overrides a wildcard refusal, or this becomes "any q=0 anywhere wins".
        assert!(
            refuse("text/*;q=0, text/html"),
            "an exact `text/html` offer must override a `text/*;q=0` refusal"
        );

        // And a wildcard alone still decides when html is never named.
        assert!(refuse("text/*"), "`text/*` alone still offers html");
        assert!(!refuse("text/*;q=0"), "`text/*;q=0` alone still refuses");

        // Precedence must survive the split-field-line path as well.
        let mut split = axum::http::HeaderMap::new();
        split.append(axum::http::header::ACCEPT, "text/*;q=1".parse().unwrap());
        split.append(axum::http::header::ACCEPT, "text/html;q=0".parse().unwrap());
        assert!(
            !accepts_html(&split),
            "the specific refusal wins even when it arrives on a later line"
        );
    }

    #[tokio::test]
    async fn splicing_the_banner_drops_a_now_wrong_etag_and_digest() {
        // The handler stands in for an inner `EtagLayer` labelling the pre-splice bytes.
        let app = Router::new()
            .route(
                "/",
                get(|| async {
                    Response::builder()
                        .header(CONTENT_TYPE, "text/html; charset=utf-8")
                        .header(axum::http::header::ETAG, "\"pre-splice\"")
                        .header("content-digest", "sha-256=:deadbeef:")
                        .header("digest", "SHA-256=deadbeef")
                        .header(axum::http::header::ACCEPT_RANGES, "bytes")
                        .body(Body::from("<html><body>hi</body></html>"))
                        .unwrap()
                }),
            )
            // `None`: with CSRF enforced and no cookie, nothing would be injected.
            .layer(axum::middleware::from_fn(move |req, next| async move {
                inject_consent_banner(req, next, 1, None, "_csrf").await
            }));

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header(axum::http::header::ACCEPT, "text/html")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let headers = response.headers().clone();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();

        assert!(
            html.contains("autumn-consent-banner"),
            "precondition: this request must actually get a banner: {html}"
        );
        assert!(
            !headers.contains_key(axum::http::header::ETAG),
            "a validator for the pre-splice bytes must not survive the splice"
        );
        assert!(
            !headers.contains_key("content-digest"),
            "Content-Digest describes bytes that are no longer being sent"
        );
        assert!(
            !headers.contains_key("digest"),
            "the obsolete Digest spelling must go too"
        );
        assert!(
            !headers.contains_key(axum::http::header::ACCEPT_RANGES),
            "a range offer over bytes this middleware rewrote must be withdrawn: \
             a resumed 206 comes from the handler and addresses the original, \
             bannerless representation"
        );
        assert_eq!(
            headers
                .get(CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<usize>().ok()),
            Some(html.len()),
            "Content-Length must match the spliced body"
        );
    }

    #[tokio::test]
    async fn a_response_that_is_not_rewritten_keeps_its_etag() {
        // Nothing is spliced, so the inner layer's validator is still accurate.
        let app = Router::new()
            .route(
                "/data.json",
                get(|| async {
                    Response::builder()
                        .header(CONTENT_TYPE, "application/json")
                        .header(axum::http::header::ETAG, "\"kept\"")
                        .header(axum::http::header::ACCEPT_RANGES, "bytes")
                        .body(Body::from("{}"))
                        .unwrap()
                }),
            )
            .layer(axum::middleware::from_fn(move |req, next| async move {
                inject_consent_banner(req, next, 1, Some("autumn-csrf"), "_csrf").await
            }));

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/data.json")
                    .header(axum::http::header::ACCEPT, "application/json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::ETAG)
                .and_then(|v| v.to_str().ok()),
            Some("\"kept\""),
            "an untouched body keeps its validator"
        );
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::ACCEPT_RANGES)
                .and_then(|v| v.to_str().ok()),
            Some("bytes"),
            "and keeps its range offer -- those bytes are still what ships"
        );
    }
}
