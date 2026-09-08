//! The theme layer: front-end chrome, the content pipeline, menus and widgets.
//!
//! WordPress's template hierarchy is a filename convention — `single.php`,
//! `archive-product.php`, `category-rust.php` — resolved at runtime by trying
//! paths until one exists. That is flexible and completely unverifiable: a
//! typo produces a silent fallback to `index.php`.
//!
//! Here a theme is a value implementing [`Theme`]: the hierarchy is a set of
//! methods with defaults, so a theme overrides what it wants and the compiler
//! guarantees the rest exists. Switching themes is a settings change; the
//! active one is resolved per request from `settings.active_theme`.

use autumn_web::prelude::*;

use crate::content_types;
use crate::models::{Menu, MenuItem, Post, Term, User, Widget};
use crate::plugins::{Filter, apply_filters};
use crate::settings::Settings;
use crate::shortcodes;

// ── The content pipeline ────────────────────────────────────────────────────

/// Turn an author's Markdown into the HTML a page renders.
///
/// The order is deliberate and is the security-relevant part:
///
/// 1. **Markdown → HTML, sanitized.** `render_user_content` runs the output
///    through an allowlist sanitizer, so a `<script>` an author pasted into the
///    editor never survives — whether they are a trusted Editor or a
///    Contributor whose account was taken over.
/// 2. **Shortcodes expand.** They emit raw HTML *after* sanitization because a
///    shortcode handler is trusted application code, not user input; running
///    them first would mean the sanitizer stripping their markup.
/// 3. **`the_content` filters.** Plugins get the last word, exactly as in
///    WordPress.
#[must_use]
pub fn render_content(markdown: &str) -> PreEscaped<String> {
    let sanitized = autumn_web::markdown::render_user_content_html(markdown);
    let expanded = shortcodes::expand(&sanitized);
    PreEscaped(apply_filters(Filter::TheContent, expanded))
}

/// A post title, passed through the `the_title` filter. Escaped on render.
#[must_use]
pub fn render_title(title: &str) -> String {
    apply_filters(Filter::TheTitle, title.to_owned())
}

/// The `<title>` element's text: the page title, the separator and the site
/// title, then the `document_title` filter.
#[must_use]
pub fn document_title(page_title: &str, settings: &Settings) -> String {
    let raw = if page_title.trim().is_empty() {
        settings.site_title.clone()
    } else {
        format!("{page_title} · {}", settings.site_title)
    };
    apply_filters(Filter::DocumentTitle, raw)
}

// ── Navigation menus ────────────────────────────────────────────────────────

/// A resolved menu entry, with its target URL already computed.
#[derive(Debug, Clone)]
pub struct NavNode {
    pub label: String,
    pub url: String,
    pub children: Vec<NavNode>,
}

/// Assemble flat menu-item rows into the nested structure the nav renders.
///
/// `resolve` turns an item into its URL — it needs the permalink structure and,
/// for post-targeted items, the post's ancestry, so it is supplied by the
/// caller rather than computed here.
#[must_use]
pub fn build_nav(items: &[MenuItem], resolve: &impl Fn(&MenuItem) -> String) -> Vec<NavNode> {
    fn walk(
        items: &[MenuItem],
        parent: Option<i64>,
        depth: usize,
        resolve: &impl Fn(&MenuItem) -> String,
    ) -> Vec<NavNode> {
        // Two levels is what a horizontal nav can show; a deeper tree is a
        // sitemap, and rendering it as a dropdown-inside-a-dropdown helps
        // nobody. The bound also makes a cyclic `parent_id` (only reachable by
        // direct write) terminate instead of recursing forever.
        if depth > 2 {
            return Vec::new();
        }
        let mut nodes: Vec<&MenuItem> = items.iter().filter(|i| i.parent_id == parent).collect();
        nodes.sort_by_key(|i| (i.position, i.id));
        nodes
            .into_iter()
            .map(|item| NavNode {
                label: item.label.clone(),
                url: resolve(item),
                children: walk(items, Some(item.id), depth + 1, resolve),
            })
            .collect()
    }
    walk(items, None, 0, resolve)
}

// ── Widgets ─────────────────────────────────────────────────────────────────

/// A widget kind the theme knows how to render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WidgetKind {
    /// A list of the most recent published posts.
    RecentPosts,
    /// The category list, with post counts.
    Categories,
    /// A tag cloud.
    TagCloud,
    /// A block of author-written Markdown.
    Text,
    /// The site search form.
    Search,
}

impl WidgetKind {
    /// Parse a stored `widgets.kind` value.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "recent_posts" => Some(Self::RecentPosts),
            "categories" => Some(Self::Categories),
            "tag_cloud" => Some(Self::TagCloud),
            "text" => Some(Self::Text),
            "search" => Some(Self::Search),
            _ => None,
        }
    }

    /// The stored value.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::RecentPosts => "recent_posts",
            Self::Categories => "categories",
            Self::TagCloud => "tag_cloud",
            Self::Text => "text",
            Self::Search => "search",
        }
    }

    /// The label shown in the widget picker.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::RecentPosts => "Recent Posts",
            Self::Categories => "Categories",
            Self::TagCloud => "Tag Cloud",
            Self::Text => "Text",
            Self::Search => "Search",
        }
    }

    /// Every kind, for the widget picker.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[
            Self::RecentPosts,
            Self::Categories,
            Self::TagCloud,
            Self::Text,
            Self::Search,
        ]
    }
}

/// The data a sidebar render needs, loaded once by the caller.
pub struct SidebarData {
    pub widgets: Vec<Widget>,
    pub recent_posts: Vec<(String, String)>,
    pub categories: Vec<Term>,
    pub tags: Vec<Term>,
}

/// Render a sidebar's widgets in order.
#[must_use]
pub fn render_sidebar(data: &SidebarData) -> Markup {
    let mut ordered: Vec<&Widget> = data.widgets.iter().collect();
    ordered.sort_by_key(|w| (w.position, w.id));

    html! {
        @for widget in &ordered {
            @if let Some(kind) = WidgetKind::parse(&widget.kind) {
                section class="mb-8" aria-labelledby=(format!("widget-{}", widget.id)) {
                    @if !widget.title.trim().is_empty() {
                        h2 id=(format!("widget-{}", widget.id))
                           class="text-sm font-semibold uppercase tracking-wide text-gray-500 mb-3" {
                            (widget.title)
                        }
                    }
                    (render_widget(kind, widget, data))
                }
            }
        }
    }
}

fn render_widget(kind: WidgetKind, widget: &Widget, data: &SidebarData) -> Markup {
    match kind {
        WidgetKind::RecentPosts => {
            // `count` is the widget instance's own setting; an absent or
            // nonsense value falls back rather than rendering nothing.
            let limit = widget
                .settings
                .get("count")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(5)
                .clamp(1, 20) as usize;
            html! {
                ul class="space-y-2 text-sm" {
                    @for (title, url) in data.recent_posts.iter().take(limit) {
                        li { a href=(url) class="text-indigo-700 hover:underline" { (title) } }
                    }
                    @if data.recent_posts.is_empty() {
                        li class="text-gray-400" { "No posts yet." }
                    }
                }
            }
        }
        WidgetKind::Categories => html! {
            ul class="space-y-1 text-sm" {
                @for term in &data.categories {
                    li {
                        a href=(format!("/category/{}", term.slug))
                          class="text-indigo-700 hover:underline" { (term.name) }
                        span class="text-gray-400" { " (" (term.post_count) ")" }
                    }
                }
                @if data.categories.is_empty() {
                    li class="text-gray-400" { "No categories yet." }
                }
            }
        },
        WidgetKind::TagCloud => html! {
            div class="flex flex-wrap gap-2" {
                @for term in &data.tags {
                    a href=(format!("/tag/{}", term.slug))
                      class="px-2 py-0.5 bg-gray-100 rounded text-xs text-gray-700 hover:bg-gray-200" {
                        (term.name)
                    }
                }
                @if data.tags.is_empty() {
                    span class="text-gray-400 text-sm" { "No tags yet." }
                }
            }
        },
        WidgetKind::Text => {
            let body = widget
                .settings
                .get("text")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            html! { div class="prose prose-sm max-w-none" { (render_content(body)) } }
        }
        WidgetKind::Search => html! {
            form action="/search" method="get" role="search" class="flex gap-2" {
                label for=(format!("widget-search-{}", widget.id)) class="sr-only" { "Search" }
                input id=(format!("widget-search-{}", widget.id)) type="search" name="s"
                      placeholder="Search…" class="flex-1 border rounded px-2 py-1 text-sm";
                button type="submit"
                       class="px-3 py-1 bg-indigo-600 text-white rounded text-sm hover:bg-indigo-700" {
                    "Go"
                }
            }
        },
    }
}

// ── The theme ───────────────────────────────────────────────────────────────

/// Everything the site chrome needs, resolved once per request.
pub struct Chrome {
    pub settings: Settings,
    pub nav: Vec<NavNode>,
    pub sidebar: Option<Markup>,
    pub current_user: Option<User>,
    /// The hidden CSRF input for the chrome's own logout form.
    ///
    /// Carried on the chrome rather than resolved inside the layout because a
    /// theme is a plain function of its inputs — giving it an extractor would
    /// make every theme method a handler.
    pub csrf: Markup,
}

/// A front-end theme.
///
/// Every method has a default, so a theme is `impl Theme for MyTheme {}` plus
/// whatever it wants to change — the equivalent of a WordPress child theme
/// overriding one template file, without the silent-fallback failure mode.
pub trait Theme: Send + Sync {
    /// The slug stored in `settings.active_theme`.
    fn slug(&self) -> &'static str;

    /// The name shown on the Appearance screen.
    fn name(&self) -> &'static str;

    /// The site chrome: everything outside the page's own content.
    fn layout(&self, chrome: &Chrome, page_title: &str, content: Markup) -> Markup {
        default_layout(chrome, page_title, content)
    }

    /// One post as it appears in a listing. WordPress's `content.php`.
    fn post_card(&self, post: &Post, url: &str, settings: &Settings) -> Markup {
        default_post_card(post, url, settings)
    }
}

/// The theme a fresh install runs.
pub struct DefaultTheme;

impl Theme for DefaultTheme {
    fn slug(&self) -> &'static str {
        "default"
    }
    fn name(&self) -> &'static str {
        "Autumn Default"
    }
}

/// A second registered theme, so the Appearance screen's switcher has something
/// to switch to and the seam is exercised rather than merely described.
pub struct MinimalTheme;

impl Theme for MinimalTheme {
    fn slug(&self) -> &'static str {
        "minimal"
    }
    fn name(&self) -> &'static str {
        "Minimal"
    }

    /// Drops the excerpt and the metadata line — a title-and-date index.
    fn post_card(&self, post: &Post, url: &str, settings: &Settings) -> Markup {
        html! {
            article class="py-3 border-b border-gray-100 flex items-baseline justify-between gap-4" {
                a href=(url) class="text-lg text-indigo-700 hover:underline" {
                    (render_title(&post.title))
                }
                @if let Some(published) = post.published_at {
                    time datetime=(published.and_utc().to_rfc3339())
                         class="text-xs text-gray-400 shrink-0" {
                        (settings.format_date(published))
                    }
                }
            }
        }
    }
}

/// Every registered theme.
#[must_use]
pub fn registered_themes() -> Vec<&'static dyn Theme> {
    vec![&DefaultTheme, &MinimalTheme]
}

/// The active theme, or the default when the stored slug names none.
#[must_use]
pub fn active_theme(settings: &Settings) -> &'static dyn Theme {
    registered_themes()
        .into_iter()
        .find(|theme| theme.slug() == settings.active_theme)
        .unwrap_or(&DefaultTheme)
}

fn default_post_card(post: &Post, url: &str, settings: &Settings) -> Markup {
    html! {
        article class="py-6 border-b border-gray-100 last:border-0" {
            h2 class="text-xl font-semibold mb-1" {
                a href=(url) class="text-gray-900 hover:text-indigo-700" {
                    (render_title(&post.title))
                }
                @if post.sticky {
                    span class="ml-2 align-middle px-1.5 py-0.5 text-[10px] uppercase tracking-wide \
                                bg-amber-100 text-amber-800 rounded" { "Featured" }
                }
            }
            p class="text-xs text-gray-500 mb-2" {
                @if let Some(published) = post.published_at {
                    time datetime=(published.and_utc().to_rfc3339()) {
                        (settings.format_date(published))
                    }
                }
                @if post.comment_count > 0 {
                    " · " (autumn_web::format::pluralize(post.comment_count, "comment"))
                }
            }
            p class="text-gray-700" { (post.display_excerpt()) }
        }
    }
}

fn default_layout(chrome: &Chrome, page_title: &str, content: Markup) -> Markup {
    let settings = &chrome.settings;
    html! {
        (maud::DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (document_title(page_title, settings)) }
                meta name="description" content=(settings.tagline);
                link rel="stylesheet" href="/static/css/app.css";
                link rel="alternate" type="application/atom+xml"
                     title=(format!("{} · Atom feed", settings.site_title)) href="/feed";
                link rel="alternate" type="application/rss+xml"
                     title=(format!("{} · RSS feed", settings.site_title)) href="/feed/rss";
                script src=(autumn_web::htmx::HTMX_JS_PATH) {}
            }
            body class="bg-white text-gray-900" {
                a href="#main-content"
                  class="sr-only focus:not-sr-only focus:absolute focus:top-2 focus:left-2 \
                         focus:z-50 focus:px-4 focus:py-2 focus:bg-white focus:border \
                         focus:border-gray-300 focus:rounded focus:shadow" {
                    "Skip to main content"
                }
                header class="border-b border-gray-200" {
                    div class="max-w-5xl mx-auto px-4 py-6" {
                        div class="flex items-baseline justify-between gap-4 flex-wrap" {
                            div {
                                a href="/" class="text-2xl font-bold tracking-tight" {
                                    (settings.site_title)
                                }
                                @if !settings.tagline.trim().is_empty() {
                                    p class="text-sm text-gray-500 mt-0.5" { (settings.tagline) }
                                }
                            }
                            div class="text-sm flex items-center gap-3" {
                                @match &chrome.current_user {
                                    Some(user) => {
                                        @if user.role().can_access_admin() {
                                            a href="/admin" class="text-indigo-700 hover:underline" {
                                                "Dashboard"
                                            }
                                        }
                                        span class="text-gray-400" { (user.public_name()) }
                                        form action="/logout" method="post" class="inline" {
                                            (chrome.csrf)
                                            button type="submit"
                                                   class="text-gray-600 hover:text-gray-900" {
                                                "Log out"
                                            }
                                        }
                                    }
                                    None => {
                                        a href="/login" class="text-gray-600 hover:text-gray-900" {
                                            "Log in"
                                        }
                                    }
                                }
                            }
                        }
                        @if !chrome.nav.is_empty() {
                            nav aria-label="Primary" class="mt-4" {
                                ul class="flex flex-wrap gap-5 text-sm" {
                                    @for node in &chrome.nav {
                                        li class="relative group" {
                                            a href=(node.url)
                                              class="text-gray-700 hover:text-indigo-700" {
                                                (node.label)
                                            }
                                            @if !node.children.is_empty() {
                                                ul class="mt-1 ml-3 space-y-1 text-xs text-gray-500" {
                                                    @for child in &node.children {
                                                        li {
                                                            a href=(child.url)
                                                              class="hover:text-indigo-700" {
                                                                (child.label)
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                div class="max-w-5xl mx-auto px-4 py-8 flex flex-col md:flex-row gap-10" {
                    main id="main-content" class="flex-1 min-w-0" { (content) }
                    @if let Some(sidebar) = &chrome.sidebar {
                        aside class="w-full md:w-64 shrink-0" aria-label="Sidebar" { (sidebar) }
                    }
                }

                footer class="border-t border-gray-200 mt-12" {
                    div class="max-w-5xl mx-auto px-4 py-6 text-xs text-gray-500 flex \
                               flex-wrap gap-3 justify-between" {
                        span { "© " (chrono::Utc::now().format("%Y").to_string()) " " (settings.site_title) }
                        span {
                            a href="/feed" class="hover:underline" { "Atom" }
                            " · "
                            a href="/sitemap.xml" class="hover:underline" { "Sitemap" }
                            " · Powered by Autumn"
                        }
                    }
                }
            }
        }
    }
}

/// The URL of a term archive.
#[must_use]
pub fn term_url(term: &Term) -> String {
    let base = content_types::find_taxonomy(&term.taxonomy)
        .map_or_else(|| term.taxonomy.clone(), |t| t.rewrite_base.to_owned());
    format!("/{base}/{}", term.slug)
}

/// The URL of a menu, used by the admin's menu list.
#[must_use]
pub fn menu_label(menu: &Menu) -> String {
    if menu.location.is_empty() {
        menu.name.clone()
    } else {
        format!("{} ({})", menu.name, menu.location)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: i64, parent: Option<i64>, label: &str, position: i32) -> MenuItem {
        MenuItem {
            id,
            menu_id: 1,
            parent_id: parent,
            label: label.to_owned(),
            url: format!("/{label}"),
            post_id: None,
            term_id: None,
            position,
        }
    }

    #[test]
    fn nav_nests_children_under_parents_in_position_order() {
        let items = vec![
            item(1, None, "second", 20),
            item(2, None, "first", 10),
            item(3, Some(2), "child-b", 20),
            item(4, Some(2), "child-a", 10),
        ];
        let nav = build_nav(&items, &|i| i.url.clone());
        assert_eq!(nav.len(), 2);
        assert_eq!(nav[0].label, "first");
        assert_eq!(nav[1].label, "second");
        let labels: Vec<&str> = nav[0].children.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, vec!["child-a", "child-b"]);
    }

    #[test]
    fn a_cyclic_menu_terminates_instead_of_recursing_forever() {
        // Only reachable by a direct write, but the renderer must survive it.
        let items = vec![item(1, Some(2), "a", 0), item(2, Some(1), "b", 0)];
        let nav = build_nav(&items, &|i| i.url.clone());
        assert!(nav.is_empty(), "a cycle has no root, so nothing renders");
    }

    #[test]
    fn content_pipeline_sanitizes_before_shortcodes_expand() {
        // The author's `<script>` must not survive; a registered shortcode's
        // HTML must, because it runs after sanitization.
        shortcodes::add_shortcode("themebox", |_| "<hr class=\"themed\">".to_owned());
        let rendered =
            render_content("Hello <script>alert(1)</script>\n\n[themebox]").into_string();
        assert!(!rendered.contains("<script"), "rendered: {rendered}");
        assert!(
            rendered.contains("<hr class=\"themed\">"),
            "rendered: {rendered}"
        );
    }

    #[test]
    fn document_title_falls_back_to_the_site_title() {
        let settings = Settings::default();
        assert_eq!(document_title("", &settings), settings.site_title);
        assert_eq!(
            document_title("About", &settings),
            format!("About · {}", settings.site_title)
        );
    }

    #[test]
    fn an_unknown_active_theme_falls_back_to_the_default() {
        let mut settings = Settings {
            active_theme: "deleted-theme".to_owned(),
            ..Settings::default()
        };
        assert_eq!(active_theme(&settings).slug(), "default");
        settings.active_theme = "minimal".to_owned();
        assert_eq!(active_theme(&settings).slug(), "minimal");
    }

    #[test]
    fn widget_kind_slugs_round_trip() {
        for kind in WidgetKind::all() {
            assert_eq!(WidgetKind::parse(kind.slug()), Some(*kind));
        }
        assert_eq!(WidgetKind::parse("nonexistent"), None);
    }
}
