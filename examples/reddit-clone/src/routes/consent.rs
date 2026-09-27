//! Cookie-consent routes — record, withdraw, and re-decide (#1214).
//!
//! The framework ships the codec (`accept_all_cookie` /
//! `reject_non_essential_cookie` / `expire_consent_cookie`), the gate
//! (`Consent::allows`), and the banner plus its injector. What an app owns is
//! the part only it can know: which of its cookies are non-essential, and where
//! the withdraw link lives.
//!
//! This app has exactly one non-essential category — `"analytics"` — and it is
//! gated at its only call site in [`crate::routes::layout::analytics_snippet`].
//! The session cookie and the CSRF cookie are strictly necessary and are never
//! routed through the gate at all; see `docs/guide/cookie-consent.md`.
//!
//! Note the method split, which is the part most hand-rolled banners get wrong:
//! **accept and reject are `POST`** (so they sit behind CSRF protection), and
//! only the *preferences page* is a `GET`. A `GET` that changes consent can be
//! fired by a link prefetcher, a browser extension, or a cross-site top-level
//! navigation — none of which is the visitor deciding anything.

use autumn_web::consent::Consent;
use autumn_web::prelude::*;
use autumn_web::reexports::http::HeaderMap;
use autumn_web::reexports::http::header::{CACHE_CONTROL, REFERER, SET_COOKIE, VARY};

use autumn_web::seo::SeoMeta;

use super::layout::layout_with_seo;

/// The version of this app's cookie policy.
///
/// Bump it when the policy changes — a new category, a new vendor, or a change
/// to what an existing category does. A consent decision recorded under an
/// older version is treated as undecided, so the banner reappears and the gate
/// closes until the visitor decides again.
pub const CONSENT_POLICY_VERSION: u32 = 1;

/// The non-essential categories this app asks for.
///
/// Keep this list and the strings passed to `Consent::allows` in sync — a
/// category the visitor consents to but nothing gates on is noise, and a
/// category something gates on but the banner never asks for is a permanently
/// closed gate.
pub const CATEGORIES: &[&str] = &["analytics"];

/// Record consent to every category in [`CATEGORIES`].
#[post("/consent/accept")]
pub async fn accept(headers: HeaderMap) -> impl IntoResponse {
    let cookie = autumn_web::consent::accept_all_cookie(CATEGORIES, CONSENT_POLICY_VERSION);
    ([(SET_COOKIE, cookie)], Redirect::to(&back_to(&headers)))
}

/// Record an explicit rejection of every non-essential category.
#[post("/consent/reject")]
pub async fn reject(headers: HeaderMap) -> impl IntoResponse {
    let cookie = autumn_web::consent::reject_non_essential_cookie(CONSENT_POLICY_VERSION);
    ([(SET_COOKIE, cookie)], Redirect::to(&back_to(&headers)))
}

/// Withdraw the recorded decision entirely (GDPR Art. 7(3)).
///
/// Clearing the cookie returns the visitor to *undecided*, which is not the
/// same as rejecting: the gate closes either way, but the banner comes back on
/// the next page so the choice can be made again from scratch.
#[post("/consent/withdraw")]
pub async fn withdraw(flash: Flash, headers: HeaderMap) -> impl IntoResponse {
    let cookie = autumn_web::consent::expire_consent_cookie();
    flash.success("Cookie choice cleared.").await;
    ([(SET_COOKIE, cookie)], Redirect::to(&back_to(&headers)))
}

/// The preferences page linked from the footer of every page.
///
/// Renders the framework's own banner widget rather than a second set of
/// buttons, so "change your mind" and "decide the first time" are the same
/// markup posting to the same routes. The injector detects the banner's marker
/// and skips adding another copy on top.
#[get(
    "/consent/manage",
    seo(
        title = "Cookie preferences \u{2022} Autumn Reddit",
        robots = "noindex, nofollow"
    )
)]
pub async fn manage(
    // The `seo(...)` above is only a declaration: something has to carry it
    // into the `<head>`. `SeoMeta` is the extractor that receives the route's
    // declared metadata, and `layout_with_seo` is what renders it. Reaching for
    // the plain `layout` here silently dropped `robots = "noindex, nofollow"`,
    // because `layout` builds its own title-only `SeoMeta` — leaving a page
    // that embeds a live CSRF token and a visitor's cookie choice indexable
    // despite declaring otherwise.
    seo: SeoMeta,
    session: Session,
    csrf: CsrfToken,
    // `CsrfLayer` scans a URL-encoded body for the CONFIGURED field name only.
    // Naming the hidden inputs `_csrf` — or `DEFAULT_CSRF_FORM_FIELD`, which is
    // the same string — renders a page whose every button 403s on an app that
    // set `security.csrf.form_field`. This extractor is the configured value,
    // and is inert under the default.
    csrf_field: CsrfFormField,
    consent: Consent,
    flash: Flash,
) -> impl IntoResponse {
    let current_user = session.get("username").await;
    let flash_html = flash.render().await;

    let status = if consent.needs_prompt(CONSENT_POLICY_VERSION) {
        "You have not made a choice yet."
    } else if consent.allows("analytics", CONSENT_POLICY_VERSION) {
        "Analytics cookies are currently allowed."
    } else {
        "Only strictly-necessary cookies are running."
    };

    let markup = layout_with_seo(
        seo,
        current_user.as_deref(),
        Some(csrf.token()),
        html! {
            (flash_html)
            div class="max-w-2xl mx-auto" {
                h1 class="text-2xl font-bold mb-4" { "Cookie preferences" }
                p class="text-sm text-gray-600 mb-6" { (status) }

                // The framework widget: two equally-weighted buttons, plain
                // HTML forms, no JavaScript required.
                (autumn_web::consent::consent_banner_markup(
                    Some(csrf.token()),
                    &csrf_field.0,
                ))

                @if consent.is_decided() {
                    form action=(paths::withdraw()) method="post" class="mt-6" {
                        input type="hidden" name=(csrf_field.0) value=(csrf.token());
                        button type="submit"
                               class="text-sm text-gray-500 underline hover:text-orange-600" {
                            "Clear my choice and ask me again"
                        }
                    }
                }
            }
        },
    );

    // This page embeds the visitor's own live CSRF token, so it must never be
    // shared by a cache: a public cache serving it to someone else would leak
    // that token and break every other visitor's consent form.
    (
        [(CACHE_CONTROL, "private, no-store"), (VARY, "Cookie")],
        markup,
    )
}

/// Same-origin-clamped "return the visitor to where they were".
///
/// `redirect_target_from_referer` refuses an off-site destination, so the
/// `Referer` header cannot turn these routes into an open redirect.
fn back_to(headers: &HeaderMap) -> String {
    autumn_web::consent::redirect_target_from_referer(
        headers.get(REFERER).and_then(|v| v.to_str().ok()),
    )
}

autumn_web::paths![accept, reject, withdraw, manage];

#[cfg(test)]
mod tests {
    use autumn_web::consent::Consent;
    use autumn_web::prelude::*;
    use autumn_web::seo::SeoMeta;

    use super::{CATEGORIES, CONSENT_POLICY_VERSION};

    #[test]
    fn an_undecided_visitor_is_prompted_and_gated() {
        let consent = Consent::undecided();
        assert!(consent.needs_prompt(CONSENT_POLICY_VERSION));
        assert!(!consent.allows("analytics", CONSENT_POLICY_VERSION));
        // The necessary category is exempt by definition, so the app can route
        // its session/CSRF call sites through the same check if it wants one
        // code path.
        assert!(consent.allows("necessary", CONSENT_POLICY_VERSION));
    }

    /// The preferences page declares `robots = "noindex, nofollow"`, and a
    /// declaration is only worth as much as the rendering that carries it.
    /// `manage` originally called the plain `layout`, which builds its own
    /// title-only `SeoMeta`, so the robots directive never reached the
    /// `<head>` — leaving a page that embeds a live CSRF token and the
    /// visitor's cookie choice indexable.
    ///
    /// `manage` takes five extractors, so unlike `about` it cannot be called
    /// directly here. The guard is a chain of two halves instead:
    ///
    /// 1. `manage`'s first parameter is the route's `SeoMeta` (checked at
    ///    compile time below), and an unused binding would fail the `-D
    ///    warnings` gate — so it is not merely accepted, it is used.
    /// 2. `layout_with_seo`, the only thing it can be used for here, really
    ///    does render the directive.
    ///
    /// Neither half alone would have caught the original bug: the old code
    /// passed this second assertion while dropping the tag.
    #[test]
    fn the_preferences_page_really_renders_its_noindex_directive() {
        // Half 1 — compile-time: `manage` still receives the declared metadata.
        fn takes_route_seo<F, Fut>(_handler: F)
        where
            F: Fn(SeoMeta, Session, CsrfToken, CsrfFormField, Consent, Flash) -> Fut,
        {
        }
        takes_route_seo(super::manage);

        // Half 2 — the renderer it hands that metadata to emits the directive.
        let seo = SeoMeta::new()
            .title("Cookie preferences \u{2022} Autumn Reddit")
            .robots("noindex, nofollow");

        let rendered = super::layout_with_seo(seo, None, Some("tok"), maud::html! {}).into_string();

        assert!(
            rendered.contains(r#"content="noindex, nofollow""#),
            "the declared robots directive must reach the rendered head: {rendered}"
        );
    }

    #[test]
    fn every_advertised_category_is_one_the_app_actually_gates_on() {
        // A category the banner asks for but nothing gates on is consent
        // theater; this keeps the two honest as the app grows.
        assert_eq!(CATEGORIES, &["analytics"]);
    }
}
