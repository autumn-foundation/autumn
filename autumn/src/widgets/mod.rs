//! Server-rendered widget helpers: display primitives and interactive search.
//!
//! All widgets return [`maud::Markup`] and are composable — pass the output of
//! one as the body of another. Every widget is CSP-safe (no inline JS) and
//! HTML-escapes caller-supplied text via Maud.
//!
//! # Features
//!
//! The core widgets (layout, navigation, feedback, data display) need the
//! `maud` feature. Four families have their own feature (issue #3408). Each
//! one is on by default and turns on `maud`.
//!
//! | Feature | Widgets |
//! |---------|---------|
//! | `widgets-search` | `active_search`, `autocomplete_input` (single or tags mode) |
//! | `widgets-social` | `reaction_controls`, `comment_thread`, `infinite_feed` |
//! | `widgets-workflow` | `transition_controls`, `bulk_actions_form` |
//! | `widgets-charts` | `sparkline`, `bar_chart`, `line_chart` |
//!
//! With `default-features = false`, name each family the app uses. A family
//! also controls its CSS in `autumn-widgets.css` and its script in
//! `autumn-widgets.js`.
//!
//! # Display widgets
//!
//! | Widget | Use |
//! |--------|-----|
//! | `card` | Titled content panel with optional header action and footer |
//! | `stat_card` | Metric tile: label / value / optional link |
//! | `property_list` | `<dl>` of label/value rows for a record detail view |
//! | `data_table` | Column-driven, sortable `<table>` |
//! | `breadcrumb` | Accessible `<nav>` breadcrumb trail |
//! | `hero` | Landing-page banner: headline, optional subtitle, and CTAs |
//! | `nav_link` | Navigation anchor, auto-marked active + `aria-current` |
//! | `nav_bar` | Top-bar/sidebar `<nav>` landmark: brand, links, dropdowns, responsive toggle |
//! | `tabs` | No-JS `tablist`/`tab`/`tabpanel` switcher with `:target` deep-linking |
//! | `accordion` / `collapsible` | No-JS `<details>` disclosure. `accordion` is exclusive or multi-open |
//! | `infinite_feed` | htmx infinite-scroll / "Load more" feed from a `CursorPage` |
//! | `reaction_controls` | No-JS vote/like toggle buttons + live aggregate for `#[votable]` |
//! | `comment_thread` | No-JS/htmx nested comment list with an inline reply form per node, for `#[commentable]` |
//! | `locale_switcher` | Path-preserving language switcher for locale-prefixed routing (issue #1251) |
//! | `theme_toggle` | No-JS System / Light / Dark switch that stores the `autumn.theme` cookie (issue #3396) |
//! | `button` / `button_group` | Button with a variant, a size, an icon and a loading state. A `role="group"` of buttons (issue #3406) |
//! | `separator` / `kbd` / `kbd_combo` | `<hr>` or a vertical `role="separator"`. Keyboard keys (issue #3406) |
//! | `avatar_group` | Avatars that overlap, with a `+N` item (issue #3406) |
//! | `progress` / `meter` | Native `<progress>` and `<meter>`, with a name and an optional visible label (issue #3406) |
//!
//! # Feedback widgets
//!
//! | Widget | Use |
//! |--------|-----|
//! | `toast` / `toast_with` / `toast_region` | htmx action feedback, appended out-of-band. Pauses on hover or focus. Has a close button |
//! | `tooltip` / `tooltip_with` | Text that shows on hover and focus, linked by `aria-describedby` (issue #3404) |
//! | `spinner` / `skeleton` | A `role="status"` spinner and a placeholder shape. Each can be an htmx `hx-indicator` (issue #3406) |
//! | `empty_state` | Title, text and action for a list with no data (issue #3406) |
//!
//! # Interactive / search widgets
//!
//! | Situation | Use |
//! |-----------|-----|
//! | Keyword search over a rendered list | `active_search` / `active_search_input` |
//! | Select a single related record and store its ID | `autocomplete_input` |
//! | Select many values or tags, as removable chips (issue #3410) | `autocomplete_input` with `.multiple()` |
//! | Pre-fill either mode from a `Changeset` | `autocomplete_input_for` |
//! | Plain `GET` form is sufficient | `axum::extract::Query` |
//! | You need unusual htmx wiring | Hand-write `hx-*` attributes |
//!
//! # Bulk actions
//!
//! | Widget | Use |
//! |--------|-----|
//! | `bulk_select_checkbox` | One row's `name="ids"` select checkbox (issue #1312) |
//! | `bulk_actions_toolbar` | `role="group"` submit button applying the selection |
//! | `bulk_actions_form` | `POST` form wrapping a list + toolbar, with the CSRF field |
//!
//! # Modals & confirmation
//!
//! | Situation | Use |
//! |-----------|-----|
//! | Custom dialog content | `modal` |
//! | Panel from an edge (filters, menus, details) | `drawer` |
//! | Confirm a destructive action (delete, etc.) without `window.confirm()` | `confirm_action` |
//!
//! # Integration with the repository full-text search feature
//!
//! The widgets wire up the client side; your handler owns the Diesel query.
//! If your repository has `#[repository(..., searchable)]`, the generated
//! `repo.search(q)` method works directly with these widgets:
//!
//! ```rust,ignore
//! use autumn_web::prelude::*;
//! use autumn_web::widgets::{ActiveSearchConfig, active_search};
//! use serde::Deserialize;
//!
//! #[derive(Deserialize)]
//! struct SearchQuery { q: String }
//!
//! #[get("/posts")]
//! async fn index() -> Markup {
//!     let config = ActiveSearchConfig::new("/posts/search", "#post-results");
//!     html! {
//!         (active_search("post-search", "Search posts", &config))
//!     }
//! }
//!
//! #[get("/posts/search")]
//! async fn search(
//!     Query(params): Query<SearchQuery>,
//!     repo: PgPostRepository,
//! ) -> AutumnResult<Markup> {
//!     if params.q.trim().is_empty() {
//!         return Ok(active_search_empty_state("Enter a search term"));
//!     }
//!     let results = repo.search(&params.q).await?;
//!     Ok(html! {
//!         @if results.is_empty() {
//!             (active_search_empty_state("No results found"))
//!         } @else {
//!             @for post in &results { li { (post.title) } }
//!         }
//!     })
//! }
//! ```
//!
//! # No-JavaScript fallback
//!
//! `active_search` and `autocomplete_input` include a `<noscript>` block
//! with a plain HTML form or select that works without JavaScript. Your handler
//! already returns an HTML fragment — the only addition for a full no-JS page
//! is wrapping the response in your layout template.

// The core modules compile with `maud`. `nav` also holds `localized_path`,
// which needs no `maud`. Each opt-in family has its own feature.
#[cfg(feature = "widgets-charts")]
mod charts;
#[cfg(feature = "maud")]
mod data;
#[cfg(feature = "maud")]
mod feedback;
#[cfg(feature = "maud")]
mod layout;
mod nav;
#[cfg(feature = "widgets-search")]
mod search;
#[cfg(feature = "widgets-social")]
mod social;
#[cfg(feature = "widgets-workflow")]
mod workflow;

#[cfg(feature = "widgets-charts")]
pub use charts::*;
#[cfg(feature = "maud")]
pub use data::*;
#[cfg(feature = "maud")]
pub use feedback::*;
#[cfg(feature = "maud")]
pub use layout::*;
pub use nav::*;
#[cfg(feature = "widgets-search")]
pub use search::*;
#[cfg(feature = "widgets-social")]
pub use social::*;
#[cfg(feature = "widgets-workflow")]
pub use workflow::*;

// The toast constants need no `maud`, as before the split: an app without
// `maud` can still target the region in an OOB swap or an `HX-Trigger`.

/// The default id for the `toast_region` container, and the OOB target
/// `toast` appends into.
///
/// Drop `toast_region(DEFAULT_TOAST_REGION_ID)` once in your base layout;
/// `toast(msg, variant)` then appends into it out-of-band.
pub const DEFAULT_TOAST_REGION_ID: &str = "toast-region";

/// The time a toast shows before it fades out, when you do not set
/// `ToastConfig::duration`. The stylesheet default (`5s`) must match.
pub const DEFAULT_TOAST_DURATION: std::time::Duration = std::time::Duration::from_secs(5);

/// Strip a leading `#` from a CSS ID selector to get a bare element ID.
///
/// `aria-controls` takes an ID (no `#`), while `hx-target` takes a CSS selector.
#[cfg(feature = "maud")]
fn selector_to_id(selector: &str) -> &str {
    selector.strip_prefix('#').unwrap_or(selector)
}

/// Build a root element's `class` string, merging `base` with any
/// caller-supplied extra class. Shared by every widget with a `.class()`
/// escape hatch (`card`, `hero`, ...).
#[cfg(feature = "maud")]
fn merge_class(base: &str, extra: Option<&str>) -> String {
    match extra {
        Some(e) if !e.is_empty() => format!("{base} {e}"),
        _ => base.to_string(),
    }
}

/// Render `content` inside the `<h1>`–`<h6>` element selected by `level`, with
/// `class` applied and an optional `id` (used by [`modal`] so `aria-labelledby`
/// has a target). Shared by every widget with a configurable heading level
/// (`card`, `hero`, `modal`, ...) so an `HeadingLevel` variant is matched in
/// one place.
#[cfg(feature = "maud")]
fn heading(
    level: HeadingLevel,
    id: Option<&str>,
    class: &str,
    content: &maud::Markup,
) -> maud::Markup {
    maud::html! {
        @match level {
            HeadingLevel::H1 => h1 id=[id] class=(class) { (content) },
            HeadingLevel::H2 => h2 id=[id] class=(class) { (content) },
            HeadingLevel::H3 => h3 id=[id] class=(class) { (content) },
            HeadingLevel::H4 => h4 id=[id] class=(class) { (content) },
            HeadingLevel::H5 => h5 id=[id] class=(class) { (content) },
            HeadingLevel::H6 => h6 id=[id] class=(class) { (content) },
        }
    }
}

// ── Deterministic hashing (badge/avatar colors) ────────────────────────────

/// FNV-1a 64-bit hash of `s`. Deterministic across processes and platforms
/// (unlike `std`'s `DefaultHasher`), so the same input always maps to the same
/// badge variant / avatar color on every render and every machine.
#[cfg(feature = "maud")]
fn fnv1a_hash(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in s.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Panic in a debug build when `label` gives no accessible name.
#[cfg(feature = "maud")]
#[track_caller]
fn assert_named(widget: &str, label: &str) {
    debug_assert!(
        !crate::a11y::is_blank(label),
        "{widget}: the label is empty or blank, so the element has no accessible name"
    );
}
