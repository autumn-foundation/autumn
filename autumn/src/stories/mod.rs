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

/// Demo backend for the "Active search" story. See [`demo_search`].
const STORIES_DEMO_SEARCH_PATH: &str = "/_stories/demo/search";

/// Demo backend for the "Autocomplete" story. See [`demo_tag_search`].
const STORIES_DEMO_TAG_SEARCH_PATH: &str = "/_stories/demo/tags/search";

/// Demo backend for the "Infinite feed" story's sentinel. See
/// [`demo_infinite_feed`].
const STORIES_DEMO_FEED_PATH: &str = "/_stories/demo/posts/feed";

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
        // Demo backends for the two typeahead stories (Active search,
        // Autocomplete) — see `demo_search`/`demo_tag_search` below for why
        // these two specifically get real handlers where every other
        // story's action URL stays a synthetic 404-on-submit (already the
        // documented pattern for e.g. Confirm action / Avatar). Namespaced
        // under `/_stories/demo/*`, the same convention `confirm_action`'s
        // story already uses (`/_stories/demo/delete-post`), so a real app's
        // own routes can never collide with these.
        .route(STORIES_DEMO_SEARCH_PATH, axum::routing::get(demo_search))
        .route(
            STORIES_DEMO_TAG_SEARCH_PATH,
            axum::routing::get(demo_tag_search),
        )
        .route(
            STORIES_DEMO_FEED_PATH,
            axum::routing::get(demo_infinite_feed),
        )
}

/// Query params `demo_search` and `demo_tag_search` both accept — the `q`
/// param [`ActiveSearchConfig`](crate::widgets::ActiveSearchConfig) and
/// [`AutocompleteConfig`](crate::widgets::AutocompleteConfig) both default
/// `param_name`/`query_param` to.
#[derive(serde::Deserialize)]
struct DemoQuery {
    #[serde(default)]
    q: String,
}

/// Demo backend for the "Active search" story's action — mounted at
/// [`STORIES_DEMO_SEARCH_PATH`]. Filters a small fixed post list
/// case-insensitively by `q`, returning the same
/// [`active_search_empty_state`](crate::widgets::active_search_empty_state)
/// the story documents for the no-matches case.
async fn demo_search(
    crate::extract::Query(DemoQuery { q }): crate::extract::Query<DemoQuery>,
) -> Html<String> {
    const POSTS: &[&str] = &[
        "The Long Autumn",
        "Falling Leaves",
        "Autumn in Practice",
        "Shipping Widgets",
    ];
    let query = q.to_lowercase();
    let matches: Vec<&str> = POSTS
        .iter()
        .copied()
        .filter(|title| query.is_empty() || title.to_lowercase().contains(&query))
        .collect();
    let markup = if matches.is_empty() {
        crate::widgets::active_search_empty_state("No posts matched your search.")
    } else {
        maud::html! {
            ul class="story-demo-results" {
                @for title in matches {
                    li { (title) }
                }
            }
        }
    };
    Html(markup.into_string())
}

/// Demo backend for the "Autocomplete" story's `/tags/search`-shaped action
/// — mounted at [`STORIES_DEMO_TAG_SEARCH_PATH`]. Filters a small fixed tag
/// list case-insensitively by the `q` query param, rendering matches with
/// the same [`autocomplete_option`](crate::widgets::autocomplete_option) /
/// [`autocomplete_empty_state`](crate::widgets::autocomplete_empty_state)
/// the story's source already documents.
async fn demo_tag_search(
    crate::extract::Query(DemoQuery { q }): crate::extract::Query<DemoQuery>,
) -> Html<String> {
    const TAGS: &[(&str, &str)] = &[("42", "rust"), ("7", "web"), ("13", "axum"), ("21", "htmx")];
    let query = q.to_lowercase();
    let matches: Vec<(&str, &str)> = TAGS
        .iter()
        .copied()
        .filter(|(_, label)| query.is_empty() || label.to_lowercase().contains(&query))
        .collect();
    let markup = if matches.is_empty() {
        crate::widgets::autocomplete_empty_state("No matching tags.")
    } else {
        maud::html! {
            @for (value, label) in matches {
                (crate::widgets::autocomplete_option(value, label))
            }
        }
    };
    Html(markup.into_string())
}

/// Demo backend for the "Infinite feed" story's sentinel — mounted at
/// [`STORIES_DEMO_FEED_PATH`].
///
/// Exists only so loading `htmx.min.js` (needed for the two typeahead
/// demos above) doesn't turn this *specific* story into a visible
/// regression: unlike every other action URL in the gallery, this one's
/// [`FeedMode::Reveal`](crate::widgets::FeedMode::Reveal) sentinel fires
/// `hx-trigger="revealed, click"` — htmx auto-requests it the moment the
/// element scrolls into view, no click required, so visiting the story page
/// alone would have 404-swapped it without the visitor doing anything. This
/// always returns a terminal page (no further cursor), matching what the
/// story's own `feed_page` snippet already documents as "the last page".
async fn demo_infinite_feed() -> Html<String> {
    let markup = crate::widgets::feed_page(
        maud::html! {
            article class="post" { h3 { "Loaded live via a real request." } }
        },
        None,
        &crate::widgets::FeedConfig::new(STORIES_DEMO_FEED_PATH),
    );
    Html(markup.into_string())
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

/// Gallery chrome layered on top of the framework widget stylesheet
/// (`WIDGETS_CSS_PATH`, linked before this `<style>` in `<head>`, so this
/// block's declarations win the cascade at equal specificity).
///
/// Themed with the shared [`crate::ui::tokens`] custom properties
/// (`var(--bg)`, `var(--primary)`, …) rather than hardcoded grays, so the
/// gallery reads as a real Autumn surface instead of bare unstyled HTML —
/// and so every `autumn-*` widget previewed inside a story re-themes too,
/// since custom properties inherit down from `body`.
///
/// The `body { --bg: …; }` block below sets the gallery's own default
/// ("Autumn": warm, not the framework's violet default) by overriding the
/// tokens locally; it doesn't touch `tokens.css` itself, so apps embedding
/// these widgets elsewhere are unaffected. [`theme_switch`] adds three more
/// presets, swapped live by the same tokens — see its doc comment.
const STORY_GALLERY_CSS: &str = r"
body {
    margin: 0;
    font-family: var(--font-family);
    color: var(--text);
    background: var(--bg);
    transition: background-color 0.15s ease, border-color 0.15s ease, color 0.15s ease;

    /* The pale first-draft --border and --primary here both failed contrast
       (review follow-up, verified independently across all three light
       themes, none of it yet flagged when this comment was written):
       --border read at ~1.3:1 against --surface in every one of them
       (pastel-on-near-white, picked to look subtle, not checked against
       what it sits on) — WCAG 1.4.11 wants 3:1. And unlike Midnight,
       darkening --primary here has no downside to weigh: --primary-light
       is the *light* end of the pair already, so a darker --primary only
       ever increases contrast, for both the white-text button case and
       the primary-on-primary-light case (e.g. #c2540a on #fbe4cd was
       3.74:1, short of text's 4.5:1). No role conflict like Midnight's —
       just needed darkening. Ocean and Forest below get the same two
       fixes, values picked the same way. */
    --bg: #fdf8f2;
    --surface: #fffaf4;
    --text: #2b1c10;
    --text-muted: #7a6a58;
    --border: #8a6f52;
    --primary: #a8470c;
    --primary-hover: #83380a;
    --primary-light: #fbe4cd;
    --radius: 0.6rem;
    --shadow: 0 1px 3px rgba(43, 28, 16, 0.12), 0 4px 14px rgba(43, 28, 16, 0.08);
}

body:has(#story-theme-ocean:checked) {
    --bg: #f0f7fb; --surface: #ffffff; --text: #0f2a3d; --text-muted: #52717f;
    --border: #6b93a6; --primary: #0a5f78; --primary-hover: #084c60; --primary-light: #d7f0f5;
    --shadow: 0 1px 3px rgba(15, 42, 61, 0.12), 0 4px 14px rgba(15, 42, 61, 0.08);
}
body:has(#story-theme-forest:checked) {
    --bg: #f3f8f1; --surface: #ffffff; --text: #1b2e18; --text-muted: #5c7256;
    --border: #5c8a52; --primary: #256829; --primary-hover: #1d5320; --primary-light: #dcefdb;
    --shadow: 0 1px 3px rgba(27, 46, 24, 0.12), 0 4px 14px rgba(27, 46, 24, 0.08);
}
body:has(#story-theme-midnight:checked) {
    /* Reuses tokens.css's own default violet (not a lighter #8b7cf6-family
       shade) because white text sits directly on --primary throughout the
       widget set (checked pills, buttons, …): #7c3aed keeps that pairing at
       ~5.7:1 contrast, comfortably above WCAG AA's 4.5:1 for normal text —
       a lighter violet dropped as low as 3.33:1 (review follow-up).

       --primary-light also needed its own override here, NOT a darker tint
       to match this theme's dark surfaces: text=var(--primary) on
       bg=var(--primary-light) (hovered sidebar links, the selected
       autocomplete option) needs the *opposite* of the button pairing above
       — with primary that dark, only a light background clears 4.5:1
       (review follow-up: the dark tint this carried before was 2.46:1). So
       this reuses tokens.css's own default --primary-light (#ede9fe)
       unchanged — the identical pairing already proven in the light theme
       — rendering as a deliberately bright chip against the dark chrome,
       ~4.8:1.

       --danger stays at tokens.css's own default (#dc2626,
       --danger-hover #b91c1c, --danger-light #fee2e2, all unset here) for
       the identical reason: `.autumn-modal__confirm--danger` fills with
       --danger under white text, the same button-fill role --primary
       plays above, so it needs to stay dark (review follow-up: lightening
       it to fix the Comment thread error text below dropped the confirm
       button to 2.77:1). --danger-light is the same primary-light-chip
       role as above too — dark text (--danger) on a light chip is already
       proven in the light theme, so it also stays at the default.

       --surface-muted isn't a tokens.css variable at all — widgets.css's
       badge component reads it as `var(--surface-muted, #f1f5f9)`, a
       standalone fallback nothing else declares. Every other theme here
       leaves it unset and gets that light fallback, which is fine paired
       with their (dark-ish, light-surface-appropriate) --text-muted. Dark
       mode needs its own dark fallback for the same reason --primary-light
       needed its own value above: --text-muted is light here, so it
       reads at only 2.64:1 on the undeclared light default (review
       follow-up).

       --border needed lightening too: form controls (e.g.
       .autumn-autocomplete__input) sit on --surface with only this border
       as their visible boundary, and the original tint (matched to the
       other dark surfaces, not against them) was 1.26:1 — WCAG 1.4.11
       wants 3:1 for a UI component boundary, not the 4.5:1 text threshold
       above (review follow-up). */
    --bg: #14151c; --surface: #1d1f2b; --text: #e7e7ee; --text-muted: #9497ab;
    --border: #666b8c; --primary: #7c3aed; --primary-hover: #6d28d9; --primary-light: #ede9fe;
    --surface-muted: #23253a;
    --shadow: 0 1px 3px rgba(0, 0, 0, 0.4), 0 4px 14px rgba(0, 0, 0, 0.3);
}
/* --primary (#7c3aed) and --danger (#dc2626, from tokens.css's default,
   unset above) are each also the *only* colors widgets.css ships for text
   and graphical marks that aren't inside a button or a chip — e.g.
   `.autumn-feed__more`, `.autumn-comment-reply-toggle`, this file's own
   `.story-breadcrumb a` (all `color: var(--primary)`), the chart line/
   point/bar SVG marks (`stroke`/`fill: var(--primary)` — deliberately, per
   widgets.css's own comment: 'charts re-theme by overriding --primary'),
   and `.autumn-comments-error` (`color: var(--danger)`) — a role with no
   token of its own, alongside the button-fill role above and the
   *-light-chip role before it. All three roles read the same variable but
   need different lightness in dark mode: button-fill wants it dark (for
   white text), plain text/marks want it light (for surface contrast) —
   mutually exclusive for one shared value (review follow-up, three
   rounds: --primary at the button-fill shade was 2.87:1 on --surface for
   text and, separately, 2.87:1 again for the chart marks, which only need
   WCAG 1.4.11's 3:1 graphical-object threshold rather than 4.5:1 but still
   fell short; --danger lightened to fix its own text case promptly broke
   its confirm button the same way, 2.77:1). Giving plain text/marks their
   own token is a widgets.css-wide change well past this PR's scope of
   theming the gallery, so this overrides plain-text-color usage instead —
   every `color: var(--primary)` rule in widgets.css that isn't paired
   with `background: var(--primary-light)` (that pairing is the
   already-fixed chip role, e.g. the selected autocomplete option) — a
   real fix for the actual gap, not a one-off patch for whichever
   selector a story happened to render and review happened to catch
   first (four of these eight were found that way, across four rounds).
   See PR #2887 for the proposed follow-up (dedicated text/mark-color
   tokens in tokens.css, so this whole list stops needing upkeep by
   hand). */
body:has(#story-theme-midnight:checked) .autumn-feed__more,
body:has(#story-theme-midnight:checked) .autumn-comment-reply-toggle,
body:has(#story-theme-midnight:checked) .autumn-nav__item a:hover,
body:has(#story-theme-midnight:checked) .autumn-locale-switcher a:hover,
body:has(#story-theme-midnight:checked) .autumn-breadcrumb__link:hover,
body:has(#story-theme-midnight:checked) .autumn-reaction-active,
body:has(#story-theme-midnight:checked) .wizard-step--completed .wizard-step__label,
body:has(#story-theme-midnight:checked) .story-breadcrumb a {
    color: #a78bfa;
}
body:has(#story-theme-midnight:checked) .autumn-reaction-active {
    border-color: #a78bfa;
}
body:has(#story-theme-midnight:checked) .autumn-comments-error {
    color: #f87171;
    border-color: #f87171;
}
/* Charts read `var(--primary)` directly for every stroke/fill (by design —
   see the comment above), so redeclaring the custom property itself,
   scoped to `.autumn-chart`, re-themes every mark inside it in one
   declaration instead of overriding each `.autumn-chart__*` rule. */
body:has(#story-theme-midnight:checked) .autumn-chart {
    --primary: #a78bfa;
}

.story-theme-switch { display: flex; align-items: center; gap: 0.4rem; padding: 0.6rem 1.25rem; border-bottom: 1px solid var(--border); background: var(--surface); }
.story-theme-switch legend { font-size: 0.75rem; text-transform: uppercase; letter-spacing: 0.05em; color: var(--text-muted); margin-right: 0.25rem; padding: 0; }
.story-theme-switch input[type='radio'] { position: absolute; opacity: 0; width: 1px; height: 1px; }
.story-theme-switch label { padding: 0.3rem 0.75rem; border-radius: 999px; border: 1px solid var(--border); font-size: 0.8rem; line-height: 1; cursor: pointer; color: var(--text-muted); }
.story-theme-switch input[type='radio']:checked + label { background: var(--primary); border-color: var(--primary); color: #fff; }
.story-theme-switch input[type='radio']:focus-visible + label { outline: 2px solid var(--primary); outline-offset: 2px; }

.story-layout { display: flex; gap: 2rem; align-items: flex-start; }
.story-sidebar { flex: 0 0 14rem; padding: 1.25rem; border-right: 1px solid var(--border); min-height: 100vh; background: var(--surface); }
.story-sidebar h2 { font-size: 0.75rem; text-transform: uppercase; letter-spacing: 0.05em; color: var(--text-muted); margin: 1.5rem 0 0.35rem; }
.story-sidebar h2:first-child { margin-top: 0; }
.story-sidebar ul { list-style: none; margin: 0; padding: 0; }
.story-sidebar li { margin: 0.15rem 0; }
.story-sidebar a { color: var(--text); text-decoration: none; border-radius: 0.35rem; padding: 0.15rem 0.4rem; display: block; }
.story-sidebar a:hover { background: var(--primary-light); color: var(--primary); }
.story-content { flex: 1 1 auto; padding: 1.75rem 2rem 3rem; max-width: 60rem; }
.story-content h1 { margin-top: 0; }
.story-breadcrumb { color: var(--text-muted); font-size: 0.85rem; }
.story-breadcrumb a { color: var(--primary); }
.story-preview { padding: 1.75rem; border: 1px solid var(--border); border-radius: var(--radius); margin-bottom: 1.5rem; background: var(--surface); box-shadow: var(--shadow); }
.story-content pre { background: var(--bg); border: 1px solid var(--border); border-radius: var(--radius); padding: 1rem; overflow-x: auto; }
.story-empty { padding: 2rem; border: 1px dashed var(--border); border-radius: var(--radius); color: var(--text-muted); }
";

/// The gallery's theme presets: `(radio value / id suffix, visible label)`.
/// `"autumn"` is the default (checked server-side so the page never renders
/// themeless before CSS applies).
const STORY_THEMES: [(&str, &str); 4] = [
    ("autumn", "Autumn"),
    ("ocean", "Ocean"),
    ("forest", "Forest"),
    ("midnight", "Midnight"),
];

/// Live theme switcher: a `fieldset` of radio buttons that re-theme the
/// whole page — including every `autumn-*` widget previewed in a story —
/// with **no JavaScript**.
///
/// Each radio's `:checked` state is matched by a `body:has(#story-theme-…
/// :checked)` rule in [`STORY_GALLERY_CSS`] that overrides the shared
/// design tokens (`--bg`, `--primary`, …); because custom properties
/// inherit, every rule referencing `var(--primary)` — in this stylesheet
/// *and* in the linked `WIDGETS_CSS_PATH` bundle — re-resolves live the
/// moment a different radio is checked. Same `:has()`-driven, script-free
/// pattern as the alert widget's dismiss toggle and the tabs widget's
/// `:target` deep-linking; safe under a strict `script-src 'self'` CSP with
/// no `'unsafe-inline'` and no nonce, since nothing here executes.
fn theme_switch() -> maud::Markup {
    maud::html! {
        fieldset class="story-theme-switch" {
            legend { "Theme" }
            @for (value, label) in STORY_THEMES {
                input type="radio" name="story-theme" id=(format!("story-theme-{value}"))
                    checked[value == "autumn"];
                label for=(format!("story-theme-{value}")) { (label) }
            }
        }
    }
}

/// Full HTML document shell: framework widget stylesheet + htmx + widget
/// runtime script + gallery chrome.
///
/// Loads two same-origin scripts (both always mounted under the `htmx`
/// feature), in this order so `defer` preserves it regardless of download
/// timing:
///
/// 1. `htmx.min.js` — every `hx-get`/`hx-post`-driven widget (active search,
///    autocomplete, reaction controls, comment threads, the infinite feed
///    sentinel, …) renders correct `hx-*` attributes either way (see each
///    story's "Rendered HTML" tab), but they stay inert without htmx itself
///    on the page to process them.
/// 2. `autumn-widgets.js` — the framework's own `data-*`-hook runtime for
///    `modal_trigger`, `confirm_action`, `nav_bar`, and the autocomplete
///    widget's selection wiring.
///
/// Neither script needs a CSP nonce: the framework's default policy keeps
/// `'self'` in `script-src` in both plain and nonce modes.
///
/// Most widgets' action URLs stay synthetic 404s-on-submit by design (e.g.
/// Confirm action, Bulk actions) — see each story's own comment. Two
/// typeahead stories (Active search, Autocomplete) get real demo backends
/// instead (`demo_search`, `demo_tag_search`), since a search box that
/// visibly does nothing while typing reads as broken rather than as a
/// deliberately-inert mockup; the Infinite feed story gets one too
/// (`demo_infinite_feed`) purely to stop its auto-firing `revealed` trigger
/// from 404-swapping on page load — see that function's doc comment.
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
    #[cfg(feature = "htmx")]
    let scripts: &[&str] = &[
        crate::htmx::HTMX_JS_PATH,
        crate::htmx::AUTUMN_WIDGETS_JS_PATH,
    ];
    #[cfg(not(feature = "htmx"))]
    let scripts: &[&str] = &[];
    maud::html! {
        (maud::DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " — Autumn stories" }
                link rel="stylesheet" href=(crate::ui::WIDGETS_CSS_PATH);
                @for src in scripts {
                    script src=(src) defer {}
                }
                style nonce=[nonce.map(crate::security::CspNonce::value)] {
                    (maud::PreEscaped(STORY_GALLERY_CSS))
                }
            }
            body {
                (theme_switch())
                (body)
            }
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
                    None,
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
            "Greeting",
            {
                maud::html! { span { "hi" } }
            }
        };
        let registry = StoryGallery::builtin().extend([custom]).into_registry();
        assert_eq!(registry.stories().len(), builtin_count + 1);
        assert!(
            registry.find("greeting").is_some(),
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

    // U6 (AC4): the index page links the framework widget stylesheet, loads
    // the widget runtime script, and lists stories grouped in the sidebar.
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

        #[cfg(feature = "htmx")]
        {
            let js_selector = crate::test_html::SelectorList::parse(&format!(
                "script[src=\"{}\"]",
                crate::htmx::AUTUMN_WIDGETS_JS_PATH
            ))
            .expect("selector parses");
            assert!(
                !js_selector.matches(&dom).is_empty(),
                "index must load the widget runtime script so interactive \
                 stories (modal, confirm-action, nav-bar) work live: {page}"
            );
        }

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

        #[cfg(feature = "htmx")]
        {
            let js_selector = crate::test_html::SelectorList::parse(&format!(
                "script[src=\"{}\"]",
                crate::htmx::AUTUMN_WIDGETS_JS_PATH
            ))
            .expect("selector parses");
            assert!(
                !js_selector.matches(&dom).is_empty(),
                "detail page must load the widget runtime script: {page}"
            );
        }

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

    // U11 (theming/interactivity follow-up): every story page renders a live
    // theme switcher — one radio per `STORY_THEMES` entry, "autumn" checked
    // by default — and each radio's `id` matches a `body:has(#story-theme-…
    // :checked)` override in `STORY_GALLERY_CSS`, so picking a theme needs no
    // JavaScript. This also guards the CSP: the switcher must never grow an
    // inline `<script>`/`on*=` handler, only the pure-CSS `:has()` pattern.
    #[test]
    fn theme_switch_renders_radios_wired_to_css_overrides() {
        let registry = StoryRegistry::new(vec![demo_story("Display", "Card")]);
        let index = render_story_index(&registry, None).into_string();
        let dom = crate::test_html::parse(&index);

        let fieldset = crate::test_html::SelectorList::parse("fieldset.story-theme-switch")
            .expect("selector parses");
        assert!(
            !fieldset.matches(&dom).is_empty(),
            "index must render the theme switcher: {index}"
        );

        for (value, label) in STORY_THEMES {
            let radio_id = format!("story-theme-{value}");
            let radio_selector = crate::test_html::SelectorList::parse(&format!(
                "input[type=\"radio\"][name=\"story-theme\"]#{radio_id}"
            ))
            .expect("selector parses");
            assert!(
                !radio_selector.matches(&dom).is_empty(),
                "missing theme radio for {value:?}: {index}"
            );

            let label_selector =
                crate::test_html::SelectorList::parse(&format!("label[for=\"{radio_id}\"]"))
                    .expect("selector parses");
            let label_matches = label_selector.matches(&dom);
            assert!(
                !label_matches.is_empty(),
                "missing theme label for {value:?}: {index}"
            );
            assert!(
                label_matches[0].text().contains(label),
                "theme label for {value:?} should read {label:?}: {index}"
            );

            // "autumn" is the base `body {}` rule's own default — it needs no
            // `:has()` override, only the other three presets do.
            if value != "autumn" {
                assert!(
                    STORY_GALLERY_CSS.contains(&format!("#story-theme-{value}:checked")),
                    "STORY_GALLERY_CSS must override tokens when #{radio_id} is checked"
                );
            }
        }

        assert!(
            index.contains(r#"id="story-theme-autumn" checked"#),
            "the default theme (autumn) must be checked server-side so the \
             page never renders themeless before CSS applies: {index}"
        );

        assert!(
            !index.contains("<script>")
                && !index.contains(" onclick=")
                && !index.contains(" onchange="),
            "the theme switcher must stay pure-CSS (:has()), no inline script \
             or event handler, to hold under a strict script-src CSP: {index}"
        );
    }

    // U12 (interactivity follow-up): every hx-*-driven widget stayed inert
    // because htmx.min.js was never loaded, only the framework's own
    // data-*-hook runtime was. Both must load, htmx first (both `defer`, so
    // document order fixes execution order — autumn-widgets.js's
    // autocomplete wiring can assume htmx is already present).
    #[cfg(feature = "htmx")]
    #[test]
    fn story_page_loads_htmx_before_the_widgets_runtime() {
        let page = render_story_index(&StoryRegistry::default(), None).into_string();
        let htmx_at = page
            .find(crate::htmx::HTMX_JS_PATH)
            .expect("story page must load htmx.min.js");
        let widgets_at = page
            .find(crate::htmx::AUTUMN_WIDGETS_JS_PATH)
            .expect("story page must load autumn-widgets.js");
        assert!(
            htmx_at < widgets_at,
            "htmx.min.js must appear before autumn-widgets.js so `defer` \
             preserves that execution order: {page}"
        );
    }

    // U13 (interactivity follow-up): the two typeahead stories point at real
    // demo backends now (not the synthetic 404-on-submit URLs most other
    // stories use), and since the URL is a plain string literal in the
    // story's own source — not a reference to the route const, which would
    // show up literally in the public "Source" tab — nothing else catches
    // the two drifting apart. Guard it here instead.
    #[test]
    fn typeahead_stories_point_at_their_registered_demo_routes() {
        let registry = builtin();
        let active_search = registry
            .find("active-search")
            .expect("active-search story exists");
        assert!(
            active_search.source().contains(STORIES_DEMO_SEARCH_PATH),
            "Active search story must point at STORIES_DEMO_SEARCH_PATH \
             ({STORIES_DEMO_SEARCH_PATH}): {}",
            active_search.source()
        );

        let autocomplete = registry
            .find("autocomplete")
            .expect("autocomplete story exists");
        assert!(
            autocomplete.source().contains(STORIES_DEMO_TAG_SEARCH_PATH),
            "Autocomplete story must point at STORIES_DEMO_TAG_SEARCH_PATH \
             ({STORIES_DEMO_TAG_SEARCH_PATH}): {}",
            autocomplete.source()
        );

        let feed = registry
            .find("infinite-feed")
            .expect("infinite-feed story exists");
        assert!(
            feed.source().contains(STORIES_DEMO_FEED_PATH),
            "Infinite feed story must point at STORIES_DEMO_FEED_PATH \
             ({STORIES_DEMO_FEED_PATH}), or its Reveal-mode sentinel \
             404-swaps on page load with no visitor action: {}",
            feed.source()
        );
    }

    // U14 (interactivity follow-up): the demo handlers backing those routes
    // actually filter, case-insensitively, and fall back to the same empty
    // states the stories document.
    #[tokio::test]
    async fn demo_search_filters_case_insensitively_with_empty_state_fallback() {
        let hit = demo_search(crate::extract::Query(DemoQuery {
            q: "AUTUMN".to_owned(),
        }))
        .await
        .0;
        assert!(hit.contains("The Long Autumn"), "{hit}");
        assert!(hit.contains("Autumn in Practice"), "{hit}");
        assert!(!hit.contains("Falling Leaves"), "{hit}");

        let miss = demo_search(crate::extract::Query(DemoQuery {
            q: "nonexistent".to_owned(),
        }))
        .await
        .0;
        assert!(
            miss.contains("No posts matched your search."),
            "no-match query must fall back to active_search_empty_state: {miss}"
        );
    }

    #[tokio::test]
    async fn demo_tag_search_filters_case_insensitively_with_empty_state_fallback() {
        let hit = demo_tag_search(crate::extract::Query(DemoQuery { q: "RU".to_owned() }))
            .await
            .0;
        assert!(hit.contains("rust"), "{hit}");
        assert!(!hit.contains("axum"), "{hit}");

        let miss = demo_tag_search(crate::extract::Query(DemoQuery {
            q: "nonexistent".to_owned(),
        }))
        .await
        .0;
        assert!(
            miss.contains("No matching tags."),
            "no-match query must fall back to autocomplete_empty_state: {miss}"
        );
    }

    // U15 (interactivity follow-up): the feed demo must terminate (no
    // further sentinel) — otherwise it would just keep auto-firing.
    #[tokio::test]
    async fn demo_infinite_feed_returns_a_terminal_page_with_no_further_sentinel() {
        let body = demo_infinite_feed().await.0;
        assert!(body.contains("Loaded live via a real request."), "{body}");
        assert!(
            !body.contains("autumn-feed__sentinel"),
            "a next_cursor of None must emit no further sentinel, or the \
             `revealed` trigger keeps auto-firing forever: {body}"
        );
    }
}
