//! HTML routes for the blog application.
//!
//! These routes render Maud templates styled with Tailwind CSS and
//! use htmx attributes for interactive publish/delete behaviour.

use autumn_web::assets::asset_url;
use autumn_web::cache::cache_fragment_global;
use autumn_web::config::AutumnConfig;
use autumn_web::extract::{Form, Path};
use autumn_web::i18n::Locale;
use autumn_web::prelude::{IntoResponse, StatusCode};
use autumn_web::seo::{SeoMeta, locale_alternates};
use autumn_web::widgets::{Crumb, HeroConfig, breadcrumb, hero, locale_switcher};
use autumn_web::{AutumnError, AutumnResult, Db, Markup, Redirect, delete, get, html, post, t};
use diesel::prelude::*;
use diesel_async::RunQueryDsl;

use crate::models::{NewPost, Post, UpdatePost};
use crate::schema::posts;

/// The current request's locale-stripped path plus any query string (issue
/// #1251) — what [`locale_switcher`] needs to link to *this* page in every
/// other supported locale. Inside a `/{locale}/...` nest, axum's `Uri`
/// extractor already returns the prefix-stripped path, so this is a plain
/// passthrough with the query string reattached.
fn path_and_query(uri: &autumn_web::reexports::http::Uri) -> String {
    match uri.query() {
        Some(query) => format!("{}?{query}", uri.path()),
        None => uri.path().to_owned(),
    }
}

// ── Layout ──────────────────────────────────────────────────────

/// Base HTML layout wrapping page content.
///
/// Takes the request [`Locale`] so nav, footer, and locale-switcher labels
/// are translated through the [`t!`] macro. Pages reachable via
/// `#[static_get]` (e.g. `about.rs`) use the same extractor during static
/// rendering, so pre-rendered HTML receives the configured bundle too.
///
/// `path_and_query` is the current page's locale-stripped path (plus any
/// query string) — see [`locale_switcher`] — used to render the site-wide
/// language switcher so it always links to *this* page in each other
/// supported locale (issue #1251), not just the home page. Pass `None` for
/// pages excluded from locale-prefixing (`/admin/*`, per
/// `[i18n] locale_prefix_exclude` in `autumn.toml`) — the switcher would
/// otherwise link to a `/{locale}/admin/...` URL that 404s.
///
/// Accepts an optional [`SeoMeta`] to inject per-page meta tags. Falls back
/// to a sensible site-wide description when omitted.
pub fn layout(
    locale: &Locale,
    path_and_query: Option<&str>,
    title: &str,
    content: Markup,
) -> Markup {
    layout_with_seo(
        locale,
        path_and_query,
        SeoMeta::new()
            .title(title)
            .description("A blog built with the Autumn web framework for Rust."),
        content,
    )
}

/// Layout variant accepting an explicit [`SeoMeta`] builder.
pub fn layout_with_seo(
    locale: &Locale,
    path_and_query: Option<&str>,
    seo: SeoMeta,
    content: Markup,
) -> Markup {
    html! {
        (autumn_web::PreEscaped("<!DOCTYPE html>"))
        html lang=(locale.tag()) {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                (seo.render())
                link rel="stylesheet" href=(autumn_web::ui::WIDGETS_CSS_PATH);
                link rel="stylesheet" href=(asset_url("css/autumn.css"));
                script src=(asset_url("js/htmx.min.js")) {}
            }
            body class="bg-stone-50 min-h-screen font-sans text-stone-800 antialiased" {
                a href="#main-content"
                  class="skip-link sr-only focus:not-sr-only focus:absolute focus:top-2 focus:left-2 \
                         focus:z-50 focus:px-4 focus:py-2 focus:bg-white focus:text-gray-900 \
                         focus:border focus:border-gray-300 focus:rounded focus:shadow" {
                    (t!(locale, "layout.skip_to_content"))
                }
                // Navigation
                nav class="border-b border-stone-200 bg-white/80 backdrop-blur-sm sticky top-0 z-10" {
                    div class="max-w-3xl mx-auto px-6 py-4 flex items-center justify-between" {
                        a href=(paths::index()) class="text-lg font-semibold text-stone-900 hover:text-amber-700 transition-colors" {
                            "\u{1F342} " (t!(locale, "nav.brand"))
                        }
                        div class="flex items-center gap-4" {
                            a href=(paths::index()) class="text-sm text-stone-600 hover:text-amber-700 transition-colors" { (t!(locale, "nav.home")) }
                            a href="/about" class="text-sm text-stone-600 hover:text-amber-700 transition-colors" { (t!(locale, "nav.about")) }
                            a href="/greet" class="text-sm text-stone-600 hover:text-amber-700 transition-colors" { (t!(locale, "nav.greet")) }
                            a href=(paths::admin_list()) class="text-sm text-stone-600 hover:text-amber-700 transition-colors" { (t!(locale, "nav.admin")) }
                            a href="/backoffice/posts" class="text-sm text-stone-600 hover:text-amber-700 transition-colors" { "Plugin Admin" }
                            a href=(paths::new_form()) class="text-sm px-3 py-1.5 bg-amber-700 text-white rounded-lg hover:bg-amber-800 transition-colors" { (t!(locale, "nav.new_post")) }
                            // Locale switcher (issue #1251) — one call,
                            // preserves this exact page's path and query;
                            // only the locale segment changes. Omitted on
                            // pages excluded from locale-prefixing (e.g.
                            // `/admin/*`), which have no `/{locale}/...` URL.
                            @if let Some(path_and_query) = path_and_query {
                                span class="text-xs text-stone-400 ml-2" { (t!(locale, "nav.locale.label")) ":" }
                                (locale_switcher(
                                    path_and_query,
                                    locale.tag(),
                                    locale.bundle().map_or(&[], |b| b.supported_locales()),
                                ))
                            }
                        }
                    }
                }

                // Main content
                main id="main-content" class="max-w-3xl mx-auto py-10 px-6" {
                    (content)
                }

                // Footer
                footer class="border-t border-stone-200 mt-16" {
                    div class="max-w-3xl mx-auto text-center text-xs text-stone-500 py-8" {
                        (t!(locale, "footer.tagline"))
                        " \u{2022} "
                        a href="https://github.com/autumn-foundation/autumn"
                          class="text-amber-700 underline hover:text-amber-800" {
                            "Autumn"
                        }
                        " \u{2022} Rust + Diesel + Maud + htmx + Tailwind"
                    }
                }
            }
        }
    }
}

// ── Components ──────────────────────────────────────────────────

/// Render a single post card for the listing page.
///
/// The rendered markup is cached with [`cache_fragment_global`], keyed by the
/// post's id **plus** its `updated_at` timestamp. On a warm cache an unchanged
/// row is served without re-running the `html!{}` work; editing the post bumps
/// `updated_at`, which changes the cache key and re-renders the card on the
/// very next request — no manual eviction. When no cache backend is configured
/// the helper renders directly, so this is safe in any environment.
fn post_card(post: &Post) -> Markup {
    cache_fragment_global(
        format_args!("blog:post_card:{}", post.id),
        // Microsecond resolution so two edits in the same wall-clock second
        // still produce distinct cache keys (a plain `timestamp()` would
        // collide and serve the first edit's stale markup).
        post.updated_at.and_utc().timestamp_micros(),
        None,
        || render_post_card(post),
    )
}

/// The actual Maud render for a post card (executed only on a cache miss).
fn render_post_card(post: &Post) -> Markup {
    let date = post.created_at.format("%b %d, %Y");
    let preview: String = post.body.chars().take(200).collect::<String>();
    let preview = if post.body.len() > 200 {
        format!("{preview}...")
    } else {
        preview
    };

    html! {
        article class="group" {
            a href=(paths::show(post.slug.clone()))
               class="block bg-white rounded-xl border border-stone-200 \
                      hover:border-amber-300 shadow-sm hover:shadow-md \
                      transition-all p-6" {
                div class="flex items-center gap-2 mb-3" {
                    time class="text-xs text-stone-500" datetime=(post.created_at.format("%Y-%m-%d")) {
                        (date)
                    }
                    @if !post.published {
                        span class="text-xs px-2 py-0.5 bg-yellow-100 text-yellow-700 rounded-full font-medium" {
                            "Draft"
                        }
                    }
                }
                h2 class="text-xl font-semibold text-stone-900 group-hover:text-amber-700 \
                          transition-colors mb-2" {
                    (post.title)
                }
                p class="text-sm text-stone-500 leading-relaxed" {
                    (preview)
                }
            }
        }
    }
}

/// Look up the message [`NewPost::validate_fields`] recorded against `field`,
/// if any.
fn field_error<'a>(errors: &'a [(&str, &str)], field: &str) -> Option<&'a str> {
    errors
        .iter()
        .find(|(f, _)| *f == field)
        .map(|(_, msg)| *msg)
}

/// Render the post editor form (used for both new and edit).
///
/// `data` is the values to show — a blank/seeded [`NewPost`] on the initial
/// GET, or the author's just-rejected submission on a failed POST — and
/// `errors` is whatever [`NewPost::validate_fields`] found wrong with it
/// (empty on the GET path). Sharing this one function between the GET routes
/// and the POST routes' 422 branch is what makes a rejected submission
/// redisplay with the author's title/slug/body/published choice intact and a
/// message next to the field that caused it, instead of losing the draft to
/// a generic error page.
fn post_form(action: &str, is_edit: bool, data: &NewPost, errors: &[(&str, &str)]) -> Markup {
    let title_error = field_error(errors, "title");
    let body_error = field_error(errors, "body");

    html! {
        form action=(action) method="post"
             class="space-y-6" {
            // Title
            div {
                label for="title" class="block text-sm font-medium text-stone-700 mb-1.5" { "Title" }
                input type="text" id="title" name="title"
                      value=(data.title)
                      required
                      autocomplete="off"
                      placeholder="Your post title"
                      aria-invalid=(if title_error.is_some() { "true" } else { "false" })
                      aria-describedby="title-error"
                      class="w-full px-4 py-2.5 bg-white border border-stone-300 rounded-lg \
                             text-sm placeholder-stone-400 \
                             focus:outline-none focus:ring-2 focus:ring-amber-400/50 \
                             focus:border-amber-400 transition-colors";
                div id="title-error" {
                    @if let Some(msg) = title_error {
                        p class="text-red-600 text-xs mt-1" role="alert" { (msg) }
                    }
                }
            }

            // Slug
            div {
                label for="slug" class="block text-sm font-medium text-stone-700 mb-1.5" { "Slug" }
                input type="text" id="slug" name="slug"
                      value=(data.slug)
                      autocomplete="off"
                      placeholder="auto-generated-from-title"
                      class="w-full px-4 py-2.5 bg-white border border-stone-300 rounded-lg \
                             text-sm placeholder-stone-400 font-mono \
                             focus:outline-none focus:ring-2 focus:ring-amber-400/50 \
                             focus:border-amber-400 transition-colors";
                p class="text-xs text-stone-500 mt-1" { "Leave blank to auto-generate from title" }
            }

            // Body
            div {
                label for="body" class="block text-sm font-medium text-stone-700 mb-1.5" { "Content" }
                textarea id="body" name="body"
                         rows="16"
                         required
                         placeholder="Write your post content here..."
                         aria-invalid=(if body_error.is_some() { "true" } else { "false" })
                         aria-describedby="body-error"
                         class="w-full px-4 py-3 bg-white border border-stone-300 rounded-lg \
                                text-sm placeholder-stone-400 leading-relaxed \
                                focus:outline-none focus:ring-2 focus:ring-amber-400/50 \
                                focus:border-amber-400 transition-colors resize-y" {
                    (data.body)
                }
                div id="body-error" {
                    @if let Some(msg) = body_error {
                        p class="text-red-600 text-xs mt-1" role="alert" { (msg) }
                    }
                }
            }

            // Published toggle (no hidden field — unchecked checkbox is
            // absent from form data; #[serde(default)] handles it as false)
            div class="flex items-center gap-3" {
                input type="checkbox" id="published" name="published" value="true"
                      checked[data.published]
                      class="w-4 h-4 rounded border-stone-300 text-amber-600 \
                             focus:ring-amber-400/50";
                label for="published" class="text-sm text-stone-700" { "Publish immediately" }
            }

            // Submit
            div class="flex items-center gap-3 pt-2" {
                button type="submit"
                       class="px-6 py-2.5 bg-amber-700 text-white text-sm font-medium rounded-lg \
                              shadow-sm hover:bg-amber-800 active:bg-amber-900 \
                              transition-colors" {
                    @if is_edit { "Update Post" } @else { "Create Post" }
                }
                a href=(paths::admin_list())
                   class="px-4 py-2.5 text-sm text-stone-600 hover:text-stone-800 transition-colors" {
                    "Cancel"
                }
            }
        }
    }
}

// ── Public routes ───────────────────────────────────────────────

/// Home page — list published posts.
#[get("/")]
pub async fn index(
    locale: Locale,
    uri: autumn_web::reexports::http::Uri,
    mut db: Db,
) -> AutumnResult<Markup> {
    let published_posts = Post::published(&mut db).await?;

    Ok(layout(
        &locale,
        Some(&path_and_query(&uri)),
        "Autumn Blog",
        html! {
            (hero(
                &HeroConfig::new(&t!(locale, "home.hero.title"))
                    .subtitle(&t!(locale, "home.hero.subtitle"))
            ))

            @if published_posts.is_empty() {
                div class="text-center py-20" {
                    p class="text-stone-500 text-lg mb-2" { "\u{1F343}" }
                    p class="text-stone-500" { "No posts yet. Check back soon!" }
                }
            } @else {
                div class="space-y-4" {
                    @for p in &published_posts {
                        (post_card(p))
                    }
                }
            }
        },
    ))
}

/// View a single published post by slug.
///
/// `og_type` never varies per post, so it is declared once on the route via
/// `seo(...)`; the [`SeoMeta`] extractor hands the handler a builder already
/// carrying it, and the handler layers the per-post title and description on
/// top.
#[get("/posts/{slug}", seo(og_type = "article"))]
pub async fn show(
    locale: Locale,
    uri: autumn_web::reexports::http::Uri,
    slug: Path<String>,
    seo: SeoMeta,
    config: AutumnConfig,
    mut db: Db,
) -> AutumnResult<Markup> {
    let p = Post::find_by_slug(&slug, &mut db).await?;
    let date = p.created_at.format("%B %d, %Y");

    // Simple paragraph rendering — split on double newlines
    let paragraphs: Vec<&str> = p.body.split("\n\n").collect();

    let seo = seo.title(format!("{} • Autumn Blog", p.title)).description(
        p.body
            .split('\n')
            .next()
            .unwrap_or(&p.title)
            .chars()
            .take(160)
            .collect::<String>(),
    );

    // hreflang alternates (issue #1251): `/posts/{slug}` is a dynamic
    // (`#[get]`) route, so — unlike the static `/about` page — it's genuinely
    // reachable at `/en/posts/{slug}` and `/es/posts/{slug}`, and these
    // alternates point at real, working URLs.
    let seo = if let Some(base_url) = config.seo.base_url.as_deref() {
        seo.hreflang_alternates(locale_alternates(
            base_url,
            &format!("/posts/{}", *slug),
            locale.bundle().map_or("en", |b| b.default_locale()),
            locale.bundle().map_or(&[], |b| b.supported_locales()),
        ))
    } else {
        seo
    };

    Ok(layout_with_seo(
        &locale,
        Some(&path_and_query(&uri)),
        seo,
        html! {
            (breadcrumb(&[
                Crumb::link("Blog", &paths::index()),
                Crumb::current(&p.title),
            ]))
            article {

                // Post header
                header class="mb-8" {
                    h1 class="text-3xl font-bold tracking-tight text-stone-900 mb-3" {
                        (p.title)
                    }
                    time class="text-sm text-stone-500" datetime=(p.created_at.format("%Y-%m-%d")) {
                        (date)
                    }
                }

                // Post body
                div class="prose prose-stone max-w-none" {
                    @for paragraph in &paragraphs {
                        @if !paragraph.trim().is_empty() {
                            p class="text-stone-700 leading-relaxed mb-4" { (paragraph.trim()) }
                        }
                    }
                }
            }
        },
    ))
}

// ── Admin routes ────────────────────────────────────────────────

/// Admin dashboard — list all posts (published and drafts).
#[get("/admin")]
pub async fn admin_list(locale: Locale, mut db: Db) -> AutumnResult<Markup> {
    let all_posts = Post::all(&mut db).await?;
    let published_count = all_posts.iter().filter(|p| p.published).count();
    let draft_count = all_posts.len() - published_count;

    Ok(layout(
        &locale,
        None, // `/admin` is excluded from locale-prefixing.
        "Admin \u{2022} Autumn Blog",
        html! {
            header class="mb-8" {
                h1 class="text-2xl font-semibold tracking-tight text-stone-900" {
                    "Manage Posts"
                }
                div class="flex items-center gap-3 mt-2" {
                    span class="text-xs text-stone-500" {
                        (all_posts.len()) " total \u{2022} "
                        (published_count) " published \u{2022} "
                        (draft_count) " drafts"
                    }
                }
            }

            @if all_posts.is_empty() {
                div class="text-center py-16" {
                    p class="text-stone-500 text-sm mb-4" { "No posts yet." }
                    a href=(paths::new_form())
                       class="px-5 py-2.5 bg-amber-700 text-white text-sm font-medium rounded-lg \
                              hover:bg-amber-800 transition-colors" {
                        "Write your first post"
                    }
                }
            } @else {
                div class="space-y-2" {
                    @for p in &all_posts {
                        div id=(format!("post-{}", p.id))
                            class="flex items-center justify-between bg-white rounded-lg \
                                   border border-stone-200 hover:border-stone-300 \
                                   shadow-sm transition-colors px-5 py-4" {
                            div class="flex-1 min-w-0" {
                                div class="flex items-center gap-2" {
                                    a href=(paths::edit_form(p.id))
                                       class="text-sm font-medium text-stone-900 hover:text-amber-700 \
                                              transition-colors truncate" {
                                        (p.title)
                                    }
                                    @if p.published {
                                        span class="shrink-0 text-xs px-2 py-0.5 bg-green-50 text-green-700 \
                                                    rounded-full font-medium" {
                                            "Published"
                                        }
                                    } @else {
                                        span class="shrink-0 text-xs px-2 py-0.5 bg-yellow-50 text-yellow-700 \
                                                    rounded-full font-medium" {
                                            "Draft"
                                        }
                                    }
                                }
                                p class="text-xs text-stone-500 mt-0.5" {
                                    "/" (p.slug) " \u{2022} " (p.created_at.format("%b %d, %Y"))
                                }
                            }
                            div class="flex items-center gap-2 ml-4 shrink-0" {
                                @if p.published {
                                    a href=(paths::show(p.slug.clone()))
                                       class="text-xs text-amber-700 underline hover:text-amber-800 transition-colors" {
                                        "View"
                                    }
                                }
                                a href=(paths::edit_form(p.id))
                                   class="text-xs text-amber-700 underline hover:text-amber-800 transition-colors" {
                                    "Edit"
                                }
                                button hx-delete=(paths::delete_post(p.id))
                                       hx-target=(format!("#post-{}", p.id))
                                       hx-swap="outerHTML"
                                       hx-confirm="Delete this post? This cannot be undone."
                                       class="text-xs text-red-600 underline hover:text-red-700 \
                                              transition-colors cursor-pointer" {
                                    "Delete"
                                }
                            }
                        }
                    }
                }
            }
        },
    ))
}

/// The new-post page body, shared by the GET route and the POST route's 422
/// branch (see [`post_form`]).
fn new_post_page(locale: &Locale, data: &NewPost, errors: &[(&str, &str)]) -> Markup {
    layout(
        locale,
        None, // `/admin` is excluded from locale-prefixing.
        "New Post \u{2022} Autumn Blog",
        html! {
            (breadcrumb(&[
                Crumb::link("Admin", &paths::admin_list()),
                Crumb::current("New Post"),
            ]))
            h1 class="text-2xl font-semibold tracking-tight text-stone-900 mb-6" {
                "New Post"
            }
            (post_form(&paths::create(), false, data, errors))
        },
    )
}

/// Show the new post form.
#[get("/admin/new")]
pub async fn new_form(locale: Locale) -> Markup {
    new_post_page(&locale, &NewPost::default(), &[])
}

/// Create a new post from a form submission.
///
/// On validation failure (empty title or body) the new-post page is
/// re-rendered with the author's draft intact and a message next to the
/// field that failed (422), instead of the generic error page
/// `NewPost::validated`'s `?` used to produce — which dropped the author off
/// the form and discarded both fields they had typed.
#[post("/admin")]
pub async fn create(
    locale: Locale,
    mut db: Db,
    form: Form<NewPost>,
) -> AutumnResult<impl IntoResponse> {
    let submitted = form.0;
    let errors = submitted.validate_fields();
    if !errors.is_empty() {
        return Ok((
            StatusCode::UNPROCESSABLE_ENTITY,
            new_post_page(&locale, &submitted, &errors),
        )
            .into_response());
    }

    let new_post = submitted.normalized();
    diesel::insert_into(posts::table)
        .values(&new_post)
        .execute(&mut *db)
        .await?;

    Ok(Redirect::to(&paths::admin_list()).into_response())
}

/// The edit-post page body, shared by the GET route and the POST route's 422
/// branch (see [`post_form`]). `crumb_title` drives the breadcrumb/`<title>`
/// text: the GET route passes the stored post's title, the POST route's
/// failure branch passes back whatever the author just typed (so the page
/// reflects what's currently in the box rather than a stale DB value).
fn edit_post_page(
    locale: &Locale,
    id: i64,
    crumb_title: &str,
    data: &NewPost,
    errors: &[(&str, &str)],
) -> Markup {
    layout(
        locale,
        None, // `/admin` is excluded from locale-prefixing.
        &format!("Edit: {crumb_title} \u{2022} Autumn Blog"),
        html! {
            (breadcrumb(&[
                Crumb::link("Admin", &paths::admin_list()),
                Crumb::current(&format!("Edit: {crumb_title}")),
            ]))
            h1 class="text-2xl font-semibold tracking-tight text-stone-900 mb-6" {
                "Edit Post"
            }
            (post_form(&paths::update(id), true, data, errors))
        },
    )
}

/// Show the edit form for a post.
#[get("/admin/{id}/edit")]
pub async fn edit_form(locale: Locale, id: Path<i64>, mut db: Db) -> AutumnResult<Markup> {
    let p = Post::find(*id, &mut db).await?;
    let data = NewPost {
        title: p.title.clone(),
        slug: p.slug.clone(),
        body: p.body.clone(),
        published: p.published,
    };

    Ok(edit_post_page(&locale, p.id, &p.title, &data, &[]))
}

/// Update a post from a form submission.
///
/// On validation failure the edit page is re-rendered the same way
/// [`create`] does — see that handler's doc comment; the same anti-pattern
/// applied here via `NewPost::validated`'s `?` on the update path too.
#[post("/admin/{id}")]
pub async fn update(
    locale: Locale,
    id: Path<i64>,
    mut db: Db,
    form: Form<NewPost>,
) -> AutumnResult<impl IntoResponse> {
    let submitted = form.0;
    let errors = submitted.validate_fields();
    if !errors.is_empty() {
        return Ok((
            StatusCode::UNPROCESSABLE_ENTITY,
            edit_post_page(&locale, *id, &submitted.title, &submitted, &errors),
        )
            .into_response());
    }

    let valid = submitted.normalized();
    let changes = UpdatePost {
        title: Some(valid.title),
        slug: Some(valid.slug),
        body: Some(valid.body),
        published: Some(valid.published),
        // Bump the version token so the cached post card re-renders on the
        // next request (Postgres has no ON UPDATE trigger for `updated_at`).
        updated_at: Some(chrono::Utc::now().naive_utc()),
    };

    let updated = diesel::update(posts::table.find(*id))
        .set(&changes)
        .execute(&mut *db)
        .await?;

    if updated == 0 {
        return Err(AutumnError::not_found_msg(format!(
            "Post with id {} not found",
            *id
        )));
    }

    Ok(Redirect::to(&paths::admin_list()).into_response())
}

/// Delete a post by ID (htmx endpoint).
#[delete("/admin/{id}")]
pub async fn delete_post(id: Path<i64>, mut db: Db) -> AutumnResult<String> {
    let deleted = diesel::delete(posts::table.find(*id))
        .execute(&mut *db)
        .await?;

    if deleted == 0 {
        return Err(AutumnError::not_found_msg(format!(
            "Post with id {} not found",
            *id
        )));
    }

    Ok(String::new())
}

autumn_web::paths![
    index,
    show,
    admin_list,
    new_form,
    create,
    edit_form,
    update,
    delete_post
];

/// Error-path coverage for `create`/`update`'s redisplay-on-failure fix
/// (issue: `NewPost::validated`'s `?` used to send every rejected
/// title/body through a generic error page, losing the draft).
#[cfg(test)]
mod post_form_tests {
    use super::*;

    fn blank() -> NewPost {
        NewPost::default()
    }

    #[test]
    fn a_blank_title_is_rejected_with_a_field_message() {
        let form = NewPost {
            title: "   ".into(),
            body: "Some body text".into(),
            ..blank()
        };
        let errors = form.validate_fields();
        assert_eq!(
            field_error(&errors, "title"),
            Some("Title must not be empty")
        );
        assert_eq!(field_error(&errors, "body"), None);
    }

    #[test]
    fn a_blank_body_is_rejected_with_a_field_message() {
        let form = NewPost {
            title: "A real title".into(),
            body: "  \n ".into(),
            ..blank()
        };
        let errors = form.validate_fields();
        assert_eq!(field_error(&errors, "title"), None);
        assert_eq!(field_error(&errors, "body"), Some("Body must not be empty"));
    }

    #[test]
    fn a_fully_populated_post_has_no_errors() {
        let form = NewPost {
            title: "A real title".into(),
            body: "Some body text".into(),
            ..blank()
        };
        assert!(form.validate_fields().is_empty());
    }

    #[test]
    fn normalized_auto_generates_a_slug_from_the_title_when_left_blank() {
        let form = NewPost {
            title: "  Hello World  ".into(),
            slug: String::new(),
            body: "  Body text  ".into(),
            published: true,
        };
        let normalized = form.normalized();
        assert_eq!(normalized.title, "Hello World");
        assert_eq!(normalized.body, "Body text");
        assert_eq!(normalized.slug, "hello-world");
        assert!(normalized.published);
    }

    /// The rejected form redisplay keeps the author's draft and wires each
    /// error to its field (adjacent to cause, aria-invalid, preserved
    /// entered data) instead of dropping them onto a generic error page.
    #[test]
    fn a_rejected_submission_keeps_the_authors_input_and_wires_its_error() {
        let submitted = NewPost {
            title: String::new(),
            slug: "my-custom-slug".into(),
            body: String::new(),
            published: true,
        };
        let errors = submitted.validate_fields();
        let html = post_form(&paths::create(), false, &submitted, &errors).into_string();

        // The author's draft survives the round trip.
        assert!(html.contains(r#"value="my-custom-slug""#), "{html}");
        assert!(
            html.contains(
                r#"input type="checkbox" id="published" name="published" value="true" checked"#
            ),
            "{html}"
        );

        // Both failure modes are wired to their field.
        assert!(html.contains(r#"aria-describedby="title-error""#), "{html}");
        assert!(html.contains(r#"aria-invalid="true""#), "{html}");
        assert!(html.contains("Title must not be empty"), "{html}");
        assert!(html.contains("Body must not be empty"), "{html}");
        assert!(html.contains(r#"role="alert""#), "{html}");
    }

    #[test]
    fn a_clean_form_shows_no_errors_and_aria_invalid_false() {
        let data = NewPost {
            title: "Buy milk".into(),
            body: "Two percent, please.".into(),
            ..blank()
        };
        let html = post_form(&paths::create(), false, &data, &[]).into_string();
        assert!(html.contains(r#"aria-invalid="false""#), "{html}");
        assert!(!html.contains(r#"role="alert""#), "{html}");
        assert!(html.contains(r#"value="Buy milk""#), "{html}");
    }

    #[test]
    fn post_form_labels_the_submit_button_by_is_edit() {
        let data = blank();
        let create_html = post_form(&paths::create(), false, &data, &[]).into_string();
        assert!(create_html.contains("Create Post"), "{create_html}");
        assert!(!create_html.contains("Update Post"), "{create_html}");

        let edit_html = post_form(&paths::update(1), true, &data, &[]).into_string();
        assert!(edit_html.contains("Update Post"), "{edit_html}");
    }
}
