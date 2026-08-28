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
    CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE,
    CONTENT_TYPE, IF_MODIFIED_SINCE, IF_NONE_MATCH, SET_COOKIE, VARY,
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

    // An htmx *fragment* request asks for a piece of a page, not a document.
    // Such a response carries no `</body>`, so the splice below would append
    // the banner to the end of the fragment — and htmx would then swap that
    // copy into whatever element it targets, while the banner already injected
    // into the enclosing page stays put. The visitor sees duplicate consent
    // controls, and on a page whose fragments refresh (a live feed, a vote
    // button) a new copy on every swap. A fragment is never the right place to
    // prompt, so skip it: the enclosing document already carries the banner,
    // and the next full page load re-evaluates consent as usual.
    //
    // A *boosted* navigation (`hx-boost`) is deliberately NOT treated as a
    // fragment. It also carries `HX-Request`, but the response is a complete
    // document whose body replaces the current one — so skipping it would both
    // omit the banner from the new page and destroy the one on the old page in
    // the same swap, leaving an undecided visitor silently unprompted until
    // they made a non-htmx navigation.
    //
    // Note this only skips the *banner*. A fragment still gets `Vary: Cookie`
    // below: skipping injection is about where a prompt belongs, not about
    // cache correctness, and a fragment handler may itself render
    // consent-dependent markup via `Consent::allows`.

    let consent = Consent::from_headers(request.headers());
    let request_csrf_cookie =
        csrf_cookie_name.and_then(|name| find_cookie(request.headers(), name));

    let needs_prompt = consent.needs_prompt(policy_version);
    // Strip the conditional validators only for a client that could actually
    // be shown a banner. The reason to strip them is that an inner `EtagLayer`
    // could otherwise answer `304` with no body, replaying a browser-cached,
    // banner-less page and skipping a prompt that is now due.
    //
    // That reasoning applies only to responses this middleware could inject
    // into, and the check has to happen here — before `next` runs — where the
    // response's content type is not yet known. `Accept` normally
    // distinguishes the two cases: a browser navigation offers `text/html`,
    // while an API client asks for JSON. Stripping unconditionally would mean
    // every request from a consent-less client loses `If-None-Match` /
    // `If-Modified-Since` — and an API client never acquires a consent cookie,
    // so its conditional requests could never return `304` again and it would
    // refetch full bodies forever.
    //
    // htmx requests are the exception `Accept` alone gets wrong: they go over
    // XHR without setting `Accept`, so they arrive with the XHR default `*/*`,
    // which this deliberately does not treat as HTML. Any of them may still be
    // a whole-document swap (see `could_render_a_banner`). Leaving their
    // validators intact would let an inner `EtagLayer` answer `304` with no
    // body to inject, so a policy bump or a withdrawal could leave the visitor
    // unprompted for as long as their cache stays fresh.
    if needs_prompt && could_render_a_banner(request.headers()) {
        request.headers_mut().remove(IF_NONE_MATCH);
        request.headers_mut().remove(IF_MODIFIED_SINCE);
    }

    let mut response = next.run(request).await;

    if !is_html_content_type(&response) {
        return response;
    }

    // A `206 Partial Content` body is a byte slice of a larger representation,
    // and `Content-Range` states which bytes. Splicing would insert markup into
    // the middle of that range and rewrite `Content-Length` while leaving
    // `Content-Range` describing the original — so a range cache or a resuming
    // download would reassemble a corrupted document. The selected bytes can
    // easily satisfy the document test too, since a first range of an HTML file
    // begins exactly like one.
    //
    // Checked by header as well as status: a proxy or a handler can attach
    // `Content-Range` without the canonical status.
    // `Content-Disposition: attachment` means these bytes become a file on the
    // visitor's disk, not a page in their browser. Splicing would edit the
    // export they asked for and write their live CSRF token into it — a token
    // that then sits in a saved file, possibly shared. An exported document is
    // also never where a consent prompt belongs.
    if is_attachment(&response)
        || response.status() == axum::http::StatusCode::PARTIAL_CONTENT
        || response.headers().contains_key(CONTENT_RANGE)
    {
        response
            .headers_mut()
            .append(VARY, HeaderValue::from_static("Cookie"));
        return response;
    }

    // On a route Axum wraps, `HEAD` needs no special handling here: Axum's
    // per-route wrapper computes `Content-Length` from *this* middleware's
    // returned body and empties it strictly afterwards, so splicing here is
    // what makes the `HEAD` metadata match the equivalent `GET`. The test
    // `undecided_visitors_head_request_matches_the_spliced_gets_content_length_and_cache_control`
    // pins that.
    //
    // The pre-rendered path is the exception, and it is caught below by the
    // empty-body check rather than by the method: with a `dist` manifest the
    // static-first middleware answers a `HEAD` hit itself with `Body::empty()`
    // and this layer is reapplied *outside* it, so no wrapper will fix up what
    // we return.
    // A fragment never carries the banner, but it may well carry markup the
    // handler gated on `Consent::allows` — an analytics snippet, a
    // personalised control. Without `Vary: Cookie` a shared cache could store
    // one visitor's fragment and replay it to a visitor whose consent differs,
    // serving gated markup to someone who rejected it. Skipping the banner is
    // correct; skipping the cache variance is not.
    // Everything below here is an HTML response, so its representation can
    // depend on the consent cookie whether or not this middleware injects
    // anything. Two cases get the cache variance and nothing else:
    //
    //   * a `Content-Encoding` body — HTML we cannot splice into without
    //     decoding it, but HTML all the same. Since we already know the media
    //     type is HTML, `!is_html_response` here means exactly "encoded".
    //
    // A fragment is no longer detected here: it is recognised further down by
    // having no `</body>`, which is the property that actually matters and the
    // only one that does not depend on guessing from request headers.
    if !is_html_response(&response) {
        response
            .headers_mut()
            .append(VARY, HeaderValue::from_static("Cookie"));
        return response;
    }

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

    let csrf_token = csrf_cookie_name
        .and_then(|name| extract_response_csrf_cookie(&response, name))
        .or(request_csrf_cookie);

    // With CSRF enforced but no token obtainable, the banner's buttons would
    // POST to a CSRF-protected route without the hidden field and get a `403`.
    // Rendering controls that cannot work is worse than rendering none: the
    // visitor is invited to decide and then silently refused.
    //
    // This is reachable in a built (SSG/ISG) deployment. User `.layer()`
    // middleware is applied OUTSIDE the static-first layer precisely so it can
    // process pre-rendered responses — which means that on a static cache hit
    // `CsrfLayer` never runs, sets no cookie, and there is nothing to embed.
    // Skipping leaves the visitor unprompted on that page; the next dynamic
    // page prompts them normally, and no non-essential cookie was set in the
    // meantime because the gate stays closed while undecided.
    if csrf_cookie_name.is_some() && csrf_token.is_none() {
        tracing::debug!(
            "consent banner skipped: CSRF is enforced but no token was available \
             (typically a pre-rendered static page, where CsrfLayer never runs)"
        );
        // `Vary: Cookie` for the same reason as every other HTML pass-through:
        // this response is bannerless *because this visitor had no CSRF
        // cookie*. Cached without the header, a CDN could replay it to a
        // visitor who does have one — and who could therefore have been
        // prompted — suppressing the banner for as long as the entry stays
        // fresh. The response varies on the cookie jar whether or not anything
        // was injected.
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
    // Any htmx request counts, deliberately, because no header distinguishes a
    // fragment swap from a whole-document one. `hx-boost` and a history-cache
    // miss have their own headers; an ordinary `hx-get` with `hx-target="body"`
    // has none (htmx sends `HX-Target` only when the target has an id, and the
    // body usually has none) yet replaces the document just the same. Three
    // attempts at enumerating the document cases from headers each missed one.
    //
    // htmx also issues these over XHR, whose default `Accept` is `*/*`, so
    // `accepts_html` says no for precisely the requests that most need a
    // prompt. The cost of being generous here is bounded and falls only on
    // htmx clients: one full response instead of a `304`, while a prompt is
    // due. An API client sends neither `text/html` nor `HX-Request` and keeps
    // its conditional requests — which is the property this predicate exists
    // to protect.
    accepts_html(headers) || htmx_header_is_true(headers, "hx-request")
}

/// An htmx boolean header: present and not the literal `false` htmx never sends.
fn htmx_header_is_true(headers: &axum::http::HeaderMap, name: &str) -> bool {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| !value.eq_ignore_ascii_case("false"))
}

/// Whether the client offered to accept an HTML document.
fn accepts_html(headers: &axum::http::HeaderMap) -> bool {
    headers.get(axum::http::header::ACCEPT).is_none_or(|value| {
        value.to_str().is_ok_and(|accept| {
            accept.split(',').any(|range| {
                let mut parts = range.split(';').map(str::trim);
                let Some(media) = parts.next() else {
                    return false;
                };
                // `text/*` is a valid range that includes HTML, and a client
                // sending it is a document client. `*/*` stays excluded
                // deliberately — it is the XHR default, so treating it as a
                // document offer would strip every API client's conditional
                // validators. That carve-out is about `*/*` specifically, not
                // about wildcards in general, which is what this missed.
                if !media.eq_ignore_ascii_case("text/html") && !media.eq_ignore_ascii_case("text/*")
                {
                    return false;
                }
                // `text/html;q=0` is an explicit refusal, not an offer. A
                // substring check reads it as acceptance and then strips the
                // client's conditional validators on every request — so an API
                // client that names HTML only to reject it would never get a
                // `304` again. Anything that is not a zero quality (including a
                // malformed one) is treated as accepted, matching the lenient
                // spirit of the rest of this predicate.
                !parts.any(|param| {
                    param
                        .split_once('=')
                        .is_some_and(|(k, v)| k.eq_ignore_ascii_case("q") && is_zero_quality(v))
                })
            })
        })
    })
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
    // A `Content-Encoding` body is HTML we cannot splice into without decoding
    // it first, so it is not injectable — but it is still HTML, and still
    // varies on the consent cookie. See `is_html_content_type`.
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

            // An already-empty HTML body is not a page to prompt in, and
            // appending a bare banner to one produces a document that is
            // nothing but a banner. The case that makes this reachable is a
            // `HEAD` hit on a pre-rendered page: with a `dist` manifest the
            // static-first middleware answers it directly with `Body::empty()`
            // and `Content-Type: text/html`, and user layers — this one
            // included — are reapplied *outside* that middleware, so no Axum
            // per-route wrapper will fix up what we return. Splicing there
            // would give a `HEAD` response a body and a `Content-Length` of the
            // banner rather than of the equivalent `GET`.
            //
            // Deliberately keyed on the empty body rather than on the method:
            // on a route Axum *does* wrap, a `HEAD` still carries the full body
            // here and must be spliced, so that its `Content-Length` matches
            // the `GET`.
            if bytes.is_empty() {
                parts
                    .headers
                    .append(VARY, HeaderValue::from_static("Cookie"));
                return Response::from_parts(parts, Body::from(bytes));
            }

            if contains_ascii_case_insensitive(&bytes, RENDERED_BANNER_MARKER.as_bytes()) {
                // The handler's own rendering already carries a live,
                // per-visitor CSRF token (see `consent_banner_markup`) —
                // apply the same cache guards as the injection path below so
                // a shared cache can't replay this visitor's token to
                // someone else.
                parts
                    .headers
                    .insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
                parts
                    .headers
                    .append(VARY, HeaderValue::from_static("Cookie"));
                return Response::from_parts(parts, Body::from(bytes));
            }

            // The presence of `</body>` *is* the fragment test. An htmx swap
            // that replaces the whole document has one; a fragment does not.
            // Deciding here rather than from request headers is what makes this
            // correct for `hx-boost`, a history-cache miss, and an ordinary
            // `hx-get` with `hx-target="body"` alike — the last of which sends
            // no header distinguishing it from a fragment at all.
            let Some(updated) = splice_before_body_close(&bytes, snippet) else {
                parts
                    .headers
                    .append(VARY, HeaderValue::from_static("Cookie"));
                return Response::from_parts(parts, Body::from(bytes));
            };

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
fn splice_before_body_close(body: &[u8], snippet: &str) -> Option<Vec<u8>> {
    // The opening is checked FIRST, and it gates both branches.
    //
    // Searching for `</body>` alone is not a document test: a fragment can
    // contain those bytes without being one — inside a `<script>` string, or an
    // HTML comment — and splicing at that offset would drop the banner inside
    // the script or the comment, corrupting the fragment and rendering no
    // usable controls. A real document both opens like one and (usually) closes
    // its body; a fragment that merely mentions the tag does neither.
    //
    // This is a shape test, not a parser. It is deliberately fail-safe: an
    // input it cannot recognise is left alone rather than spliced blind, so the
    // failure mode is a missing banner, never mangled markup.
    if !starts_like_a_document(body) {
        return None;
    }

    // Splice before `</body>` ONLY when it is the document's actual ending —
    // that is, when nothing but `</html>` and whitespace follows it.
    //
    // A raw reverse search cannot tell a real closing tag from the same bytes
    // sitting in a `<script>` string or an HTML comment, and this middleware
    // has no HTML parser. So instead of trying to identify the tag, it checks
    // the one thing that distinguishes the real one: a document ends
    // `…</body></html>`, while a script literal or a trailing comment has other
    // content after it. When the tail does not look like an ending, the banner
    // is appended instead — which is already the correct, tested behaviour for
    // a document that omits `</body>` entirely, and cannot corrupt markup the
    // way splicing into a script string would.
    if let Some(index) = rfind_ascii_case_insensitive(body, b"</body>")
        && tail_is_document_end(&body[index + b"</body>".len()..])
    {
        let mut out = Vec::with_capacity(body.len() + snippet.len());
        out.extend_from_slice(&body[..index]);
        out.extend_from_slice(snippet.as_bytes());
        out.extend_from_slice(&body[index..]);
        return out.into();
    }

    // HTML5 permits omitting `</body>`, so a document can legitimately lack
    // one — `<!doctype html><main>…</main>` is a real page a browser renders,
    // and it must still be prompted.
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
    // A UTF-8 BOM is an encoding artifact, not content — plenty of editors and
    // template toolchains emit one — and a browser reads the document straight
    // through it. It is three fixed bytes in exactly one position, so stripping
    // it is normalization rather than another guess about HTML shape.
    let mut head = body
        .strip_prefix(b"\xEF\xBB\xBF".as_slice())
        .unwrap_or(body);
    // Skip whitespace and any leading comments: a generator's `<!-- built at
    // … -->` preamble ahead of the doctype is still a document, and treating
    // it as a fragment would silently cost that page its banner.
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
    // `<html` must be followed by a tag terminator or ANY ASCII whitespace.
    // Spelling out `<html>` and `<html ` missed `<html\nlang="en">` and the tab
    // and carriage-return forms, all of which are ordinary formatting — and a
    // doctype-less document that wraps its opening tag would have been read as
    // a fragment and gone unprompted.
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
        // The caller only ever invokes this on a response whose Content-Type
        // is already validated as text/html (is_html_response), so a
        // document that omits its <html>/<body> wrapper tags entirely --
        // valid HTML5 tag omission, e.g. `<!doctype html><main>...</main>`
        // -- is still a real page a browser renders; it must still get the
        // banner appended rather than being silently skipped.
        let out = splice_before_body_close(b"<!doctype html><main>ok</main>", "<snip>")
            .expect("a document must be spliceable");
        let s = String::from_utf8(out).unwrap();
        assert!(s.ends_with("<snip>"), "{s}");
    }

    #[test]
    fn splice_refuses_a_fragment_rather_than_appending_to_it() {
        // The round-one bug: appending to a fragment put a second banner on the
        // page when htmx swapped it in. A fragment is now recognised by its own
        // opening — no request header separates it from a whole-document swap.
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
        // `<html>` and `<html ` were spelled out literally; a document that
        // wraps its attributes onto the next line is just as valid and was
        // being read as a fragment.
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
        // A complete document whose only `</body>` bytes live in a script
        // string, or whose real closing tag is followed by a comment carrying
        // the same text. Splicing at the raw match would drop the banner into
        // JavaScript or a comment: corrupt markup AND no usable prompt.
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
        // Finding the bytes `</body>` is not a document test. Splicing at that
        // offset would land the banner inside the script string or the comment
        // — corrupting the fragment AND rendering no usable controls.
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
        // A byte sequence that is not valid UTF-8 (0xE9 alone is an ISO-8859-1
        // 'é', but an invalid/incomplete UTF-8 sequence on its own).
        // `String::from_utf8_lossy` would replace it with U+FFFD; this must
        // pass it through byte-for-byte instead, since the raw bytes are
        // spliced around, never decoded.
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
        // The static-page case: user `.layer()` middleware runs OUTSIDE the
        // static-first layer (so it can process pre-rendered responses), which
        // means `CsrfLayer` never ran and set no cookie. Rendering the banner
        // here would give the visitor two buttons that both POST without the
        // hidden `_csrf` field and get a 403 — inviting a decision and then
        // silently refusing it.
        let app = Router::new()
            .route("/about", get(|| async { html_page() }))
            .layer(axum::middleware::from_fn(move |req, next| async move {
                // `Some(..)` = the app enforces CSRF, but nothing in this stack
                // ever issues the cookie.
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
        // `None` = the app disabled CSRF entirely, so an absent token is
        // expected rather than a symptom, and the banner works without one.
        // This is what keeps the skip above from silently disabling the
        // feature for such an app.
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
        // An API client never acquires a consent cookie, so it always "needs
        // prompting" — if that stripped its validators, its conditional
        // requests could never answer 304 again and it would refetch full
        // bodies forever.
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
        // The guard above must not have disabled the protection it narrows:
        // a browser navigation still has to bypass a cached 304, or the
        // banner-less page replays and the prompt is skipped.
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
        // A fragment carries no `</body>`, so the splice would APPEND the
        // banner — and htmx would swap that copy into the page alongside the
        // banner the enclosing document already has. An undecided visitor
        // would see duplicate consent controls on every page that hydrates a
        // fragment on load.
        let app = Router::new()
            .route(
                "/_partials/nav-auth",
                get(|| async {
                    Response::builder()
                        .header(CONTENT_TYPE, "text/html; charset=utf-8")
                        // A real fragment: no <html>, no <body>.
                        .body(Body::from("<div id=\"nav\">Log in</div>"))
                        .unwrap()
                }),
            )
            .layer(axum::middleware::from_fn(move |req, next| async move {
                inject_consent_banner(req, next, 1, Some("autumn-csrf"), "_csrf").await
            }));

        // No consent cookie: this visitor definitely still needs prompting,
        // so the only thing keeping the banner out is the htmx check.
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
        // `is_html_response` returns false for any `Content-Encoding` body,
        // because it cannot be spliced into without decoding. But it is still
        // HTML whose representation depends on the consent cookie — a handler
        // can gate markup on `Consent::allows` and compress the result — so
        // treating "cannot splice" as "not HTML" dropped `Vary: Cookie` and let
        // a shared cache serve one visitor's choice to another.
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
        // The widened HTML check must not start stamping `Vary` on every
        // response — a JSON API's cacheability is not this middleware's business.
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
        // No banner, but the handler may have gated markup on
        // `Consent::allows`. Without `Vary: Cookie` a shared cache could store
        // a consenting visitor's fragment and replay it to one who rejected.
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
        // htmx issues a boosted navigation over XHR without setting `Accept`,
        // so it arrives as `*/*` — which `accepts_html` deliberately rejects to
        // protect API clients' 304s. But a boosted request IS a document this
        // middleware injects into, so leaving its validators intact would let
        // an inner EtagLayer answer 304 with no body to inject, and a policy
        // bump could go unprompted for as long as the cache stays fresh.
        //
        // Note the previous test does not cover this: it sends no `Accept` at
        // all, and a missing `Accept` is already treated as HTML.
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
        // `*/*` stays excluded: that carve-out is about the XHR default
        // specifically, not about wildcards generally.
        assert!(!accept("*/*"));

        // The finding: named, but refused.
        assert!(!accept("application/json, text/html;q=0"));
        assert!(!accept("text/html;q=0.0"));
        assert!(!accept("text/html; q=0.000"));

        // Still accepted — a zero check must not become a q-ranking.
        assert!(accept("text/html"));
        assert!(accept("text/html;q=0.1"));
        assert!(accept("text/html;q=0.9, application/json"));
        assert!(accept("application/json, text/html"));
        // `*/*` remains deliberately excluded: the XHR default must not count
        // as a browser navigation. See `a_plain_xhr_json_client_...`.
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
        // The boosted carve-out must not extend to an ordinary `*/*` XHR that
        // is not a boosted navigation — that is the API client whose 304s the
        // Accept check exists to protect.
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
        // `hx-boost` sends BOTH `HX-Request` and `HX-Boosted`, but the response
        // is a complete page that replaces the current body. Treating it as a
        // fragment would omit the banner from the new page AND destroy the one
        // on the old page in the same swap — leaving an undecided visitor
        // silently unprompted until a non-htmx navigation.
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
        // The guard above must not have turned the banner off in general.
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
        // The handler's own rendering (as in the test above) carries a live,
        // per-visitor CSRF token. Even though this middleware doesn't splice
        // anything into that response, it must still stamp the same
        // no-cache guards as the injection path — otherwise a shared cache
        // could serve one visitor's token-bearing form to another.
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
        // The form-field name is config-derived, not request-derived (see
        // `inject_consent_banner`'s doc: `CsrfLayer` sits inner to user
        // layers, so a `CsrfFormField` request extension is never visible
        // here) -- so it must be threaded in as an explicit parameter, not
        // read back off the request or response.
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
}
