//! HTML routes for the blog application.
//!
//! These routes render Maud templates styled with Tailwind CSS and
//! use htmx attributes for interactive publish/delete behaviour.

use autumn_web::assets::asset_url;
use autumn_web::cache::cache_fragment_global;
use autumn_web::extract::{Form, Path};
use autumn_web::i18n::Locale;
use autumn_web::seo::SeoMeta;
use autumn_web::widgets::{Crumb, HeroConfig, breadcrumb, hero};
use autumn_web::{AutumnError, AutumnResult, Db, Markup, Redirect, delete, get, html, post, t};
use diesel::prelude::*;
use diesel_async::RunQueryDsl;

use crate::models::{NewPost, Post, UpdatePost};
use crate::schema::posts;

// ── Layout ──────────────────────────────────────────────────────

/// Base HTML layout wrapping page content.
///
/// Takes the request [`Locale`] so nav, footer, and locale-switcher labels
/// are translated through the [`t!`] macro. Pages reachable via
/// `#[static_get]` (e.g. `about.rs`) use the same extractor during static
/// rendering, so pre-rendered HTML receives the configured bundle too.
///
/// Accepts an optional [`SeoMeta`] to inject per-page meta tags. Falls back
/// to a sensible site-wide description when omitted.
pub fn layout(locale: &Locale, title: &str, content: Markup) -> Markup {
    layout_with_seo(
        locale,
        SeoMeta::new()
            .title(title)
            .description("A blog built with the Autumn web framework for Rust."),
        content,
    )
}

/// Layout variant accepting an explicit [`SeoMeta`] builder.
pub fn layout_with_seo(locale: &Locale, seo: SeoMeta, content: Markup) -> Markup {
    html! {
        (autumn_web::PreEscaped("<!DOCTYPE html>"))
        html lang=(locale.tag()) {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                (seo.render())
                link rel="stylesheet" href=(asset_url("css/autumn.css"));
                script src=(asset_url("js/htmx.min.js")) {}
            }
            body class="bg-stone-50 min-h-screen font-sans text-stone-800 antialiased" {
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
                            // Lightweight locale switcher — `?locale=` query
                            // overrides the resolved locale per the documented
                            // resolution order.
                            span class="text-xs text-stone-400 ml-2" { (t!(locale, "nav.locale.label")) ":" }
                            a href="?locale=en" class="text-xs text-stone-600 hover:text-amber-700" { (t!(locale, "nav.locale.en")) }
                            a href="?locale=es" class="text-xs text-stone-600 hover:text-amber-700" { (t!(locale, "nav.locale.es")) }
                        }
                    }
                }

                // Main content
                main class="max-w-3xl mx-auto py-10 px-6" {
                    (content)
                }

                // Footer
                footer class="border-t border-stone-200 mt-16" {
                    div class="max-w-3xl mx-auto text-center text-xs text-stone-500 py-8" {
                        (t!(locale, "footer.tagline"))
                        " \u{2022} "
                        a href="https://github.com/madmax983/autumn"
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

/// Render the post editor form (used for both new and edit).
fn post_form(action: &str, post: Option<&Post>) -> Markup {
    let title_val = post.map_or("", |p| &p.title);
    let slug_val = post.map_or("", |p| &p.slug);
    let body_val = post.map_or("", |p| &p.body);
    let published = post.is_some_and(|p| p.published);

    html! {
        form action=(action) method="post"
             class="space-y-6" {
            // Title
            div {
                label for="title" class="block text-sm font-medium text-stone-700 mb-1.5" { "Title" }
                input type="text" id="title" name="title"
                      value=(title_val)
                      required
                      autocomplete="off"
                      placeholder="Your post title"
                      class="w-full px-4 py-2.5 bg-white border border-stone-300 rounded-lg \
                             text-sm placeholder-stone-400 \
                             focus:outline-none focus:ring-2 focus:ring-amber-400/50 \
                             focus:border-amber-400 transition-colors";
            }

            // Slug
            div {
                label for="slug" class="block text-sm font-medium text-stone-700 mb-1.5" { "Slug" }
                input type="text" id="slug" name="slug"
                      value=(slug_val)
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
                         class="w-full px-4 py-3 bg-white border border-stone-300 rounded-lg \
                                text-sm placeholder-stone-400 leading-relaxed \
                                focus:outline-none focus:ring-2 focus:ring-amber-400/50 \
                                focus:border-amber-400 transition-colors resize-y" {
                    (body_val)
                }
            }

            // Published toggle (no hidden field — unchecked checkbox is
            // absent from form data; #[serde(default)] handles it as false)
            div class="flex items-center gap-3" {
                input type="checkbox" id="published" name="published" value="true"
                      checked[published]
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
                    @if post.is_some() { "Update Post" } @else { "Create Post" }
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
pub async fn index(locale: Locale, mut db: Db) -> AutumnResult<Markup> {
    let published_posts = Post::published(&mut db).await?;

    Ok(layout(
        &locale,
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
#[get("/posts/{slug}")]
pub async fn show(locale: Locale, slug: Path<String>, mut db: Db) -> AutumnResult<Markup> {
    let p = Post::find_by_slug(&slug, &mut db).await?;
    let date = p.created_at.format("%B %d, %Y");

    // Simple paragraph rendering — split on double newlines
    let paragraphs: Vec<&str> = p.body.split("\n\n").collect();

    let seo = SeoMeta::new()
        .title(format!("{} • Autumn Blog", p.title))
        .description(
            p.body
                .split('\n')
                .next()
                .unwrap_or(&p.title)
                .chars()
                .take(160)
                .collect::<String>(),
        )
        .og_type("article");

    Ok(layout_with_seo(
        &locale,
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

/// Show the new post form.
#[get("/admin/new")]
pub async fn new_form(locale: Locale) -> Markup {
    layout(
        &locale,
        "New Post \u{2022} Autumn Blog",
        html! {
            (breadcrumb(&[
                Crumb::link("Admin", &paths::admin_list()),
                Crumb::current("New Post"),
            ]))
            h1 class="text-2xl font-semibold tracking-tight text-stone-900 mb-6" {
                "New Post"
            }
            (post_form(&paths::create(), None))
        },
    )
}

/// Create a new post from a form submission.
#[post("/admin")]
pub async fn create(mut db: Db, form: Form<NewPost>) -> AutumnResult<Redirect> {
    let new_post = form.0.validated()?;

    diesel::insert_into(posts::table)
        .values(&new_post)
        .execute(&mut *db)
        .await?;

    Ok(Redirect::to(&paths::admin_list()))
}

/// Show the edit form for a post.
#[get("/admin/{id}/edit")]
pub async fn edit_form(locale: Locale, id: Path<i64>, mut db: Db) -> AutumnResult<Markup> {
    let p = Post::find(*id, &mut db).await?;

    Ok(layout(
        &locale,
        &format!("Edit: {} \u{2022} Autumn Blog", p.title),
        html! {
            (breadcrumb(&[
                Crumb::link("Admin", &paths::admin_list()),
                Crumb::current(&format!("Edit: {}", p.title)),
            ]))
            h1 class="text-2xl font-semibold tracking-tight text-stone-900 mb-6" {
                "Edit Post"
            }
            (post_form(&paths::update(p.id), Some(&p)))
        },
    ))
}

/// Update a post from a form submission.
#[post("/admin/{id}")]
pub async fn update(id: Path<i64>, mut db: Db, form: Form<NewPost>) -> AutumnResult<Redirect> {
    let validated = form.0.validated()?;

    let changes = UpdatePost {
        title: Some(validated.title),
        slug: Some(validated.slug),
        body: Some(validated.body),
        published: Some(validated.published),
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

    Ok(Redirect::to(&paths::admin_list()))
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
