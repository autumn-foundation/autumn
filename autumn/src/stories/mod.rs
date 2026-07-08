//! Widget story gallery: a browsable `/_stories` UI plus a CI anti-rot
//! registry for the built-in maud widgets (issue #1526).
//!
//! Mirrors the mail-preview registry precedent (`crate::mail::MailPreview` /
//! `MailPreviewRegistry`): each story is a zero-arg pure `fn() -> Markup`
//! paired with the source snippet that produced it, collected into a
//! `StoryRegistry` served at `GET /_stories` (grouped index) and
//! `GET /_stories/{slug}` (live render + Source + Rendered HTML tabs).
//!
//! Two switches gate the gallery:
//!
//! 1. **Registration** — `AppBuilder::with_story_gallery(StoryGallery::builtin())`
//!    installs the [`StoryRegistry`](crate::stories::StoryRegistry) on
//!    [`AppState`](crate::AppState).
//! 2. **Config** — the routes mount only when the resolved config has
//!    `[stories] enabled = true` ([`StoriesConfig`](crate::stories::StoriesConfig),
//!    default **false**).
//!    Unlike the dev-only mail preview, stories may be enabled in *any*
//!    profile (a public showcase is a supported use); safe because stories
//!    only ever render synthetic demo data.
//!
//! Author stories with the [`story!`](crate::stories::story) macro — its
//! block is both executed for the live render and captured byte-for-byte as
//! the displayed snippet. See `docs/guide/stories.md`.

use std::sync::Arc;

use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;
use thiserror::Error;

use crate::AppState;

/// Author a widget story: `story!{ "Group", "Name", { ... } }`.
pub use autumn_macros::story;

mod builtin;

/// Stable root path for the widget story gallery.
pub const STORIES_PATH: &str = "/_stories";

/// Route template for a single story's detail page.
const STORY_DETAIL_PATH: &str = "/_stories/{slug}";

/// Derive a URL slug from a story name: lowercase, alphanumeric runs joined
/// by single `-`, everything else (punctuation, whitespace, non-ASCII)
/// treated as a separator.
fn slugify(name: &str) -> String {
    let mut slug = String::with_capacity(name.len());
    let mut pending_separator = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            if pending_separator && !slug.is_empty() {
                slug.push('-');
            }
            pending_separator = false;
            slug.push(c.to_ascii_lowercase());
        } else {
            pending_separator = true;
        }
    }
    slug
}

/// A zero-arg, pure widget render example shown in the `/_stories` gallery.
///
/// Construct with the [`story!`](crate::stories::story) macro, which captures
/// the render block's source text so the displayed snippet is provably the
/// code that rendered. `render` is a plain `fn() -> Markup` pointer: no `Db`,
/// no `AppState`, no request data can be smuggled in.
#[derive(Clone)]
pub struct Story {
    group: &'static str,
    name: &'static str,
    slug: String,
    render: fn() -> maud::Markup,
    source: &'static str,
}

impl std::fmt::Debug for Story {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Story")
            .field("group", &self.group)
            .field("name", &self.name)
            .field("slug", &self.slug)
            .finish_non_exhaustive()
    }
}

impl Story {
    /// Register a story. Prefer the [`story!`](crate::stories::story) macro,
    /// which fills in `source` from the render block automatically.
    ///
    /// # Panics
    ///
    /// Panics when `name` produces an empty URL slug (e.g. a name made
    /// entirely of punctuation) — a programmer error better caught loudly at
    /// construction time than as a broken route.
    #[must_use]
    pub fn new(
        group: &'static str,
        name: &'static str,
        render: fn() -> maud::Markup,
        source: &'static str,
    ) -> Self {
        let slug = slugify(name);
        assert!(
            !slug.is_empty(),
            "story name {name:?} (group {group:?}) produces an empty slug; \
             use a name with at least one alphanumeric character"
        );
        Self {
            group,
            name,
            slug,
            render,
            source,
        }
    }

    /// Sidebar group this story is listed under.
    #[must_use]
    pub const fn group(&self) -> &'static str {
        self.group
    }

    /// Human-readable story name.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// URL slug derived from the name (`/_stories/{slug}`).
    #[must_use]
    pub fn slug(&self) -> &str {
        &self.slug
    }

    /// Source snippet that produced the render (shown in the Source tab).
    #[must_use]
    pub const fn source(&self) -> &'static str {
        self.source
    }

    /// Render the story's markup.
    ///
    /// # Errors
    ///
    /// Returns [`StoryRenderError::Panicked`] when the render function
    /// panics, so one broken story cannot unwind through the gallery.
    pub fn render(&self) -> Result<maud::Markup, StoryRenderError> {
        std::panic::catch_unwind(self.render).map_err(|_| StoryRenderError::Panicked {
            slug: self.slug.clone(),
        })
    }
}

/// Story gallery render errors.
#[derive(Debug, Error)]
pub enum StoryRenderError {
    /// The story's render function panicked.
    #[error("story `{slug}` panicked while rendering")]
    Panicked {
        /// Slug of the panicking story.
        slug: String,
    },
}

/// Immutable collection of registered stories, stored on
/// [`AppState`] as an extension by
/// [`AppBuilder::with_story_gallery`](crate::app::AppBuilder::with_story_gallery).
#[derive(Debug, Clone, Default)]
pub struct StoryRegistry {
    stories: Arc<Vec<Story>>,
}

impl StoryRegistry {
    /// Create a registry from story registrations.
    ///
    /// # Panics
    ///
    /// Panics when two stories share a slug (slugs derive from names), since
    /// one would shadow the other in routing — rename one of the stories.
    #[must_use]
    pub fn new(stories: Vec<Story>) -> Self {
        let mut seen: std::collections::HashMap<&str, &Story> = std::collections::HashMap::new();
        for story in &stories {
            if let Some(existing) = seen.insert(story.slug(), story) {
                panic!(
                    "duplicate story slug `{}`: `{}` / `{}` collides with `{}` / `{}`; \
                     story slugs derive from names, so rename one of them",
                    story.slug(),
                    existing.group(),
                    existing.name(),
                    story.group(),
                    story.name(),
                );
            }
        }
        Self {
            stories: Arc::new(stories),
        }
    }

    /// Registered stories, in registration order.
    #[must_use]
    pub fn stories(&self) -> &[Story] {
        &self.stories
    }

    /// Look a story up by its URL slug.
    fn find(&self, slug: &str) -> Option<&Story> {
        self.stories.iter().find(|story| story.slug() == slug)
    }

    /// Stories grouped for the index sidebar: groups in first-seen order,
    /// stories in registration order within each group (deterministic,
    /// author-controlled).
    fn grouped(&self) -> Vec<(&'static str, Vec<&Story>)> {
        let mut grouped: Vec<(&'static str, Vec<&Story>)> = Vec::new();
        for story in self.stories.iter() {
            match grouped
                .iter_mut()
                .find(|(group, _)| *group == story.group())
            {
                Some((_, stories)) => stories.push(story),
                None => grouped.push((story.group(), vec![story])),
            }
        }
        grouped
    }
}

/// Registry of every built-in widget story: >=1 story per gallery-visible
/// widget in [`crate::widgets`], enforced by the CI coverage gate
/// (`autumn/tests/integration/stories.rs`).
#[must_use]
pub fn builtin() -> StoryRegistry {
    StoryRegistry::new(builtin::builtin_stories())
}

/// Builder-side collection of stories, registered with
/// [`AppBuilder::with_story_gallery`](crate::app::AppBuilder::with_story_gallery).
///
/// Start from [`StoryGallery::builtin`] to serve the framework widget set,
/// or [`StoryGallery::new`] for an app-only gallery, then [`extend`](Self::extend)
/// with stories authored via the [`story!`](crate::stories::story) macro.
#[derive(Debug, Clone, Default)]
pub struct StoryGallery {
    stories: Vec<Story>,
}

impl StoryGallery {
    /// Create an empty, builtin-free gallery (app stories only).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a gallery seeded with every built-in widget story.
    #[must_use]
    pub fn builtin() -> Self {
        Self {
            stories: builtin::builtin_stories(),
        }
    }

    /// Append stories (e.g. custom app widgets authored with
    /// [`story!`](crate::stories::story)).
    #[must_use]
    pub fn extend(mut self, stories: impl IntoIterator<Item = Story>) -> Self {
        self.stories.extend(stories);
        self
    }

    /// Stories collected so far, in registration order.
    #[must_use]
    pub fn stories(&self) -> &[Story] {
        &self.stories
    }

    /// The gallery's axum sub-router (`GET /_stories`, `GET /_stories/{slug}`).
    ///
    /// The framework mounts this automatically when the resolved config has
    /// `[stories] enabled = true`; handlers read the [`StoryRegistry`] from
    /// the [`AppState`] extension installed by
    /// [`AppBuilder::with_story_gallery`](crate::app::AppBuilder::with_story_gallery),
    /// so manual mounters must install that extension too.
    pub fn routes<S>() -> axum::Router<S>
    where
        S: Clone + Send + Sync + 'static,
        AppState: axum::extract::FromRef<S>,
    {
        story_router()
    }

    /// Freeze the gallery into the registry stored on `AppState`.
    ///
    /// # Panics
    ///
    /// Panics on duplicate slugs — see [`StoryRegistry::new`].
    pub(crate) fn into_registry(self) -> StoryRegistry {
        StoryRegistry::new(self.stories)
    }
}

/// Widget story gallery settings (`[stories]` section in `autumn.toml`).
///
/// **Off by default, opt-in in any profile** — unlike the dev-only mail
/// preview, a public showcase is a supported use (stories only ever render
/// synthetic demo data). Profile overrides come free from the standard
/// config layering, and `AUTUMN_STORIES__ENABLED` overrides from the
/// environment:
///
/// ```toml
/// [stories]
/// enabled = false
///
/// # Dev-only gallery: mounted under `autumn dev`, 404 in prod.
/// [profile.dev.stories]
/// enabled = true
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
pub struct StoriesConfig {
    /// Whether the `/_stories` gallery routes are mounted. Default `false`.
    #[serde(default)]
    pub enabled: bool,
}

/// Build the gallery sub-router. Mounted by `build_router` when
/// `config.stories.enabled` is true; handlers read the [`StoryRegistry`]
/// from the `AppState` extension (mail-preview precedent).
pub(crate) fn story_router<S>() -> axum::Router<S>
where
    S: Clone + Send + Sync + 'static,
    AppState: axum::extract::FromRef<S>,
{
    axum::Router::new()
        .route(
            STORIES_PATH,
            axum::routing::get(
                |axum::extract::State(state): axum::extract::State<AppState>,
                 nonce: Option<crate::security::CspNonce>| async move {
                    story_index_response(&state, nonce.as_ref())
                },
            ),
        )
        .route(
            STORY_DETAIL_PATH,
            axum::routing::get(
                |axum::extract::Path(slug): axum::extract::Path<String>,
                 axum::extract::State(state): axum::extract::State<AppState>,
                 nonce: Option<crate::security::CspNonce>| async move {
                    story_detail_response(&state, &slug, nonce.as_ref())
                },
            ),
        )
}

fn registry_from_state(state: &AppState) -> StoryRegistry {
    state
        .extension::<StoryRegistry>()
        .map(|registry| (*registry).clone())
        .unwrap_or_default()
}

fn story_index_response(state: &AppState, nonce: Option<&crate::security::CspNonce>) -> Response {
    Html(render_story_index(&registry_from_state(state), nonce).into_string()).into_response()
}

fn story_detail_response(
    state: &AppState,
    slug: &str,
    nonce: Option<&crate::security::CspNonce>,
) -> Response {
    let registry = registry_from_state(state);
    let Some(story) = registry.find(slug) else {
        let page = story_page(
            "Story not found",
            &maud::html! {
                main class="story-content" {
                    h1 { "Story not found" }
                    p {
                        "No story is registered under the slug " code { (slug) } ". "
                        a href=(STORIES_PATH) { "Back to the gallery" }
                    }
                }
            },
            nonce,
        );
        return (http::StatusCode::NOT_FOUND, Html(page.into_string())).into_response();
    };

    match story.render() {
        Ok(rendered) => {
            Html(render_story_detail(story, &rendered, nonce).into_string()).into_response()
        }
        Err(error) => {
            let page = story_page(
                "Story failed to render",
                &maud::html! {
                    main class="story-content" {
                        h1 { "Story failed to render" }
                        p { (error.to_string()) }
                        p { a href=(STORIES_PATH) { "Back to the gallery" } }
                    }
                },
                nonce,
            );
            (
                http::StatusCode::INTERNAL_SERVER_ERROR,
                Html(page.into_string()),
            )
                .into_response()
        }
    }
}

/// Minimal gallery chrome layered on top of the framework widget stylesheet.
const STORY_GALLERY_CSS: &str = r"
body { margin: 0; font-family: system-ui, sans-serif; color: #1f2933; }
.story-layout { display: flex; gap: 2rem; align-items: flex-start; }
.story-sidebar { flex: 0 0 14rem; padding: 1rem 1.25rem; border-right: 1px solid #e0e0e0; min-height: 100vh; }
.story-sidebar h2 { font-size: 0.8rem; text-transform: uppercase; letter-spacing: 0.05em; color: #616e7c; margin: 1.25rem 0 0.25rem; }
.story-sidebar ul { list-style: none; margin: 0; padding: 0; }
.story-sidebar li { margin: 0.15rem 0; }
.story-content { flex: 1 1 auto; padding: 1.5rem; max-width: 60rem; }
.story-preview { padding: 1.5rem; border: 1px solid #e0e0e0; border-radius: 6px; margin-bottom: 1.5rem; }
.story-content pre { background: #f5f7fa; border-radius: 6px; padding: 1rem; overflow-x: auto; }
.story-empty { padding: 2rem; border: 1px dashed #cbd2d9; border-radius: 6px; }
";

/// Full HTML document shell: framework widget stylesheet + gallery chrome.
///
/// When the security layer's per-request CSP nonce is active
/// (`security.headers.csp_nonce.enabled = true`, which drops
/// `'unsafe-inline'` from `style-src`), the inline gallery stylesheet carries
/// the request's nonce so browsers don't block it; without the layer no
/// `nonce` attribute is emitted.
fn story_page(
    title: &str,
    body: &maud::Markup,
    nonce: Option<&crate::security::CspNonce>,
) -> maud::Markup {
    maud::html! {
        (maud::DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " — Autumn stories" }
                link rel="stylesheet" href=(crate::ui::WIDGETS_CSS_PATH);
                style nonce=[nonce.map(crate::security::CspNonce::value)] {
                    (maud::PreEscaped(STORY_GALLERY_CSS))
                }
            }
            body { (body) }
        }
    }
}

/// Render the grouped `/_stories` index page.
fn render_story_index(
    registry: &StoryRegistry,
    nonce: Option<&crate::security::CspNonce>,
) -> maud::Markup {
    story_page(
        "Widget stories",
        &maud::html! {
            div class="story-layout" {
                nav class="story-sidebar" aria-label="Stories" {
                    @for (group, stories) in registry.grouped() {
                        h2 { (group) }
                        ul {
                            @for story in stories {
                                li {
                                    a href=(format!("{STORIES_PATH}/{}", story.slug())) {
                                        (story.name())
                                    }
                                }
                            }
                        }
                    }
                }
                main class="story-content" {
                    h1 { "Widget stories" }
                    @if registry.stories().is_empty() {
                        div class="story-empty" {
                            p {
                                "No stories are registered. Add "
                                code { ".with_story_gallery(StoryGallery::builtin())" }
                                " to your " code { "AppBuilder" }
                                " to serve the built-in widget gallery, or "
                                code { "StoryGallery::new().extend([...])" }
                                " for app-only stories."
                            }
                        }
                    } @else {
                        p {
                            "Every entry renders live from a zero-arg widget example; "
                            "its detail page shows the exact source that produced it. "
                            "Pick a story from the sidebar."
                        }
                    }
                }
            }
        },
        nonce,
    )
}

/// Render a single story's detail page: live render above Source and
/// Rendered HTML tabs (dogfooding the [`crate::widgets::tabs`] widget).
///
/// `rendered` is the story's markup, rendered exactly once by the caller
/// (which also owns the render-failure response), so the preview and the
/// Rendered HTML tab always show the same output.
fn render_story_detail(
    story: &Story,
    rendered: &maud::Markup,
    nonce: Option<&crate::security::CspNonce>,
) -> maud::Markup {
    let rendered_html = rendered.clone().into_string();
    story_page(
        story.name(),
        &maud::html! {
            main class="story-content" {
                p class="story-breadcrumb" {
                    a href=(STORIES_PATH) { "Widget stories" }
                    " / " (story.group())
                }
                h1 { (story.name()) }
                section class="story-preview" { (rendered) }
                (crate::widgets::tabs(
                    "story-tabs",
                    &[
                        (
                            "story-source",
                            "Source",
                            maud::html! { pre { code { (story.source()) } } },
                        ),
                        (
                            "story-html",
                            "Rendered HTML",
                            maud::html! { pre { code { (rendered_html) } } },
                        ),
                    ],
                ))
            }
        },
        nonce,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn demo_markup() -> maud::Markup {
        maud::html! { p { "demo" } }
    }

    fn demo_story(group: &'static str, name: &'static str) -> Story {
        Story::new(group, name, demo_markup, r#"maud::html! { p { "demo" } }"#)
    }

    // U1 (AC1): slug derivation folds case, joins alphanumeric runs with `-`,
    // and treats punctuation/whitespace/non-ASCII as separators.
    #[test]
    fn slugify_handles_spaces_punctuation_unicode() {
        assert_eq!(slugify("Data table"), "data-table");
        assert_eq!(slugify("Nav / Links!"), "nav-links");
        assert_eq!(slugify("HERO"), "hero");
        // Non-ASCII acts as a separator and never leaks into the URL.
        assert_eq!(slugify("Δ delta"), "delta");
        assert_eq!(slugify("  Stat   Card  "), "stat-card");
    }

    // U1 (AC1): a name that slugifies to nothing is a programmer error caught
    // loudly at construction time, not a broken route.
    #[test]
    #[should_panic(expected = "slug")]
    fn story_new_panics_on_name_that_slugifies_to_empty() {
        let _ = Story::new("Display", "!!!", demo_markup, "demo()");
    }

    // U2 (AC1): lookup is by slug.
    #[test]
    fn registry_find_matches_slug() {
        let registry = StoryRegistry::new(vec![
            demo_story("Display", "Data table"),
            demo_story("Display", "Card"),
        ]);
        assert_eq!(registry.stories().len(), 2);
        let found = registry
            .find("data-table")
            .expect("data-table slug should resolve");
        assert_eq!(found.name(), "Data table");
        assert_eq!(found.group(), "Display");
        assert!(registry.find("nope").is_none());
    }

    // U2 (AC1, R7): duplicate slugs would let one story shadow another in
    // routing — refuse them at registry construction.
    #[test]
    #[should_panic(expected = "duplicate")]
    fn registry_new_panics_on_duplicate_slug() {
        let _ = StoryRegistry::new(vec![
            demo_story("Display", "Card"),
            demo_story("Marketing", "Card"),
        ]);
    }

    // U3 (AC1, R17): index grouping is deterministic — groups in first-seen
    // order, stories in registration order within a group.
    #[test]
    fn grouped_preserves_first_seen_group_and_registration_order() {
        let registry = StoryRegistry::new(vec![
            demo_story("Display", "Data table"),
            demo_story("Forms", "Active search"),
            demo_story("Display", "Card"),
        ]);
        let grouped = registry.grouped();
        let groups: Vec<&str> = grouped.iter().map(|(group, _)| *group).collect();
        assert_eq!(groups, ["Display", "Forms"]);
        let display_names: Vec<&str> = grouped[0].1.iter().map(|s| s.name()).collect();
        assert_eq!(display_names, ["Data table", "Card"]);
    }

    // U4 (AC7): builder-side gallery — builtin-free constructor, seeded
    // constructor, and extend with a custom `story!`.
    #[test]
    fn gallery_new_is_empty_builtin_is_seeded_extend_appends() {
        assert!(
            StoryGallery::new().into_registry().stories().is_empty(),
            "StoryGallery::new() must start builtin-free"
        );

        let builtin_count = builtin().stories().len();
        assert!(builtin_count > 0, "builtin registry must not be empty");
        assert_eq!(
            StoryGallery::builtin().into_registry().stories().len(),
            builtin_count,
            "StoryGallery::builtin() must be seeded with exactly the builtin stories"
        );

        let custom = crate::stories::story! {
            "App",
            "Badge",
            {
                maud::html! { span { "hi" } }
            }
        };
        let registry = StoryGallery::builtin().extend([custom]).into_registry();
        assert_eq!(registry.stories().len(), builtin_count + 1);
        assert!(
            registry.find("badge").is_some(),
            "extended custom story must be findable by its slug"
        );
    }

    fn panicking_story() -> maud::Markup {
        panic!("story exploded on purpose")
    }

    // U5 (AC8, R4): a panicking story surfaces as an error, it does not
    // unwind through the gallery.
    #[test]
    fn story_render_catches_panic() {
        let boom = Story::new("Display", "Boom", panicking_story, "panicking_story()");
        let err = boom
            .render()
            .expect_err("panic must be caught and reported as an error");
        assert!(
            matches!(err, StoryRenderError::Panicked { .. }),
            "expected StoryRenderError::Panicked, got {err:?}"
        );

        let fine = Story::new("Display", "Fine", demo_markup, "demo()");
        assert!(fine.render().is_ok());
    }

    // U6 (AC4): the index page links the framework widget stylesheet and
    // lists stories grouped in the sidebar.
    #[test]
    fn index_page_links_widgets_css_and_groups_stories() {
        assert_eq!(STORIES_PATH, "/_stories");

        let registry = StoryRegistry::new(vec![
            demo_story("Display", "Data table"),
            demo_story("Forms", "Active search"),
        ]);
        let page = render_story_index(&registry, None).into_string();
        let dom = crate::test_html::parse(&page);

        let css_selector = crate::test_html::SelectorList::parse(&format!(
            "link[href=\"{}\"]",
            crate::ui::WIDGETS_CSS_PATH
        ))
        .expect("selector parses");
        assert!(
            !css_selector.matches(&dom).is_empty(),
            "index must link the framework widget stylesheet: {page}"
        );

        let link_selector =
            crate::test_html::SelectorList::parse("a[href=\"/_stories/data-table\"]")
                .expect("selector parses");
        assert!(
            !link_selector.matches(&dom).is_empty(),
            "index must link each story detail page: {page}"
        );

        assert!(
            page.contains("Display") && page.contains("Forms"),
            "sidebar must show group headings: {page}"
        );
    }

    // U7 (AC4): the detail page shows the live render plus Source and
    // Rendered HTML tabs (dogfooding the `tabs` widget).
    #[test]
    fn detail_page_has_live_render_source_tab_and_html_tab() {
        let story = crate::stories::story! {
            "Display",
            "Proof",
            {
                maud::html! { p class="proof-marker" { "live proof" } }
            }
        };
        let rendered = story.render().expect("proof story renders");
        let page = render_story_detail(&story, &rendered, None).into_string();
        let dom = crate::test_html::parse(&page);

        let preview = crate::test_html::SelectorList::parse(".story-preview .proof-marker")
            .expect("selector parses");
        let matches = preview.matches(&dom);
        assert!(
            !matches.is_empty(),
            "live render must appear inside .story-preview: {page}"
        );
        assert!(
            matches[0].text().contains("live proof"),
            "live render must contain the story's output: {page}"
        );

        let tabs = crate::test_html::SelectorList::parse(".autumn-tabs").expect("selector parses");
        assert!(
            !tabs.matches(&dom).is_empty(),
            "detail page must use the tabs widget for Source / Rendered HTML: {page}"
        );

        assert!(
            page.contains("maud::html!"),
            "Source tab must show the captured snippet: {page}"
        );
        assert!(
            page.contains("&lt;p"),
            "Rendered HTML tab must show the escaped markup: {page}"
        );
    }

    // U8 (AC4/AC5, R12): enabled-but-unregistered renders a helpful empty
    // state pointing at AppBuilder::with_story_gallery, not a 500/blank page.
    #[test]
    fn index_empty_state_mentions_with_story_gallery() {
        let page = render_story_index(&StoryRegistry::default(), None).into_string();
        assert!(
            page.contains("with_story_gallery"),
            "empty state must explain how to register stories: {page}"
        );
    }

    // ── Strict balanced-HTML check over the gallery chrome ──────────────
    //
    // The integration harness (tests/integration/stories.rs) runs this
    // discipline over each story *fragment*; the full index/detail pages can
    // only be rendered here (`render_story_*` are private), so a minimal
    // copy of the strict checker lives in this module too. Unlike the
    // lenient `crate::test_html` parser, it refuses auto-closing.

    /// Panics when `html` is not well-formed (mismatched, unclosed, or
    /// stray-closed tags).
    fn assert_balanced_html(html: &str, context: &str) {
        const VOID_ELEMENTS: &[&str] = &[
            "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param",
            "source", "track", "wbr",
        ];
        const RAW_TEXT_ELEMENTS: &[&str] = &["script", "style", "textarea", "title"];

        let bytes = html.as_bytes();
        let mut stack: Vec<String> = Vec::new();
        let mut i = 0;

        while i < bytes.len() {
            if bytes[i] != b'<' {
                i += 1;
                continue;
            }
            if html[i..].starts_with("<!--") {
                let end = html[i + 4..]
                    .find("-->")
                    .unwrap_or_else(|| panic!("unterminated comment in {context}:\n{html}"));
                i += 4 + end + 3;
                continue;
            }
            if html[i..].starts_with("<!") {
                // Doctype or other declaration.
                let end = html[i..]
                    .find('>')
                    .unwrap_or_else(|| panic!("unterminated declaration in {context}:\n{html}"));
                i += end + 1;
                continue;
            }

            let closing = html[i..].starts_with("</");
            let name_start = i + if closing { 2 } else { 1 };
            let name_len = html[name_start..]
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
                .unwrap_or(html.len() - name_start);
            let name = html[name_start..name_start + name_len].to_ascii_lowercase();
            assert!(
                !name.is_empty(),
                "malformed tag at byte {i} in {context}:\n{html}"
            );

            // Find the true end of the tag, skipping `>` inside quoted attrs.
            let mut j = name_start + name_len;
            let mut quote: Option<u8> = None;
            while j < bytes.len() {
                match (quote, bytes[j]) {
                    (Some(q), c) if c == q => quote = None,
                    (None, b'"') => quote = Some(b'"'),
                    (None, b'\'') => quote = Some(b'\''),
                    (None, b'>') => break,
                    _ => {}
                }
                j += 1;
            }
            assert!(
                j < bytes.len(),
                "unterminated tag <{name} in {context}:\n{html}"
            );
            let self_closing = j > 0 && bytes[j - 1] == b'/';

            if closing {
                let open = stack.pop().unwrap_or_else(|| {
                    panic!("closing </{name}> with no open tag in {context}:\n{html}")
                });
                assert_eq!(
                    open, name,
                    "mismatched close tag in {context}: expected </{open}>, found </{name}>:\n{html}"
                );
                i = j + 1;
                continue;
            }

            if !self_closing && !VOID_ELEMENTS.contains(&name.as_str()) {
                if RAW_TEXT_ELEMENTS.contains(&name.as_str()) {
                    // Skip raw content up to the matching close tag.
                    let close = format!("</{name}");
                    let rest_start = j + 1;
                    let end = html[rest_start..]
                        .to_ascii_lowercase()
                        .find(&close)
                        .unwrap_or_else(|| {
                            panic!("unclosed raw-text element <{name}> in {context}:\n{html}")
                        });
                    let close_gt = html[rest_start + end..]
                        .find('>')
                        .unwrap_or_else(|| panic!("unterminated </{name}> in {context}:\n{html}"));
                    i = rest_start + end + close_gt + 1;
                    continue;
                }
                stack.push(name);
            }
            i = j + 1;
        }

        assert!(
            stack.is_empty(),
            "unclosed tags {stack:?} in {context}:\n{html}"
        );
    }

    // Guard the duplicated checker itself: a broken checker must not
    // green-light rotten gallery chrome.
    #[test]
    fn balanced_html_checker_rejects_malformed_markup() {
        assert_balanced_html(
            r#"<!DOCTYPE html><div class="a > b"><p>ok<br></p></div>"#,
            "self-test",
        );
        for bad in ["<div><p></div>", "<div>", "</div>"] {
            let result = std::panic::catch_unwind(|| assert_balanced_html(bad, "self-test"));
            assert!(result.is_err(), "checker should reject {bad}");
        }
    }

    // U9 (AC4/AC8): the gallery chrome itself — grouped index, empty-state
    // index, and every builtin detail page — is strictly balanced HTML, not
    // just the story fragments the integration harness checks.
    #[test]
    fn gallery_index_and_detail_pages_render_balanced_html() {
        let registry = builtin();
        assert_balanced_html(
            &render_story_index(&registry, None).into_string(),
            "story index page",
        );
        assert_balanced_html(
            &render_story_index(&StoryRegistry::default(), None).into_string(),
            "empty-state index page",
        );
        for story in registry.stories() {
            let rendered = story
                .render()
                .unwrap_or_else(|err| panic!("builtin story `{}` failed: {err}", story.slug()));
            assert_balanced_html(
                &render_story_detail(story, &rendered, None).into_string(),
                &format!("detail page for `{}`", story.slug()),
            );
        }
    }

    // U10 (review follow-up): when the security layer's per-request CSP nonce
    // is active, `style-src` drops `'unsafe-inline'`, so the gallery's inline
    // stylesheet must carry the request nonce or browsers block it. Without
    // the layer no nonce attribute is emitted.
    #[test]
    fn story_pages_apply_csp_nonce_to_inline_style() {
        let nonce = crate::security::CspNonce::new_for_tests("test-nonce-value");
        let registry = StoryRegistry::new(vec![demo_story("Display", "Card")]);

        let index = render_story_index(&registry, Some(&nonce)).into_string();
        assert!(
            index.contains(r#"<style nonce="test-nonce-value">"#),
            "index inline style must carry the CSP nonce: {index}"
        );

        let story = &registry.stories()[0];
        let rendered = story.render().expect("demo story renders");
        let detail = render_story_detail(story, &rendered, Some(&nonce)).into_string();
        assert!(
            detail.contains(r#"<style nonce="test-nonce-value">"#),
            "detail inline style must carry the CSP nonce: {detail}"
        );

        let plain = render_story_index(&registry, None).into_string();
        assert!(
            plain.contains("<style>") && !plain.contains("nonce="),
            "without the security layer no nonce attribute is emitted: {plain}"
        );
    }
}
