//! Shared layout and UI components used across all routes.

use autumn_web::reexports::axum::response::{IntoResponse, Response};
use autumn_web::reexports::http;
use autumn_web::seo::SeoMeta;
use autumn_web::widgets::{ReactionControls, reaction_controls};
use autumn_web::{HTMX_CSRF_JS_PATH, HTMX_JS_PATH, HTMX_SSE_JS_PATH, Markup, PreEscaped, html};

/// Redirect that works for both regular and htmx requests.
///
/// Returns an `HX-Redirect` header so htmx performs a full-page navigation
/// instead of swapping the response into the triggering element. Also
/// includes a standard HTTP redirect fallback for non-htmx clients.
pub fn hx_redirect_to(url: &str) -> Response {
    let mut response = autumn_web::Redirect::to(url).into_response();
    response.headers_mut().insert(
        http::header::HeaderName::from_static("hx-redirect"),
        http::header::HeaderValue::from_str(url)
            .unwrap_or_else(|_| http::header::HeaderValue::from_static("/")),
    );
    response
}

/// Render the nav auth content — the final settled state, no htmx triggers.
///
/// Used by the `/_partials/nav-auth` endpoint so its response doesn't
/// re-trigger another fetch (which would create an infinite loop).
pub fn nav_auth_content(username: Option<&str>) -> Markup {
    html! {
        div class="flex items-center gap-3 text-sm" {
            @if let Some(name) = username {
                span class="text-gray-600" { "u/" (name) }
                a href="/submit"
                  class="px-3 py-1.5 bg-orange-500 text-white rounded hover:bg-orange-600" {
                    "New Post"
                }
                // Logout uses the meta CSRF tag via the autumn-csrf.js script.
                button
                    hx-post="/logout"
                    aria-label="Log out"
                    class="text-gray-500 hover:text-orange-600 cursor-pointer" {
                    "Log out"
                }
            } @else {
                a href="/login" class="text-gray-600 hover:text-orange-600" { "Log in" }
                a href="/register"
                  class="px-3 py-1.5 bg-orange-500 text-white rounded hover:bg-orange-600" {
                    "Sign up"
                }
            }
        }
    }
}

/// Render the nav auth slot for use inside a full page layout.
///
/// When `username` is `Some` (dynamic pages — session is known at render time),
/// returns the content directly with no extra request.
///
/// When `None` (anonymous users on dynamic pages OR any static pre-rendered
/// page), wraps the anonymous buttons in an htmx one-shot hydration shell.
/// The shell fires a single `GET /_partials/nav-auth` on page load and swaps
/// itself out with `nav_auth_content` — which has no htmx trigger, so the
/// loop stops after one round-trip.
pub fn nav_auth_markup(username: Option<&str>) -> Markup {
    if username.is_some() {
        nav_auth_content(username)
    } else {
        html! {
            div class="flex items-center gap-3 text-sm"
                hx-get="/_partials/nav-auth"
                hx-trigger="load"
                hx-swap="outerHTML" {
                a href="/login" class="text-gray-600 hover:text-orange-600" { "Log in" }
                a href="/register"
                  class="px-3 py-1.5 bg-orange-500 text-white rounded hover:bg-orange-600" {
                    "Sign up"
                }
            }
        }
    }
}

/// Base HTML layout wrapping page content.
///
/// Accepts an optional `username` to show login/logout state in the nav.
///
/// This is the plain-title entry point, and most pages use it. It builds a
/// [`SeoMeta`] that holds only the title and calls [`layout_with_seo`]. Pages
/// that want more meta tags — a description, a canonical URL, Open Graph
/// values, a `robots` directive — call [`layout_with_seo`] directly with the
/// builder the `SeoMeta` extractor gave them.
#[allow(clippy::needless_pass_by_value)] // Maud Markup is idiomatically passed by value
pub fn layout(
    title: &str,
    username: Option<&str>,
    csrf_token: Option<&str>,
    content: Markup,
) -> Markup {
    layout_with_seo(
        SeoMeta::new().title(format!("{title} \u{2014} Autumn Reddit")),
        username,
        csrf_token,
        content,
    )
}

/// Base HTML layout that takes an explicit [`SeoMeta`] builder.
///
/// [`SeoMeta::render`] writes the `<title>` tag, so `seo` must carry a title.
/// The route attribute usually supplies it:
///
/// ```rust,ignore
/// #[get("/about", seo(title = "About \u{2022} Autumn Reddit"))]
/// pub async fn about(seo: SeoMeta) -> Markup {
///     layout_with_seo(seo, None, None, html! { /* ... */ })
/// }
/// ```
///
/// See `docs/guide/seo.md`.
#[allow(clippy::needless_pass_by_value)] // Maud Markup is idiomatically passed by value
pub fn layout_with_seo(
    seo: SeoMeta,
    username: Option<&str>,
    csrf_token: Option<&str>,
    content: Markup,
) -> Markup {
    html! {
        (PreEscaped("<!DOCTYPE html>"))
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                // One call writes <title>, the description, the canonical
                // link, the Open Graph tags, and the Twitter card tags — but
                // only the ones this page actually set (see docs/guide/seo.md).
                (seo.render())
                // Embed CSRF token in a meta tag so htmx JS can read it
                // (the autumn-csrf cookie is HttpOnly and inaccessible to JS)
                @if let Some(token) = csrf_token {
                    meta name="csrf-token" content=(token);
                }
                link rel="stylesheet" href=(autumn_web::flash::FLASH_CSS_PATH);
                link rel="stylesheet" href=(autumn_web::ui::WIDGETS_CSS_PATH);
                link rel="stylesheet" href="/static/css/autumn.css";
                style {
                    " #posts-list.posts-feed-compact .posts-feed-card-version { display: none !important; } "
                    " #posts-list:not(.posts-feed-compact) .posts-feed-compact-version { display: none !important; } "
                    " #posts-list-sub .posts-feed-compact-version { display: none !important; } "
                }
                script src=(HTMX_JS_PATH) {}
                script src=(HTMX_SSE_JS_PATH) {}
                script src=(HTMX_CSRF_JS_PATH) {}
            }
            body class="bg-gray-100 min-h-screen text-gray-900" {
                // Skip-to-content link — first focusable element for keyboard users.
                a href="#main-content"
                  class="skip-link sr-only focus:not-sr-only focus:absolute focus:top-2 focus:left-2 \
                         focus:z-50 focus:px-4 focus:py-2 focus:bg-white focus:text-gray-900 \
                         focus:border focus:border-gray-300 focus:rounded focus:shadow" {
                    "Skip to main content"
                }

                // ARIA live region for htmx swap announcements.
                // Update this element's content via hx-swap-oob="true" in htmx responses
                // to announce dynamic changes to screen readers without moving focus.
                div id="htmx-status" role="status" aria-live="polite" aria-atomic="true"
                    class="sr-only" {}

                // Site-wide navigation banner
                header role="banner" {
                    nav aria-label="Main navigation"
                        class="bg-white border-b border-gray-200 shadow-sm sticky top-0 z-10" {
                        div class="max-w-5xl mx-auto px-4 py-3 flex items-center justify-between" {
                            div class="flex items-center gap-6" {
                                a href="/" class="text-xl font-bold text-orange-600 hover:text-orange-700" {
                                    "autumn/reddit"
                                }
                                div class="hidden sm:flex items-center gap-4 text-sm" {
                                    a href="/r" class="text-gray-600 hover:text-orange-600" { "Communities" }
                                    a href="/about" class="text-gray-600 hover:text-orange-600" { "About" }
                                    a href="/actuator/health" class="text-gray-500 hover:text-orange-600" { "Health" }
                                }
                            }
                            (nav_auth_markup(username))
                        }
                    }
                }

                // Main content landmark
                main id="main-content" class="max-w-5xl mx-auto py-6 px-4" {
                    (content)
                }

                // Site footer
                footer role="contentinfo" class="border-t border-gray-200 mt-12" {
                    div class="max-w-5xl mx-auto text-center text-xs text-gray-400 py-6" {
                        "Built with "
                        a href="https://github.com/autumn-foundation/autumn"
                          class="text-orange-600 hover:underline" { "Autumn" }
                        " — Rust + Diesel + Maud + htmx + Tailwind"
                    }
                }
            }
        }
    }
}

/// Score display with upvote/downvote buttons.
///
/// A thin delegation to the framework's `reaction_controls` widget (#1362) —
/// the view half of `Post`'s `#[votable]` association. The widget renders one
/// no-JS `POST` form per direction (upgraded in place by htmx), ARIA toggle
/// buttons with real accessible names, and the `#votes-{id}` outerHTML
/// self-replacement contract this example's routes already rely on.
///
/// `current` is the viewer's own vote from `posts.reaction_of(user, post)`:
/// `Some(1)` / `Some(-1)` press the matching button, `None` presses neither.
/// Feeds and live fragments pass `None` deliberately — highlighting every row
/// would cost one query per post (a batch accessor is the tracked follow-up).
///
/// `csrf` is the handler's own `CsrfToken` extractor. CSRF protection is
/// **enabled** in this app (`autumn.toml`), so it is load-bearing for the no-JS
/// path: the hidden `_csrf` input is what lets a plain form POST through when
/// JavaScript is off. The htmx path would survive without it — the framework's
/// `autumn-htmx-csrf.js` shim sends the token as a header — so a missing token
/// fails only for the visitors least able to work around it. `None` is
/// therefore correct in exactly one place: fragments broadcast over SSE, which
/// by construction only reach clients that are running htmx.
pub fn vote_controls(
    post_id: i64,
    score: i64,
    current: Option<i16>,
    csrf: Option<&autumn_web::security::CsrfToken>,
) -> Markup {
    reaction_controls(
        &ReactionControls::votes(
            format!("votes-{post_id}"),
            super::votes::__autumn_path_upvote(post_id),
            super::votes::__autumn_path_downvote(post_id),
        )
        .aggregate(score)
        .current(current)
        // The form-field name stays the default `_csrf`, which is what the
        // app's CSRF layer expects.
        .csrf(csrf, None)
        .label("Post score"),
    )
}

/// The vote control as rendered inside a fragment **broadcast to every
/// subscriber** (the SSE post-card fan-out).
///
/// Same shape as [`vote_controls`] with `current = None` / no CSRF (see the
/// rationale there), plus `preserve_pressed_state`: the broadcast card swap
/// arrives on viewers who may have a vote pressed — including the voter
/// themself, racing their own targeted response — and `hx-preserve` on the
/// buttons keeps each viewer's live pressed state while the swap still
/// refreshes the aggregate and the rest of the card.
pub fn broadcast_vote_controls(post_id: i64, score: i64) -> Markup {
    reaction_controls(
        &ReactionControls::votes(
            format!("votes-{post_id}"),
            super::votes::__autumn_path_upvote(post_id),
            super::votes::__autumn_path_downvote(post_id),
        )
        .aggregate(score)
        .preserve_pressed_state(true)
        .label("Post score"),
    )
}

/// Timestamp display helper.
pub fn time_ago(dt: &chrono::NaiveDateTime) -> String {
    let now = chrono::Utc::now().naive_utc();
    let diff = now - *dt;

    if diff.num_days() > 365 {
        format!("{}y ago", diff.num_days() / 365)
    } else if diff.num_days() > 30 {
        format!("{}mo ago", diff.num_days() / 30)
    } else if diff.num_days() > 0 {
        format!("{}d ago", diff.num_days())
    } else if diff.num_hours() > 0 {
        format!("{}h ago", diff.num_hours())
    } else if diff.num_minutes() > 0 {
        format!("{}m ago", diff.num_minutes())
    } else {
        "just now".to_string()
    }
}

#[cfg(test)]
mod tests {
    use autumn_web::html;
    use autumn_web::seo::SeoMeta;

    use super::{layout, layout_with_seo};

    #[test]
    fn layout_still_renders_the_site_title_suffix() {
        let rendered = layout("Front Page", None, None, html! {}).into_string();

        assert!(
            rendered.contains("<title>Front Page \u{2014} Autumn Reddit</title>"),
            "rendered: {rendered}"
        );
    }

    #[test]
    fn layout_with_seo_renders_every_declared_tag() {
        let seo = SeoMeta::new()
            .title("hello \u{2022} r/rust \u{2022} Autumn Reddit")
            .description("A post about Rust.")
            .canonical("https://autumn-reddit.example.com/r/rust/posts/hello")
            .og_type("article")
            .twitter_card("summary_large_image");

        let rendered = layout_with_seo(seo, None, None, html! {}).into_string();

        assert!(
            rendered.contains("<title>hello \u{2022} r/rust \u{2022} Autumn Reddit</title>"),
            "rendered: {rendered}"
        );
        assert!(
            rendered.contains(r#"<meta name="description" content="A post about Rust.">"#),
            "rendered: {rendered}"
        );
        assert!(
            rendered.contains(
                r#"<link rel="canonical" href="https://autumn-reddit.example.com/r/rust/posts/hello">"#
            ),
            "rendered: {rendered}"
        );
        assert!(
            rendered.contains(r#"<meta property="og:type" content="article">"#),
            "rendered: {rendered}"
        );
        // og:title and og:description fall back to the page title and
        // description, so the app sets each value one time.
        assert!(
            rendered.contains(r#"<meta property="og:title" content="hello"#),
            "rendered: {rendered}"
        );
        assert!(
            rendered.contains(r#"<meta name="twitter:card" content="summary_large_image">"#),
            "rendered: {rendered}"
        );
        // og:url falls back to the canonical URL.
        assert!(
            rendered.contains(
                r#"<meta property="og:url" content="https://autumn-reddit.example.com/r/rust/posts/hello">"#
            ),
            "rendered: {rendered}"
        );
    }

    #[test]
    fn layout_with_seo_emits_only_the_tags_a_page_sets() {
        let rendered =
            layout_with_seo(SeoMeta::new().title("Plain"), None, None, html! {}).into_string();

        assert!(
            rendered.contains("<title>Plain</title>"),
            "rendered: {rendered}"
        );
        assert!(
            !rendered.contains("og:type"),
            "an unset value must emit no tag; rendered: {rendered}"
        );
        assert!(
            !rendered.contains("rel=\"canonical\""),
            "an unset value must emit no tag; rendered: {rendered}"
        );
    }

    /// `noindex, follow` is the directive a thin-but-linking page wants: keep
    /// this page out of the index, but let crawlers walk through to the pages
    /// that belong in it. `routes::auth::profile` declares exactly this.
    #[test]
    fn layout_with_seo_renders_noindex_follow_for_a_profile_page() {
        let seo = SeoMeta::new()
            .title("u/ferris \u{2022} Autumn Reddit")
            .robots("noindex, follow")
            .og_type("profile");

        let rendered = layout_with_seo(seo, None, None, html! {}).into_string();

        assert!(
            rendered.contains(r#"<meta name="robots" content="noindex, follow">"#),
            "rendered: {rendered}"
        );
        assert!(
            !rendered.contains("nofollow"),
            "`follow` must not be rendered as `nofollow`; rendered: {rendered}"
        );
    }

    #[test]
    fn layout_with_seo_renders_a_noindex_directive() {
        let seo = SeoMeta::new()
            .title("Submit a post \u{2022} Autumn Reddit")
            .robots("noindex, nofollow");

        let rendered = layout_with_seo(seo, Some("ferris"), None, html! {}).into_string();

        assert!(
            rendered.contains(r#"<meta name="robots" content="noindex, nofollow">"#),
            "rendered: {rendered}"
        );
    }

    #[test]
    fn layout_loads_framework_csrf_script_from_same_origin() {
        let rendered = layout("Test", None, Some("token"), html! {}).into_string();

        assert!(rendered.contains(r#"<script src="/static/js/htmx.min.js"></script>"#));
        assert!(rendered.contains(r#"<script src="/static/js/autumn-htmx-csrf.js"></script>"#));
        assert!(
            !rendered.contains("htmx:configRequest"),
            "CSRF htmx listener must not be rendered inline under script-src 'self'",
        );
    }
}
