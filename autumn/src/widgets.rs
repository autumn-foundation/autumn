//! Server-rendered widget helpers: display primitives and interactive search.
//!
//! All widgets return [`maud::Markup`] and are composable — pass the output of
//! one as the body of another. Every widget is CSP-safe (no inline JS) and
//! HTML-escapes caller-supplied text via Maud.
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
//! | `infinite_feed` | htmx infinite-scroll / "Load more" feed from a `CursorPage` |
//! | `reaction_controls` | No-JS vote/like toggle buttons + live aggregate for `#[votable]` |
//! | `locale_switcher` | Path-preserving language switcher for locale-prefixed routing (issue #1251) |
//!
//! # Feedback widgets
//!
//! | Widget | Use |
//! |--------|-----|
//! | `toast` / `toast_region` | Transient, auto-dismissing htmx action feedback, appended out-of-band |
//!
//! # Interactive / search widgets
//!
//! | Situation | Use |
//! |-----------|-----|
//! | Keyword search over a rendered list | `active_search` / `active_search_input` |
//! | Select a single related record and store its ID | `autocomplete_input` |
//! | Plain `GET` form is sufficient | `axum::extract::Query` |
//! | You need unusual htmx wiring | Hand-write `hx-*` attributes |
//!
//! # Modals & confirmation
//!
//! | Situation | Use |
//! |-----------|-----|
//! | Custom dialog content | `modal` |
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

/// HTTP method for an [`ActiveSearchConfig`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SearchMethod {
    /// `GET` request — the default. Safe, idempotent, bookmarkable.
    #[default]
    Get,
    /// `POST` request — opt-in for handlers that need a request body.
    Post,
}

/// Configuration for an [`active_search_input`] widget.
///
/// Build with [`ActiveSearchConfig::new`] and chain builder methods for
/// optional overrides.
///
/// # Example
///
/// ```rust,ignore
/// use autumn_web::widgets::{ActiveSearchConfig, active_search};
///
/// let config = ActiveSearchConfig::new("/users/search", "#user-results")
///     .debounce(500)
///     .min_length(2)
///     .placeholder("Search users…");
///
/// let widget = active_search("user-search", "Search users", &config);
/// ```
#[derive(Debug, Clone)]
pub struct ActiveSearchConfig<'a> {
    /// URL of the server-side search handler.
    pub action: &'a str,
    /// HTTP method (default: [`SearchMethod::Get`]).
    pub method: SearchMethod,
    /// CSS selector for the element that receives rendered results.
    pub target: &'a str,
    /// CSS selector for an element shown while the request is in flight.
    pub indicator: Option<&'a str>,
    /// Debounce delay in milliseconds (default: `300`).
    pub debounce_ms: u32,
    /// Minimum character count before triggering a search (default: `1`).
    pub min_length: u32,
    /// Query parameter name sent to the handler (default: `"q"`).
    pub param_name: &'a str,
    /// Whether to fire the search immediately on page load (default: `false`).
    pub initial_load: bool,
    /// Optional placeholder text for the search input.
    pub placeholder: Option<&'a str>,
}

impl<'a> ActiveSearchConfig<'a> {
    /// Create a new active search configuration with sensible defaults.
    ///
    /// - `action` — URL of the search handler
    /// - `target` — CSS selector for the results container, e.g. `"#search-results"`
    #[must_use]
    pub const fn new(action: &'a str, target: &'a str) -> Self {
        Self {
            action,
            method: SearchMethod::Get,
            target,
            indicator: None,
            debounce_ms: 300,
            min_length: 1,
            param_name: "q",
            initial_load: false,
            placeholder: None,
        }
    }

    /// Use `POST` instead of the default `GET` for the search request.
    #[must_use]
    pub const fn post(mut self) -> Self {
        self.method = SearchMethod::Post;
        self
    }

    /// Set the debounce delay in milliseconds (default: `300`).
    #[must_use]
    pub const fn debounce(mut self, ms: u32) -> Self {
        self.debounce_ms = ms;
        self
    }

    /// Set the minimum query length before a search is triggered (default: `1`).
    #[must_use]
    pub const fn min_length(mut self, n: u32) -> Self {
        self.min_length = n;
        self
    }

    /// Set a CSS selector for the htmx loading indicator element.
    #[must_use]
    pub const fn indicator(mut self, selector: &'a str) -> Self {
        self.indicator = Some(selector);
        self
    }

    /// Trigger a search on initial page load (useful for pre-populated results).
    #[must_use]
    pub const fn initial_load(mut self) -> Self {
        self.initial_load = true;
        self
    }

    /// Set placeholder text for the search input.
    #[must_use]
    pub const fn placeholder(mut self, text: &'a str) -> Self {
        self.placeholder = Some(text);
        self
    }

    /// Set a custom query parameter name (default: `"q"`).
    #[must_use]
    pub const fn param_name(mut self, name: &'a str) -> Self {
        self.param_name = name;
        self
    }
}

/// Configuration for an [`autocomplete_input`] widget.
///
/// Build with [`AutocompleteConfig::new`] and chain builder methods for
/// optional overrides.
#[derive(Debug, Clone)]
pub struct AutocompleteConfig<'a> {
    /// URL of the server-side autocomplete handler.
    pub action: &'a str,
    /// CSS selector for an element shown while the request is in flight.
    pub indicator: Option<&'a str>,
    /// Debounce delay in milliseconds (default: `300`).
    pub debounce_ms: u32,
    /// Minimum character count before triggering autocomplete (default: `1`).
    pub min_length: u32,
    /// Query parameter name sent to the handler and used as the visible input's
    /// `name` (default: `"q"`).
    pub query_param: &'a str,
    /// `name` attribute for the hidden input storing the selected record ID.
    pub value_name: &'a str,
    /// Optional placeholder text for the visible input.
    pub placeholder: Option<&'a str>,
    /// Static `(value, label)` pairs for the `<noscript>` `<select>` fallback.
    pub fallback_options: Option<&'a [(&'a str, &'a str)]>,
    /// When `true`, the hidden field is kept in sync with whatever the user
    /// types, so submitting without picking an option sends the typed text.
    ///
    /// Use this for tag-style fields where the submitted value is the text
    /// itself (e.g. `name="tag"` with `autocomplete_option(tag, tag)`). Leave
    /// `false` (the default) for ID-based lookups where the hidden field should
    /// only carry a value selected from the option list.
    pub free_text: bool,
    /// Pre-fill the widget from a previous submission (e.g. re-rendering a
    /// form after a validation error), so free-text input the user already
    /// typed isn't silently dropped. Seeds both the visible query input's
    /// `value` and the hidden field's `value`. Leave unset (the default) for a
    /// blank widget.
    pub initial_value: Option<&'a str>,
}

impl<'a> AutocompleteConfig<'a> {
    /// Create a new autocomplete configuration with sensible defaults.
    ///
    /// - `action` — URL of the autocomplete handler
    /// - `value_name` — `name` attribute for the hidden selected-ID input
    #[must_use]
    pub const fn new(action: &'a str, value_name: &'a str) -> Self {
        Self {
            action,
            indicator: None,
            debounce_ms: 300,
            min_length: 1,
            query_param: "q",
            value_name,
            placeholder: None,
            fallback_options: None,
            free_text: false,
            initial_value: None,
        }
    }

    /// Set the debounce delay in milliseconds (default: `300`).
    #[must_use]
    pub const fn debounce(mut self, ms: u32) -> Self {
        self.debounce_ms = ms;
        self
    }

    /// Set the minimum query length before autocomplete triggers (default: `1`).
    #[must_use]
    pub const fn min_length(mut self, n: u32) -> Self {
        self.min_length = n;
        self
    }

    /// Set a CSS selector for the htmx loading indicator element.
    #[must_use]
    pub const fn indicator(mut self, selector: &'a str) -> Self {
        self.indicator = Some(selector);
        self
    }

    /// Set a custom query parameter name (default: `"q"`).
    #[must_use]
    pub const fn query_param(mut self, name: &'a str) -> Self {
        self.query_param = name;
        self
    }

    /// Set placeholder text for the visible input.
    #[must_use]
    pub const fn placeholder(mut self, text: &'a str) -> Self {
        self.placeholder = Some(text);
        self
    }

    /// Set static `(value, label)` pairs for the no-JavaScript `<select>` fallback.
    #[must_use]
    pub const fn fallback_options(mut self, options: &'a [(&'a str, &'a str)]) -> Self {
        self.fallback_options = Some(options);
        self
    }

    /// Enable free-text mode: the hidden field is kept in sync with whatever
    /// the user types, so submitting without choosing an option sends the typed
    /// text as the field value.
    ///
    /// Use for tag-style fields (`name="tag"`) where creating new values by
    /// typing is allowed. Leave disabled (the default) for ID-based foreign-key
    /// lookups where only values from the option list are valid.
    #[must_use]
    pub const fn free_text(mut self) -> Self {
        self.free_text = true;
        self
    }

    /// Pre-fill the widget from a previous submission (e.g. re-rendering after
    /// a validation error), so already-typed input isn't silently dropped.
    #[must_use]
    pub const fn initial_value(mut self, value: &'a str) -> Self {
        self.initial_value = Some(value);
        self
    }
}

/// Build the `hx-trigger` value for active search / autocomplete inputs.
///
/// The canonical form is `input changed delay:{n}ms[, load]`.
/// No filter expressions are emitted; `min_length` is enforced server-side so
/// the trigger works under Autumn's default `script-src 'self'` CSP (no `unsafe-eval`).
fn build_trigger(debounce_ms: u32, initial_load: bool) -> String {
    let mut trigger = format!("input changed delay:{debounce_ms}ms");
    if initial_load {
        trigger.push_str(", load");
    }
    trigger
}

/// Strip a leading `#` from a CSS ID selector to get a bare element ID.
///
/// `aria-controls` takes an ID (no `#`), while `hx-target` takes a CSS selector.
fn selector_to_id(selector: &str) -> &str {
    selector.strip_prefix('#').unwrap_or(selector)
}

/// Render a labeled `<input type="search">` with htmx active-search attributes.
///
/// Fires debounced GET (or POST) requests as the user types and on Enter,
/// targeting `config.target`. An accessible `<label>` and `aria-controls`
/// pointing at the results container are included automatically.
///
/// Pair with [`active_search_results`] for the results container, or use
/// [`active_search`] to emit the complete widget (input + results + noscript).
///
/// # htmx attributes emitted
///
/// | Attribute | Value |
/// |-----------|-------|
/// | `hx-get` / `hx-post` | `config.action` |
/// | `hx-trigger` | `input changed delay:{n}ms[, load]` |
/// | `hx-target` | `config.target` |
/// | `hx-indicator` | `config.indicator` (only when set) |
#[cfg(feature = "maud")]
#[must_use]
pub fn active_search_input(id: &str, label: &str, config: &ActiveSearchConfig<'_>) -> maud::Markup {
    let trigger = build_trigger(config.debounce_ms, config.initial_load);
    let aria_controls = selector_to_id(config.target);
    let (hx_get, hx_post) = match config.method {
        SearchMethod::Get => (Some(config.action), None::<&str>),
        SearchMethod::Post => (None, Some(config.action)),
    };

    maud::html! {
        div class="autumn-search" {
            label for=(id) class="autumn-search__label" { (label) }
            input
                type="search"
                id=(id)
                name=(config.param_name)
                autocomplete="off"
                aria-controls=(aria_controls)
                placeholder=[config.placeholder]
                class="autumn-search__input"
                data-ac-min-length=(config.min_length)
                hx-get=[hx_get]
                hx-post=[hx_post]
                hx-trigger=(trigger)
                hx-target=(config.target)
                hx-indicator=[config.indicator];
        }
    }
}

/// Render the results container element targeted by [`active_search_input`].
///
/// Uses `role="status"` and `aria-live="polite"` so screen readers announce
/// result updates without moving keyboard focus.
#[cfg(feature = "maud")]
#[must_use]
pub fn active_search_results(id: &str) -> maud::Markup {
    maud::html! {
        div
            id=(id)
            role="status"
            aria-live="polite"
            aria-atomic="true" {}
    }
}

/// Render a complete active search widget.
///
/// Emits:
/// - A labeled search input with htmx active-search attributes.
/// - A results container whose `id` is derived from `config.target`.
/// - A `<noscript>` fallback form that works without JavaScript.
///
/// **`config.target` must be a `#id` selector** (e.g. `"#bookmark-search-results"`).
/// This function derives the results container `id` by stripping the leading `#`, so
/// class selectors (`.foo`) or other forms produce an invalid HTML `id` attribute.
/// Use [`active_search_input`] + [`active_search_results`] directly if you need a
/// non-id htmx target.
///
/// # Example
///
/// ```rust,ignore
/// use autumn_web::widgets::{ActiveSearchConfig, active_search};
///
/// let config = ActiveSearchConfig::new("/bookmarks/search", "#bookmark-search-results")
///     .placeholder("Search bookmarks…");
///
/// html! {
///     (active_search("bookmark-search", "Search bookmarks", &config))
/// }
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn active_search(id: &str, label: &str, config: &ActiveSearchConfig<'_>) -> maud::Markup {
    debug_assert!(
        config.target.starts_with('#'),
        "active_search: config.target must be a #id selector (e.g. \"#my-results\"), got {:?}. \
         Use active_search_input + active_search_results directly for other selectors.",
        config.target
    );
    // Derive the results container ID from the configured target selector so the
    // rendered container always matches what the input's hx-target points at.
    let results_id = selector_to_id(config.target).to_string();
    let noscript_method = match config.method {
        SearchMethod::Get => "get",
        SearchMethod::Post => "post",
    };

    maud::html! {
        div id=(format!("{id}-wrapper")) {
            (active_search_input(id, label, config))
            (active_search_results(&results_id))
            noscript {
                form action=(config.action) method=(noscript_method) {
                    label for=(format!("{id}-noscript")) { (label) }
                    input
                        type="search"
                        id=(format!("{id}-noscript"))
                        name=(config.param_name)
                        placeholder=[config.placeholder];
                    button type="submit" { "Search" }
                }
            }
        }
    }
}

/// Render an active search empty-state partial.
///
/// Return this from your search handler when no results match the query.
/// The `role="status"` and `aria-live="polite"` attributes ensure screen
/// readers announce the empty state.
#[cfg(feature = "maud")]
#[must_use]
pub fn active_search_empty_state(message: &str) -> maud::Markup {
    maud::html! {
        div
            role="status"
            aria-live="polite"
            class="search-empty" {
            (message)
        }
    }
}

/// Render an autocomplete input widget.
///
/// Emits:
/// - A visible `<input type="search" role="combobox">` for typing.
/// - A hidden `<input>` for storing the selected record's ID.
/// - A `<div role="listbox">` where the server renders option partials.
/// - A `<noscript>` fallback `<select>`.
///
/// Set [`AutocompleteConfig::initial_value`] to pre-fill both inputs — e.g.
/// re-rendering a form with this widget after a validation error, so text the
/// user already typed isn't silently dropped.
///
/// Use [`autocomplete_option`] and [`autocomplete_empty_state`] to render
/// option partials returned by your handler.
///
/// # Example
///
/// ```rust,ignore
/// use autumn_web::widgets::{AutocompleteConfig, autocomplete_input};
///
/// let config = AutocompleteConfig::new("/tags/autocomplete", "tag_id")
///     .placeholder("Search tags…");
///
/// html! {
///     (autocomplete_input("tag-picker", "Tag", &config))
/// }
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn autocomplete_input(id: &str, label: &str, config: &AutocompleteConfig<'_>) -> maud::Markup {
    let query_id = format!("{id}-query");
    let value_id = format!("{id}-value");
    let options_id = format!("{id}-options");
    let trigger = build_trigger(config.debounce_ms, false);
    let target = format!("#{options_id}");

    // Interaction wiring is handled by the external autumn-widgets.js script
    // (served at /static/js/autumn-widgets.js) via data-* attributes:
    //
    //  data-ac-value-id   — id of the hidden input that receives the selected value
    //  data-ac-value-name — form field name assigned to the hidden input by JS
    //  data-ac-free-text  — present when free-text typing is allowed (see free_text())
    //  data-ac-query      — marks the visible search input
    //  data-ac-min-length — minimum characters before htmx fires a request
    //
    // The hidden input has no name attribute in HTML so no-JS form submission
    // only sees the <noscript><select>, avoiding a duplicate-field conflict.

    maud::html! {
        div
            id=(format!("{id}-wrapper"))
            class="autumn-autocomplete"
            data-ac-value-id=(value_id)
            data-ac-value-name=(config.value_name)
            data-ac-free-text[config.free_text] {
            label for=(query_id) class="autumn-autocomplete__label" { (label) }
            input
                type="search"
                id=(query_id)
                name=(config.query_param)
                autocomplete="off"
                role="combobox"
                aria-expanded="false"
                aria-autocomplete="list"
                aria-controls=(options_id)
                value=(config.initial_value.unwrap_or(""))
                placeholder=[config.placeholder]
                class="autumn-autocomplete__input"
                data-ac-query
                data-ac-min-length=(config.min_length)
                hx-get=(config.action)
                hx-trigger=(trigger)
                hx-target=(target)
                hx-indicator=[config.indicator];
            input
                type="hidden"
                id=(value_id)
                value=(config.initial_value.unwrap_or(""));
            div
                id=(options_id)
                role="listbox"
                aria-label=(label)
                aria-live="polite"
                class="autumn-autocomplete__options" {}
            noscript {
                select name=(config.value_name) aria-label=(label) {
                    option value="" { "— select —" }
                    @if let Some(opts) = config.fallback_options {
                        @for (val, lbl) in opts {
                            option value=(val) selected[config.initial_value == Some(*val)] { (lbl) }
                        }
                    }
                }
            }
        }
    }
}

/// Render a single autocomplete option partial returned by the server.
///
/// The `data-value` attribute carries the record ID (or the value to submit).
/// The `autumn-widgets.js` runtime listens for click and keyboard events on the
/// listbox container and uses `data-value` to populate the hidden field.
///
/// # Example response fragment
///
/// ```rust,ignore
/// use autumn_web::widgets::autocomplete_option;
///
/// html! {
///     @for tag in &tags {
///         (autocomplete_option(&tag.id.to_string(), &tag.name))
///     }
/// }
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn autocomplete_option(value: &str, label: &str) -> maud::Markup {
    maud::html! {
        div
            role="option"
            tabindex="0"
            class="autumn-autocomplete__option"
            data-value=(value) {
            (label)
        }
    }
}

/// Render an autocomplete empty-state partial.
///
/// Return this from your autocomplete handler when no records match the query.
#[cfg(feature = "maud")]
#[must_use]
pub fn autocomplete_empty_state(message: &str) -> maud::Markup {
    maud::html! {
        div
            role="status"
            aria-live="polite"
            class="autocomplete-empty" {
            (message)
        }
    }
}

// ── property_list ─────────────────────────────────────────────────────────

/// Render a property list (definition list) for a record's fields.
///
/// Emits a `<dl class="autumn-property-list">` with one `<dt>`/`<dd>` pair per
/// entry. Labels (`&str`) are HTML-escaped; values are pre-escaped [`Markup`](maud::Markup)
/// so callers can pass plain text, a formatted date, or a nested link/badge
/// without escaping foot-guns.
///
/// An empty `rows` slice renders an empty `<dl>`, never panics.
///
/// # CSS hooks
///
/// | Selector | Element |
/// |---|---|
/// | `.autumn-property-list` | The `<dl>` wrapper |
/// | `.autumn-property-list dt` | Label term |
/// | `.autumn-property-list dd` | Value description |
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::property_list;
/// use maud::html;
///
/// struct Post { id: i64, title: String, published: bool }
/// let post = Post { id: 1, title: "Hello".into(), published: true };
///
/// let rows = vec![
///     ("Id", html! { (post.id) }),
///     ("Title", html! { (&post.title) }),
///     ("Published", html! { (post.published.to_string()) }),
/// ];
/// let markup = property_list(&rows);
/// let html = markup.into_string();
/// assert!(html.contains(r#"class="autumn-property-list""#));
/// assert!(html.contains("<dt>Id</dt>"));
/// assert!(html.contains("<dd>1</dd>"));
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn property_list(rows: &[(&str, maud::Markup)]) -> maud::Markup {
    maud::html! {
        dl class="autumn-property-list" {
            @for (label, value) in rows {
                dt { (label) }
                dd { (value) }
            }
        }
    }
}

// ── transition_controls ────────────────────────────────────────────────────

/// Render one CSRF-protected, no-JS `POST` form + button per state-machine
/// transition whose `from` matches `current`.
///
/// This wires the field-level `#[state_machine]` attribute (on `#[model]`
/// structs) into a detail view: the macro emits, per state-machine `String`
/// field `status`, a public `Model::__AUTUMN_SM_STATUS_TRANSITIONS` constant of
/// `(from, to, Option<guard>)` tuples plus a `Model::can_transition_status_to`
/// predicate. Pass those two directly:
///
/// - `transitions` is the generated `__AUTUMN_SM_<FIELD>_TRANSITIONS` constant
///   (`&'static [(&'static str, &'static str, Option<&'static str>)]` coerces to
///   the `&[(&str, &str, Option<&str>)]` parameter, so it passes as-is).
/// - `can` is `|to| record.can_transition_<field>_to(to)`.
///
/// Because those items are per-field *inherent* items (not a trait), the helper
/// takes the already-extracted data and the call site supplies the
/// field-specific bits.
///
/// # Behavior
///
/// - Only edges whose `from == current` are rendered; edges from any other state
///   are omitted entirely.
/// - Each rendered edge is an independent `<form method="post" action=(action)>`
///   carrying the hidden CSRF input (mirroring the scaffold's `csrf_input`), a
///   hidden `<input name=(field) value=(to)>` so the server route reads the
///   target state from the form body, and a single `<button type="submit">`.
/// - A button whose `can(to)` returns `false` (a legal edge whose guard
///   currently fails) is still rendered but carries the `disabled` attribute, so
///   the UI shows the transition as visible-but-blocked. A guard-less edge, or
///   one whose guard passes, renders an enabled button.
/// - No JavaScript: plain HTML forms and buttons only.
/// - When no edge matches `current` (a terminal state), an empty grouping
///   container is rendered (the `role="group"` `<div>` with no forms inside).
///
/// # CSRF
///
/// Pass the handler's `csrf: Option<&CsrfToken>` and
/// `csrf_field: Option<&CsrfFormField>` through unchanged. When `csrf` is
/// `Some`, each form gets a hidden `<input type="hidden">` named after
/// `csrf_field` (defaulting to `"_csrf"`), exactly like scaffolded forms.
///
/// # CSS hooks
///
/// | Selector | Element |
/// |----------|---------|
/// | `.autumn-transition-controls` | Grouping `<div role="group">` |
/// | `.autumn-transition` | Each per-edge `<form>` |
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::transition_controls;
///
/// // `Order::__AUTUMN_SM_STATUS_TRANSITIONS` would supply this at a call site.
/// let transitions: &[(&str, &str, Option<&str>)] = &[
///     ("draft", "published", Some("can_publish")),
///     ("published", "archived", None),
/// ];
/// let markup = transition_controls(
///     "/orders/42/transitions/status",
///     "status",
///     "draft",
///     transitions,
///     |to| to == "published", // |to| order.can_transition_status_to(to)
///     None,
///     None,
/// );
/// let html = markup.into_string();
/// assert!(html.contains(r#"class="autumn-transition-controls""#));
/// assert!(html.contains(r#"action="/orders/42/transitions/status""#));
/// assert!(html.contains(r#"<input type="hidden" name="status" value="published">"#));
/// // The `draft -> published` edge is shown; `published -> archived` is not.
/// assert!(!html.contains(r#"value="archived""#));
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn transition_controls(
    action: &str,
    field: &str,
    current: &str,
    transitions: &[(&str, &str, Option<&str>)],
    can: impl Fn(&str) -> bool,
    csrf: Option<&crate::security::CsrfToken>,
    csrf_field: Option<&crate::security::CsrfFormField>,
) -> maud::Markup {
    let csrf_field_name = csrf_field.map_or("_csrf", |f| f.0.as_str());
    maud::html! {
        div class="autumn-transition-controls" role="group" aria-label=(format!("{field} transitions")) {
            @for (from, to, _guard) in transitions {
                @if *from == current {
                    form method="post" action=(action) class="autumn-transition" {
                        @if let Some(tok) = csrf {
                            input type="hidden" name=(csrf_field_name) value=(tok.token());
                        }
                        input type="hidden" name=(field) value=(to);
                        button type="submit" disabled[!can(to)] { (format!("Mark as {to}")) }
                    }
                }
            }
        }
    }
}

// ── reaction_controls ──────────────────────────────────────────────────────

/// Configuration for a [`reaction_controls`] widget.
///
/// Build with [`ReactionControls::votes`] (signed up/down voting, the
/// `#[votable(… aggregate = sum)]` shape) or [`ReactionControls::likes`] (a
/// single membership toggle, the `aggregate = count` shape), then chain the
/// builder methods for optional overrides.
///
/// The struct owns every string it renders (no lifetimes), so a handler can
/// build it from borrowed request data and hand it straight to the widget.
///
/// # `dom_id` is trusted
///
/// `dom_id` is rendered as the container's `id` **and** interpolated into the
/// default `hx-target` (`#{dom_id}`), where it is read as a CSS selector
/// fragment. Pass a value you construct — `format!("votes-{post_id}")` from a
/// typed id is the intended shape — never a raw request parameter or any other
/// attacker-controlled string. Maud escapes the attribute value, so this is not
/// an injection hole; it is a correctness one: a value containing whitespace,
/// `#`, `.` or a quote yields a selector that matches nothing and an htmx swap
/// that silently does not land.
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::ReactionControls;
///
/// let config = ReactionControls::votes("votes-42", "/posts/42/upvote", "/posts/42/downvote")
///     .aggregate(7)          // the target's persisted `score`
///     .current(Some(1))      // `repo.reaction_of(user_id, post_id).await?`
///     .label("Post score");
/// ```
#[cfg(feature = "maud")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReactionControls {
    /// `id` of the grouping container, and the default `hx-target`.
    dom_id: String,
    /// Form action for the up (vote mode) or like (count mode) control.
    up_action: String,
    /// Form action for the down control. `None` selects like/count mode, in
    /// which only the single `up_action` control is rendered.
    down_action: Option<String>,
    /// The target's persisted aggregate (`score` / `{name}_count`).
    aggregate: i64,
    /// The viewer's own current reaction, as returned by `reaction_of()`.
    current: Option<i16>,
    /// `aria-label` of the `role="group"` container.
    label: String,
    /// Accessible name of the up button.
    up_label: String,
    /// Accessible name of the down button.
    down_label: String,
    /// Accessible name of the like button (count mode).
    like_label: String,
    /// `hx-target` every form swaps; defaults to `#{dom_id}`.
    hx_target: String,
    /// CSRF token value; `None` renders no hidden input.
    csrf_token: Option<String>,
    /// CSRF hidden-input name; defaults to `_csrf`.
    csrf_field: String,
    /// Emit `hx-preserve="true"` on the buttons, for shared/broadcast
    /// fragments that must not clobber a viewer's own pressed state.
    preserve_pressed_state: bool,
}

#[cfg(feature = "maud")]
impl ReactionControls {
    /// Shared constructor: `down_action = None` selects like/count mode.
    fn new(dom_id: String, up_action: String, down_action: Option<String>) -> Self {
        let hx_target = format!("#{dom_id}");
        Self {
            dom_id,
            up_action,
            down_action,
            aggregate: 0,
            current: None,
            label: "Reactions".to_owned(),
            up_label: "Upvote".to_owned(),
            down_label: "Downvote".to_owned(),
            like_label: "Like".to_owned(),
            hx_target,
            csrf_token: None,
            csrf_field: "_csrf".to_owned(),
            preserve_pressed_state: false,
        }
    }

    /// Signed up/down controls — the `#[votable(…, aggregate = sum)]` shape.
    ///
    /// `dom_id` names the container the control replaces on submit;
    /// `up_action` / `down_action` are the `POST` routes for each direction
    /// (typically the generated `__autumn_path_*` helpers).
    #[must_use]
    pub fn votes(
        dom_id: impl Into<String>,
        up_action: impl Into<String>,
        down_action: impl Into<String>,
    ) -> Self {
        Self::new(dom_id.into(), up_action.into(), Some(down_action.into()))
    }

    /// A single toggle control — the `#[votable(…, aggregate = count)]` shape.
    ///
    /// One `POST` route serves both directions: the generated `react()` is a
    /// toggle, so re-submitting removes the reaction.
    #[must_use]
    pub fn likes(dom_id: impl Into<String>, action: impl Into<String>) -> Self {
        Self::new(dom_id.into(), action.into(), None)
    }

    /// Set the target's aggregate — its `score` (sum) or `{name}_count`
    /// (count). Rendered verbatim, so a downvoted target shows a negative.
    #[must_use]
    pub const fn aggregate(mut self, n: i64) -> Self {
        self.aggregate = n;
        self
    }

    /// Set the viewer's own current reaction (`reaction_of()`'s result).
    ///
    /// The rendered domain is `{None, Some(1), Some(-1)}`: `Some(1)` presses
    /// the up/like button, `Some(-1)` the down button, and `None` (the
    /// default — what a feed or a signed-out viewer passes) presses neither.
    /// Any other value presses nothing, and in like/count mode `Some(-1)`
    /// presses nothing either, since that mode renders no down direction.
    /// (`reaction_of()` only ever yields those three, so this matters just for
    /// hand-built configs.)
    #[must_use]
    pub const fn current(mut self, v: Option<i16>) -> Self {
        self.current = v;
        self
    }

    /// Set the container's `aria-label`. Default: `"Reactions"`.
    #[must_use]
    pub fn label(mut self, s: impl Into<String>) -> Self {
        self.label = s.into();
        self
    }

    /// Set the up button's accessible name. Default: `"Upvote"`.
    #[must_use]
    pub fn up_label(mut self, s: impl Into<String>) -> Self {
        self.up_label = s.into();
        self
    }

    /// Set the down button's accessible name. Default: `"Downvote"`.
    #[must_use]
    pub fn down_label(mut self, s: impl Into<String>) -> Self {
        self.down_label = s.into();
        self
    }

    /// Set the like button's accessible name. Default: `"Like"`.
    #[must_use]
    pub fn like_label(mut self, s: impl Into<String>) -> Self {
        self.like_label = s.into();
        self
    }

    /// Set the `hx-target` each form swaps. Default: `#{dom_id}` — the control
    /// replaces itself in place. The value is a CSS selector, so it is subject
    /// to the same "must not be attacker-controlled" rule as `dom_id`.
    #[must_use]
    pub fn hx_target(mut self, s: impl Into<String>) -> Self {
        self.hx_target = s.into();
        self
    }

    /// Set the CSRF token value rendered as a hidden input in every form.
    ///
    /// This is the primitive; [`ReactionControls::csrf`] is the sugar that
    /// takes the handler's extractors. Without a token no hidden input is
    /// rendered at all.
    #[must_use]
    pub fn csrf_token(mut self, token: impl Into<String>) -> Self {
        self.csrf_token = Some(token.into());
        self
    }

    /// Set the CSRF hidden input's `name`. Default: `"_csrf"`.
    #[must_use]
    pub fn csrf_field(mut self, field: impl Into<String>) -> Self {
        self.csrf_field = field.into();
        self
    }

    /// Emit `hx-preserve="true"` on the buttons, so an htmx swap that replaces
    /// this control keeps the *live* button elements — and with them the
    /// viewer's own `aria-pressed` / `.autumn-reaction-active` state.
    ///
    /// For **shared/broadcast fragments only** (an SSE fan-out re-rendering a
    /// card for every subscriber, where `current` is necessarily `None`): the
    /// swap then updates the aggregate and the rest of the card without
    /// un-pressing the viewer's buttons. Each button carries a stable id
    /// derived from `dom_id` (`{dom_id}-up` / `-down` / `-like`), which is
    /// what `hx-preserve` matches on.
    ///
    /// Never set this on a control returned from the vote route itself — the
    /// direct response *wants* to repaint the pressed state it just computed,
    /// and `hx-preserve` would resurrect the stale buttons instead.
    #[must_use]
    pub const fn preserve_pressed_state(mut self, preserve: bool) -> Self {
        self.preserve_pressed_state = preserve;
        self
    }

    /// Thread the handler's `Option<&CsrfToken>` / `Option<&CsrfFormField>`
    /// through unchanged — sugar over [`ReactionControls::csrf_token`] and
    /// [`ReactionControls::csrf_field`].
    ///
    /// Both values are copied out, so the config keeps no borrow. A `None`
    /// token renders no hidden input; a `None` field keeps the `"_csrf"`
    /// default. Mirrors `transition_controls`' CSRF parameters.
    #[must_use]
    pub fn csrf(
        mut self,
        token: Option<&crate::security::CsrfToken>,
        field: Option<&crate::security::CsrfFormField>,
    ) -> Self {
        if let Some(tok) = token {
            self = self.csrf_token(tok.token());
        }
        if let Some(name) = field {
            self = self.csrf_field(name.0.as_str());
        }
        self
    }
}

/// Render a no-JS reaction control: one `POST` form per direction (CSRF-
/// protected when a token is threaded) plus the target's live aggregate,
/// upgraded in place by htmx.
///
/// This is the view half of the `#[votable]` association (issue #1362). The
/// generated `repo.react(reactor_id, target_id, value)` returns a `Reaction`
/// carrying everything the widget needs, so a route re-renders with no extra
/// query:
///
/// ```rust,ignore
/// let reaction = posts.react(user_id, post_id, 1).await?;
/// Ok(reaction_controls(
///     &ReactionControls::votes(
///         format!("votes-{post_id}"),
///         format!("/posts/{post_id}/upvote"),
///         format!("/posts/{post_id}/downvote"),
///     )
///     .aggregate(reaction.aggregate)
///     .current(reaction.value),
/// ))
/// ```
///
/// # Behavior
///
/// - The container is a `<div role="group">` carrying the config's `dom_id`
///   and `aria-label`, so assistive tech announces the buttons as one control.
/// - [`ReactionControls::votes`] renders an up form, the aggregate, and a down
///   form; [`ReactionControls::likes`] renders a single like form and the
///   aggregate.
/// - Each direction is an independent `<form method="post" action=(action)>`
///   carrying a single `<button type="submit">` (plus the hidden CSRF input
///   when a token is threaded), so the control still submits with JavaScript
///   disabled. The same form also carries `hx-post` / `hx-target` /
///   `hx-swap="outerHTML"` and a shared `hx-sync="#{dom_id}:replace"` (a
///   second click aborts the in-flight request, so an older response can
///   never repaint over a newer click), so with htmx loaded the submit is
///   intercepted and
///   the response replaces the control in place.
/// - **The widget emits `<form>` elements.** HTML forbids nesting a form
///   inside another form, so never render this widget inside one — the browser
///   drops the inner form and the buttons submit the outer one instead. Place
///   it as a sibling.
/// - Each button is an ARIA toggle: `aria-pressed="true"` exactly on the
///   direction matching `current` (`Some(1)` → up/like, `Some(-1)` → down),
///   which also gets the `autumn-reaction-active` class. `current(None)` —
///   and, in like/count mode, `current(Some(-1))`, which has no direction to
///   match — presses neither.
/// - The glyph (`▲` / `▼` / `♥`) lives in a `<span aria-hidden="true">`, so
///   the button's accessible name comes from its explicit `aria-label` — a
///   screen reader never announces a nameless icon button.
/// - The aggregate is rendered verbatim (negatives are not clamped) inside an
///   `aria-live="polite"` span, so an htmx swap announces the new total.
/// - No JavaScript, no inline styles: plain HTML forms and buttons only.
///
/// # CSRF — required for the no-JS path
///
/// Call [`ReactionControls::csrf`] with the handler's `Option<&CsrfToken>` and
/// `Option<&CsrfFormField>` (or the [`ReactionControls::csrf_token`] /
/// [`ReactionControls::csrf_field`] string primitives). When a token is set,
/// every form gets a hidden `<input type="hidden">` named after the field
/// (defaulting to `"_csrf"`), exactly like scaffolded forms. With no token,
/// no hidden input is rendered.
///
/// **This is what makes the JavaScript-off fallback actually work.** With CSRF
/// protection enabled and no token threaded, the htmx path still succeeds —
/// the framework's `autumn-htmx-csrf.js` shim adds the token as a request
/// header — but the plain form POST carries no token and is rejected with
/// `403`. A control rendered without `.csrf(...)` is therefore htmx-only in a
/// CSRF-protected app. Thread the token on every page a no-JS visitor can
/// reach; the one place it is legitimately omitted is a fragment that only
/// reaches JS-enabled clients (an SSE/live payload).
///
/// # CSS hooks
///
/// | Selector | Element |
/// |----------|---------|
/// | `.autumn-reaction-controls` | Grouping `<div role="group">` |
/// | `.autumn-reaction` | Each per-direction `<form>` |
/// | `.autumn-reaction-up` | The upvote `<form>` |
/// | `.autumn-reaction-down` | The downvote `<form>` |
/// | `.autumn-reaction-like` | The like `<form>` (count mode) |
/// | `.autumn-reaction-button` | Each submit `<button>` |
/// | `.autumn-reaction-active` | The button matching the viewer's reaction |
/// | `.autumn-reaction-count` | The `aria-live` aggregate `<span>` |
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::{ReactionControls, reaction_controls};
///
/// let markup = reaction_controls(
///     &ReactionControls::votes("votes-42", "/posts/42/upvote", "/posts/42/downvote")
///         .aggregate(7)
///         .current(Some(1))
///         .label("Post score"),
/// );
/// let html = markup.into_string();
/// assert!(html.contains(r#"id="votes-42""#));
/// assert!(html.contains(r#"class="autumn-reaction-controls""#));
/// assert!(html.contains(r##"hx-target="#votes-42""##));
/// assert!(html.contains(r#"aria-label="Upvote""#));
/// assert!(html.contains(r#"aria-pressed="true""#));  // the up button is the current reaction
/// assert!(html.contains(r#"aria-pressed="false""#)); // the down button is not
/// assert!(html.contains(">7<"));
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn reaction_controls(cfg: &ReactionControls) -> maud::Markup {
    // Every class is a plain string literal (never `format!`), so the
    // widget-CSS coverage gate can extract it and prove `widgets.css` backs it.
    let up_pressed = cfg.current == Some(1);
    let down_pressed = cfg.current == Some(-1);
    let like_mode = cfg.down_action.is_none();

    let (primary_form_class, primary_glyph, primary_label) = if like_mode {
        ("autumn-reaction autumn-reaction-like", "♥", &cfg.like_label)
    } else {
        ("autumn-reaction autumn-reaction-up", "▲", &cfg.up_label)
    };
    let primary_button_class = if up_pressed {
        "autumn-reaction-button autumn-reaction-active"
    } else {
        "autumn-reaction-button"
    };
    let down_button_class = if down_pressed {
        "autumn-reaction-button autumn-reaction-active"
    } else {
        "autumn-reaction-button"
    };
    let primary_pressed = if up_pressed { "true" } else { "false" };
    let down_pressed_attr = if down_pressed { "true" } else { "false" };
    // Stable per-direction button ids: what `hx-preserve` matches on when a
    // shared/broadcast fragment must not clobber the viewer's pressed state.
    let primary_button_id = if like_mode {
        format!("{}-like", cfg.dom_id)
    } else {
        format!("{}-up", cfg.dom_id)
    };
    let down_button_id = format!("{}-down", cfg.dom_id);
    let preserve = cfg.preserve_pressed_state.then_some("true");
    // Both forms share one sync scope with the `replace` strategy: a second
    // click (up then down before the first response lands) aborts the
    // in-flight request, so only the LAST click's response repaints the
    // control. Without this the two independent forms race and the older
    // response can land second, leaving the stale direction pressed even
    // though the database holds the newer vote — the commits themselves are
    // already serialized by the target-row lock; this orders the *responses*.
    let hx_sync = format!("#{}:replace", cfg.dom_id);

    maud::html! {
        div id=(cfg.dom_id) class="autumn-reaction-controls" role="group" aria-label=(cfg.label) {
            form method="post" action=(cfg.up_action) class=(primary_form_class)
                hx-post=(cfg.up_action) hx-target=(cfg.hx_target) hx-swap="outerHTML"
                hx-sync=(hx_sync) {
                @if let Some(token) = &cfg.csrf_token {
                    input type="hidden" name=(cfg.csrf_field) value=(token);
                }
                button type="submit" id=(primary_button_id) class=(primary_button_class)
                    aria-pressed=(primary_pressed) aria-label=(primary_label)
                    hx-preserve=[preserve] {
                    span aria-hidden="true" { (primary_glyph) }
                }
            }
            span class="autumn-reaction-count" aria-live="polite" { (cfg.aggregate) }
            @if let Some(down_action) = &cfg.down_action {
                form method="post" action=(down_action) class="autumn-reaction autumn-reaction-down"
                    hx-post=(down_action) hx-target=(cfg.hx_target) hx-swap="outerHTML"
                    hx-sync=(hx_sync) {
                    @if let Some(token) = &cfg.csrf_token {
                        input type="hidden" name=(cfg.csrf_field) value=(token);
                    }
                    button type="submit" id=(down_button_id) class=(down_button_class)
                        aria-pressed=(down_pressed_attr) aria-label=(cfg.down_label)
                        hx-preserve=[preserve] {
                        span aria-hidden="true" { "▼" }
                    }
                }
            }
        }
    }
}

// ── data_table ────────────────────────────────────────────────────────────

/// Sort direction for a [`data_table`] sortable column header.
///
/// Re-exported from [`crate::pagination`] so the widget layer and the
/// `ListQuery`/`list()` query layer share a single canonical type — a
/// `data_table` renders exactly the direction the repository applies.
#[cfg(feature = "maud")]
pub use crate::pagination::SortDir;

/// A single column definition for [`data_table`].
///
/// Each column carries a header label, an optional sort key (opt-in via
/// [`Column::sortable`]), and a cell closure that maps a row reference to
/// `Markup`.
///
/// # Example
///
/// ```rust,ignore
/// use autumn_web::widgets::{Column, data_table, DataTableConfig};
///
/// struct Post { id: i64, title: String }
///
/// let cols: Vec<Column<Post>> = vec![
///     Column::new("ID", |row| html! { (row.id) }),
///     Column::new("Title", |row| html! { (row.title.as_str()) })
///         .sortable("title"),
/// ];
/// ```
#[cfg(feature = "maud")]
pub struct Column<'a, T> {
    /// Column header label shown in `<th>`.
    pub header: &'a str,
    /// If `Some`, this column is sortable and the value is the `sort=` query param.
    pub sort_key: Option<&'a str>,
    /// Cell renderer: maps a row reference to rendered `Markup`.
    pub cell: Box<dyn Fn(&T) -> maud::Markup + Send + 'a>,
}

#[cfg(feature = "maud")]
impl<'a, T> Column<'a, T> {
    /// Create a new non-sortable column with a header label and cell closure.
    #[must_use]
    pub fn new(header: &'a str, cell: impl Fn(&T) -> maud::Markup + Send + 'a) -> Self {
        Self {
            header,
            sort_key: None,
            cell: Box::new(cell),
        }
    }

    /// Make this column sortable, linking the header with `sort={sort_key}` query params.
    ///
    /// The server owns the actual ordering; the widget only renders the link.
    #[must_use]
    pub const fn sortable(mut self, sort_key: &'a str) -> Self {
        self.sort_key = Some(sort_key);
        self
    }
}

/// Configuration for a [`data_table`] widget.
///
/// Build with [`DataTableConfig::new`] and chain builder methods for optional overrides.
///
/// # Example
///
/// ```rust,ignore
/// use autumn_web::widgets::DataTableConfig;
///
/// let config = DataTableConfig::new("No posts found.")
///     .base_path("/posts")
///     .query("q=foo")
///     .active_sort("title")
///     .caption("Posts");
/// ```
#[cfg(feature = "maud")]
#[derive(Debug, Clone)]
pub struct DataTableConfig<'a> {
    /// Optional `<caption>` for the table (table name/description, not the empty state).
    pub caption: Option<&'a str>,
    /// Message shown when `rows` is empty (required).
    pub empty_message: &'a str,
    /// Base path for sort links, e.g. `"/posts"`.
    pub base_path: &'a str,
    /// Raw query string of the current request (preserved on sort links).
    pub query: &'a str,
    /// Query parameter name for the sort column (default `"sort"`).
    pub sort_param: &'a str,
    /// Query parameter name for the sort direction (default `"dir"`).
    pub dir_param: &'a str,
    /// Query parameter name for the page (stripped on sort, default `"page"`).
    pub page_param: &'a str,
    /// The currently active sort column key, if any.
    pub active_sort: Option<&'a str>,
    /// Direction of the active sort (default [`SortDir::Asc`]).
    pub active_dir: SortDir,
    /// Optional CSS class to add to the `<table>` element.
    pub class: Option<&'a str>,
}

#[cfg(feature = "maud")]
impl<'a> DataTableConfig<'a> {
    /// Create a new config with the given empty-state message and sensible defaults.
    #[must_use]
    pub const fn new(empty_message: &'a str) -> Self {
        Self {
            caption: None,
            empty_message,
            base_path: "",
            query: "",
            sort_param: "sort",
            dir_param: "dir",
            page_param: "page",
            active_sort: None,
            active_dir: SortDir::Asc,
            class: None,
        }
    }

    /// Set the table `<caption>` (table name; use for accessibility, not for empty state).
    #[must_use]
    pub const fn caption(mut self, caption: &'a str) -> Self {
        self.caption = Some(caption);
        self
    }

    /// Set the base path for sort links (default `""`).
    #[must_use]
    pub const fn base_path(mut self, base_path: &'a str) -> Self {
        self.base_path = base_path;
        self
    }

    /// Preserve the current request query string on sort links.
    #[must_use]
    pub const fn query(mut self, query: &'a str) -> Self {
        self.query = query;
        self
    }

    /// Set the active sort column key.
    #[must_use]
    pub const fn active_sort(mut self, key: &'a str) -> Self {
        self.active_sort = Some(key);
        self
    }

    /// Set the active sort direction.
    #[must_use]
    pub const fn active_dir(mut self, dir: SortDir) -> Self {
        self.active_dir = dir;
        self
    }

    /// Add a CSS class to the `<table>` element.
    #[must_use]
    pub const fn class(mut self, class: &'a str) -> Self {
        self.class = Some(class);
        self
    }
}

/// Strip named params from a raw query string, returning the preserved remainder.
///
/// Intentional duplication of the same helper in `ui::pagination` — that one is
/// private to its module. Both are small enough that sharing outweighs coupling.
#[cfg(feature = "maud")]
fn dt_filter_query(query: &str, drop_keys: &[&str]) -> String {
    let query = query.strip_prefix('?').unwrap_or(query);
    query
        .split('&')
        .filter(|pair| {
            if pair.is_empty() {
                return false;
            }
            let key = pair.split('=').next().unwrap_or(pair);
            !drop_keys.contains(&key)
        })
        .collect::<Vec<&str>>()
        .join("&")
}

/// Render a column-driven data table from a slice of records.
///
/// Emits a semantic, accessible `<table>` with a `<thead>` row of `<th scope="col">`
/// headers and a `<tbody>` of `<tr>`/`<td>` cells. Empty rows render a single
/// placeholder row spanning all columns. Sortable columns (opt-in via
/// [`Column::sortable`]) carry `aria-sort` on the `<th>` and a link that toggles
/// direction while preserving the current query string (sort resets pagination).
///
/// # Composes with `active_search` and `pagination_nav`
///
/// ```rust
/// # use autumn_web::pagination::{Page, PageRequest};
/// # use autumn_web::ui::pagination::{pagination_nav, PagerOptions};
/// # use autumn_web::widgets::{Column, DataTableConfig, data_table};
/// # struct Post { id: i64, title: String }
/// # let req = PageRequest::new(1, 10);
/// # let rows: Vec<Post> = vec![];
/// # let page: Page<Post> = Page::new(rows, 0, &req);
/// let cols: Vec<Column<Post>> = vec![
///     Column::new("ID", |row: &Post| maud::html! { (row.id) }),
///     Column::new("Title", |row: &Post| maud::html! { (row.title.as_str()) }),
/// ];
/// let html = maud::html! {
///     (data_table(&page.content, &cols, &DataTableConfig::new("No posts yet.").base_path("/posts")))
///     (pagination_nav(&page, &PagerOptions::new("/posts")))
/// };
/// assert!(html.into_string().contains("<table"));
/// ```
///
/// # Example with sortable columns
///
/// ```rust,ignore
/// use autumn_web::prelude::*;
/// use autumn_web::widgets::{Column, DataTableConfig, SortDir, data_table};
/// use serde::Deserialize;
///
/// #[derive(Deserialize)]
/// struct SortQuery { sort: Option<String>, dir: Option<String> }
///
/// #[get("/posts")]
/// async fn index(
///     Query(q): Query<SortQuery>,
///     page_req: PageRequest,
///     repo: PgPostRepository,
/// ) -> AutumnResult<Markup> {
///     let active_dir = if q.dir.as_deref() == Some("desc") { SortDir::Desc } else { SortDir::Asc };
///     let page_data = repo.page(&page_req).await?;
///     let cols: Vec<Column<Post>> = vec![
///         Column::new("Title", |row| html! { (row.title.as_str()) }).sortable("title"),
///         Column::new("", |row| html! { a href=(format!("/posts/{}", row.id)) { "Show" } }),
///     ];
///     let config = DataTableConfig::new("No posts yet.")
///         .base_path("/posts")
///         .active_sort(q.sort.as_deref().unwrap_or(""))
///         .active_dir(active_dir);
///     Ok(layout("Posts", html! {
///         (data_table(&page_data.content, &cols, &config))
///         (pagination_nav(&page_data, &PagerOptions::new("/posts")))
///     }))
/// }
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn data_table<T>(
    rows: &[T],
    columns: &[Column<'_, T>],
    config: &DataTableConfig<'_>,
) -> maud::Markup {
    let col_count = columns.len();
    // Hoist loop-invariant: query/drop-keys are the same for every sortable column.
    let filtered = dt_filter_query(
        config.query,
        &[config.sort_param, config.dir_param, config.page_param],
    );

    maud::html! {
        table class=[config.class] {
            @if let Some(caption) = config.caption {
                caption { (caption) }
            }
            thead {
                tr {
                    @for col in columns {
                        @if let Some(sort_key) = col.sort_key {
                            // Determine sort direction for this column's link.
                            @let is_active = config.active_sort == Some(sort_key);
                            @let link_dir = if is_active {
                                config.active_dir.toggled()
                            } else {
                                SortDir::Asc
                            };
                            @let aria_sort = if is_active {
                                config.active_dir.aria_value()
                            } else {
                                "none"
                            };
                            @let href = if filtered.is_empty() {
                                format!(
                                    "{}?{}={}&{}={}",
                                    config.base_path,
                                    config.sort_param,
                                    sort_key,
                                    config.dir_param,
                                    link_dir.param_value(),
                                )
                            } else {
                                format!(
                                    "{}?{}&{}={}&{}={}",
                                    config.base_path,
                                    filtered,
                                    config.sort_param,
                                    sort_key,
                                    config.dir_param,
                                    link_dir.param_value(),
                                )
                            };
                            th scope="col" aria-sort=(aria_sort) {
                                a href=(href) { (col.header) }
                            }
                        } @else {
                            th scope="col" { (col.header) }
                        }
                    }
                }
            }
            tbody {
                @if rows.is_empty() {
                    tr {
                        td colspan=(col_count) {
                            span role="status" { (config.empty_message) }
                        }
                    }
                } @else {
                    @for row in rows {
                        tr {
                            @for col in columns {
                                td { ((col.cell)(row)) }
                            }
                        }
                    }
                }
            }
        }
    }
}

// ── breadcrumb ────────────────────────────────────────────────────────────

/// A single crumb in a [`breadcrumb`] navigation trail.
///
/// Build with [`Crumb::link`] for a linked crumb or [`Crumb::current`] for the
/// current (non-linked) page. The last item in the slice passed to [`breadcrumb`]
/// is always treated as the current page regardless of whether `href` is set.
#[cfg(feature = "maud")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Crumb<'a> {
    /// Visible label for this crumb. HTML-escaped by Maud.
    pub label: &'a str,
    /// Optional URL for this crumb. `None` on the last crumb (current page).
    pub href: Option<&'a str>,
}

#[cfg(feature = "maud")]
impl<'a> Crumb<'a> {
    /// Create a linked crumb.
    #[must_use]
    pub const fn link(label: &'a str, href: &'a str) -> Self {
        Self {
            label,
            href: Some(href),
        }
    }

    /// Create the current-page crumb (no link).
    #[must_use]
    pub const fn current(label: &'a str) -> Self {
        Self { label, href: None }
    }
}

/// Render an accessible breadcrumb navigation trail.
///
/// Emits a `<nav aria-label="Breadcrumb">` containing an `<ol>`. Every crumb
/// except the last renders as an `<a>` link (using `href` when provided); the
/// last crumb renders as the current page marked with `aria-current="page"`.
/// Separators are wrapped in `<span aria-hidden="true">` so screen readers
/// announce only the crumb labels.
///
/// An empty slice renders empty [`maud::Markup`] (no output, no panic).
/// A single crumb renders without a leading separator.
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::{Crumb, breadcrumb};
///
/// let crumbs = [
///     Crumb::link("Home", "/"),
///     Crumb::link("Posts", "/posts"),
///     Crumb::current("My Post"),
/// ];
/// let html = breadcrumb(&crumbs).into_string();
/// assert!(html.contains(r#"aria-current="page""#));
/// assert!(html.contains(r#"href="/posts""#));
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn breadcrumb(crumbs: &[Crumb<'_>]) -> maud::Markup {
    if crumbs.is_empty() {
        return maud::html! {};
    }
    let last = crumbs.len() - 1;
    maud::html! {
        nav aria-label="Breadcrumb" class="autumn-breadcrumb" {
            ol class="autumn-breadcrumb__list" {
                @for (i, crumb) in crumbs.iter().enumerate() {
                    li class="autumn-breadcrumb__item" {
                        @if i > 0 {
                            span aria-hidden="true" class="autumn-breadcrumb__separator" { "›" }
                        }
                        @if i == last {
                            span aria-current="page" class="autumn-breadcrumb__current" { (crumb.label) }
                        } @else if let Some(href) = crumb.href {
                            a href=(href) class="autumn-breadcrumb__link" { (crumb.label) }
                        } @else {
                            span class="autumn-breadcrumb__text" { (crumb.label) }
                        }
                    }
                }
            }
        }
    }
}

// ── nav_link ────────────────────────────────────────────────────────────────

/// Match mode for [`nav_link_matched`] — how the current request path is
/// compared against a link's `href` to decide the active state.
#[cfg(feature = "maud")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NavLinkMatch {
    /// Active only when the current path equals `href` exactly (default).
    #[default]
    Exact,
    /// Active when the current path equals `href`, or starts with `href`
    /// followed by a `/` segment boundary — so `/admin/posts/3/edit`
    /// activates an `/admin/posts` link, but `/admin/postsX` does not. A
    /// trailing slash on `href` is normalized away first, so `/admin/posts/`
    /// behaves identically to `/admin/posts`. An empty `href` — or `href`
    /// that normalizes to one — never matches, since that would otherwise
    /// activate on every absolute path.
    Prefix,
}

/// Render a navigation anchor, marked active when `href` equals `current_path`.
///
/// Uses [`NavLinkMatch::Exact`]; see [`nav_link_matched`] for prefix matching.
/// Pair with the [`CurrentPath`](crate::extract::CurrentPath) extractor to
/// get `current_path` from the incoming request.
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::nav_link;
///
/// let html = nav_link("/posts", "/posts/new", "New Post").into_string();
/// assert!(!html.contains("active"), "different path stays inactive: {html}");
///
/// let html = nav_link("/posts", "/posts", "Posts").into_string();
/// assert!(html.contains(r#"class="active""#));
/// assert!(html.contains(r#"aria-current="page""#));
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn nav_link(current_path: &str, href: &str, label: &str) -> maud::Markup {
    nav_link_matched(current_path, href, label, NavLinkMatch::Exact)
}

/// Render a navigation anchor with an explicit [`NavLinkMatch`] mode.
///
/// When active, the anchor carries `class="active"` and
/// `aria-current="page"`; when inactive, neither attribute is emitted.
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::{NavLinkMatch, nav_link, nav_link_matched};
///
/// let html = nav_link("/posts", "/posts", "Posts").into_string();
/// assert!(html.contains(r#"class="active""#));
/// assert!(html.contains(r#"aria-current="page""#));
///
/// // `/posts/3/edit` activates the `/posts` link only in Prefix mode.
/// let html = nav_link_matched("/posts/3/edit", "/posts", "Posts", NavLinkMatch::Prefix)
///     .into_string();
/// assert!(html.contains(r#"class="active""#));
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn nav_link_matched(
    current_path: &str,
    href: &str,
    label: &str,
    mode: NavLinkMatch,
) -> maud::Markup {
    let active = nav_link_is_active(current_path, href, mode);
    maud::html! {
        a href=(href) class=[active.then_some("active")] aria-current=[active.then_some("page")] {
            (label)
        }
    }
}

/// Decide whether `href` is the active link for `current_path` under `mode`.
#[cfg(feature = "maud")]
fn nav_link_is_active(current_path: &str, href: &str, mode: NavLinkMatch) -> bool {
    match mode {
        NavLinkMatch::Exact => current_path == href,
        NavLinkMatch::Prefix => {
            if current_path == href {
                return true;
            }
            // Normalize a trailing slash so "/posts/" behaves the same as
            // "/posts", but preserve a bare "/" rather than trimming it to an
            // empty prefix — same convention as tenancy::is_public_path, since
            // an empty prefix would otherwise match every absolute path.
            let href = if href.len() > 1 {
                href.trim_end_matches('/')
            } else {
                href
            };
            !href.is_empty() && crate::router::path_matches_route_prefix(current_path, href)
        }
    }
}

// ── locale switcher (issue #1251) ─────────────────────────────────────────────

/// Rewrite a path — optionally with a query string — to point at a
/// different locale's prefixed URL (issue #1251).
///
/// `path` is the current page's locale-stripped path, exactly as returned by
/// axum's `Uri` extractor inside a `/{locale}/...` nest (nesting strips the
/// matched locale segment before extraction reaches the handler) — e.g.
/// `"/posts"` or `"/posts?sort=asc"`. Any query string is preserved
/// verbatim; only the locale segment changes.
///
/// The root path is a special case: axum's `nest("/{locale}", router)` makes
/// the *bare* `/{locale}` (no trailing slash) match the inner router's own
/// `"/"` route — `/{locale}/` 404s — so `localized_path("/", "es")` returns
/// `"/es"`, not `"/es/"`.
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::localized_path;
///
/// assert_eq!(localized_path("/posts", "es"), "/es/posts");
/// assert_eq!(localized_path("/posts?sort=asc", "es"), "/es/posts?sort=asc");
/// assert_eq!(localized_path("/", "es"), "/es");
/// assert_eq!(localized_path("/?ref=newsletter", "es"), "/es?ref=newsletter");
/// ```
#[must_use]
pub fn localized_path(path: &str, locale: &str) -> String {
    let (before_query, query) = path.find('?').map_or((path, ""), |i| path.split_at(i));
    let joined = if before_query == "/" || before_query.is_empty() {
        format!("/{locale}")
    } else {
        let rest = before_query.strip_prefix('/').unwrap_or(before_query);
        format!("/{locale}/{rest}")
    };
    format!("{joined}{query}")
}

/// Render links to the current page in each supported locale (issue #1251),
/// preserving the current path and query — only the locale segment changes.
///
/// `path` is the current page's locale-stripped path (see [`localized_path`]).
/// `current_locale`'s entry renders as inert text with `aria-current="true"`
/// rather than a self-link.
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::locale_switcher;
///
/// let html =
///     locale_switcher("/posts", "en", &["en".to_owned(), "es".to_owned()]).into_string();
/// assert!(html.contains(r#"href="/es/posts""#));
/// assert!(!html.contains(r#"href="/en/posts""#));
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn locale_switcher(
    path: &str,
    current_locale: &str,
    supported_locales: &[String],
) -> maud::Markup {
    maud::html! {
        nav class="autumn-locale-switcher" aria-label="Language" {
            @for locale in supported_locales {
                @if locale == current_locale {
                    span class="autumn-locale-switcher__current" aria-current="true" { (locale) }
                } @else {
                    a href=(localized_path(path, locale)) { (locale) }
                }
            }
        }
    }
}

// ── nav_bar ──────────────────────────────────────────────────────────────────

/// Layout variant for [`nav_bar`] — selects a modifier class on the root
/// `<nav>` element.
#[cfg(feature = "maud")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NavBarLayout {
    /// Horizontal top navigation bar (default).
    #[default]
    Horizontal,
    /// Vertical sidebar / dashboard nav — adds `.autumn-nav--sidebar`.
    Sidebar,
}

/// A single navigation link, as rendered inside a [`nav_bar`] or [`NavMenu`].
#[cfg(feature = "maud")]
#[derive(Debug, Clone)]
pub struct NavLinkItem {
    href: String,
    label: String,
    match_mode: Option<NavLinkMatch>,
}

#[cfg(feature = "maud")]
impl NavLinkItem {
    /// Build from anything convertible to `String` — an owned `String`
    /// (e.g. a `format!()` result) moves in for free via `.into()`, while a
    /// `&str` literal still works ergonomically.
    fn new(
        href: impl Into<String>,
        label: impl Into<String>,
        match_mode: Option<NavLinkMatch>,
    ) -> Self {
        Self {
            href: href.into(),
            label: label.into(),
            match_mode,
        }
    }
}

/// A dropdown navigation item: a `<button>` trigger plus a `<ul>` of
/// sub-links. Build with [`NavMenu::new`] and chain `.link()` / `.link_matched()`
/// / `.plain_link()` for each sub-item.
#[cfg(feature = "maud")]
#[derive(Debug, Clone)]
pub struct NavMenu {
    label: String,
    links: Vec<NavLinkItem>,
}

#[cfg(feature = "maud")]
impl NavMenu {
    /// Create a new dropdown menu with the given trigger label and no sub-items.
    #[must_use]
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            links: Vec::new(),
        }
    }

    /// Append a sub-link using [`NavLinkMatch::Exact`] matching.
    #[must_use]
    pub fn link(self, href: impl Into<String>, label: impl Into<String>) -> Self {
        self.link_matched(href, label, NavLinkMatch::Exact)
    }

    /// Append a sub-link with an explicit [`NavLinkMatch`] mode.
    #[must_use]
    pub fn link_matched(
        mut self,
        href: impl Into<String>,
        label: impl Into<String>,
        mode: NavLinkMatch,
    ) -> Self {
        self.links.push(NavLinkItem::new(href, label, Some(mode)));
        self
    }

    /// Append a sub-link that never becomes active (e.g. an external link).
    #[must_use]
    pub fn plain_link(mut self, href: impl Into<String>, label: impl Into<String>) -> Self {
        self.links.push(NavLinkItem::new(href, label, None));
        self
    }
}

/// One item in a [`nav_bar`]'s primary or trailing item list.
#[cfg(feature = "maud")]
#[derive(Debug, Clone)]
pub enum NavItem {
    /// A single navigation link.
    Link(NavLinkItem),
    /// A dropdown menu of sub-links.
    Menu(NavMenu),
    /// A non-interactive group label (e.g. `"Models"` in a sidebar nav).
    Section(String),
}

#[cfg(feature = "maud")]
impl NavItem {
    /// A link using [`NavLinkMatch::Exact`] matching against the current path.
    #[must_use]
    pub fn link(href: impl Into<String>, label: impl Into<String>) -> Self {
        Self::link_matched(href, label, NavLinkMatch::Exact)
    }

    /// A link with an explicit [`NavLinkMatch`] mode.
    #[must_use]
    pub fn link_matched(
        href: impl Into<String>,
        label: impl Into<String>,
        mode: NavLinkMatch,
    ) -> Self {
        Self::Link(NavLinkItem::new(href, label, Some(mode)))
    }

    /// A link that never becomes active regardless of the current path —
    /// e.g. a link to an unrelated tool (an admin actuator page) that should
    /// never claim `aria-current`.
    #[must_use]
    pub fn plain_link(href: impl Into<String>, label: impl Into<String>) -> Self {
        Self::Link(NavLinkItem::new(href, label, None))
    }

    /// A dropdown menu item.
    #[must_use]
    pub const fn menu(menu: NavMenu) -> Self {
        Self::Menu(menu)
    }

    /// A non-interactive group label.
    #[must_use]
    pub fn section(label: impl Into<String>) -> Self {
        Self::Section(label.into())
    }
}

/// Configuration for [`nav_bar`]. Build with [`NavBarConfig::new`].
#[cfg(feature = "maud")]
#[derive(Debug, Clone)]
pub struct NavBarConfig {
    id: String,
    aria_label: String,
    brand: Option<(maud::Markup, Option<String>)>,
    items: Vec<NavItem>,
    trailing: Vec<NavItem>,
    layout: NavBarLayout,
    toggle_label: String,
    class: Option<String>,
}

#[cfg(feature = "maud")]
impl Default for NavBarConfig {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "maud")]
impl NavBarConfig {
    /// Create a new nav bar configuration: no brand or items, `aria-label`
    /// `"Main"`, id `"autumn-nav"`, toggle label `"Menu"`, horizontal layout.
    #[must_use]
    pub fn new() -> Self {
        Self {
            id: "autumn-nav".to_string(),
            aria_label: "Main".to_string(),
            brand: None,
            items: Vec::new(),
            trailing: Vec::new(),
            layout: NavBarLayout::Horizontal,
            toggle_label: "Menu".to_string(),
            class: None,
        }
    }

    /// Set the brand/logo slot from plain text linking to `href`. The text is
    /// HTML-escaped by Maud.
    #[must_use]
    pub fn brand(self, label: &str, href: &str) -> Self {
        self.brand_html(maud::html! { (label) }, Some(href))
    }

    /// Set the brand/logo slot from pre-built [`maud::Markup`]. When `href`
    /// is `Some`, the brand renders as an anchor; when `None`, it renders as
    /// a `<span>` (e.g. for a plain wordmark with no link).
    ///
    /// Callers are responsible for escaping any user-supplied content inside `brand`.
    #[must_use]
    pub fn brand_html(mut self, brand: maud::Markup, href: Option<&str>) -> Self {
        self.brand = Some((brand, href.map(str::to_string)));
        self
    }

    /// Append a primary navigation item.
    #[must_use]
    pub fn item(mut self, item: NavItem) -> Self {
        self.items.push(item);
        self
    }

    /// Append multiple primary navigation items at once — convenient when
    /// building the list from a dynamic source (e.g. a loop over registered models).
    #[must_use]
    pub fn items(mut self, items: impl IntoIterator<Item = NavItem>) -> Self {
        self.items.extend(items);
        self
    }

    /// Append an item to the right-aligned trailing slot (account menu, CTA, etc.).
    #[must_use]
    pub fn trailing(mut self, item: NavItem) -> Self {
        self.trailing.push(item);
        self
    }

    /// Set the `aria-label` on the `<nav>` landmark (default `"Main"`). Give
    /// each `nav_bar` on a page a distinct label so screen-reader users can
    /// tell them apart.
    #[must_use]
    pub fn aria_label(mut self, label: &str) -> Self {
        self.aria_label = label.to_string();
        self
    }

    /// Set the layout variant (default [`NavBarLayout::Horizontal`]).
    #[must_use]
    pub const fn layout(mut self, layout: NavBarLayout) -> Self {
        self.layout = layout;
        self
    }

    /// Set the element `id` used to derive `aria-controls` target ids
    /// (default `"autumn-nav"`). Give a second `nav_bar` on the same page a
    /// distinct id so their collapse/menu ids don't collide.
    #[must_use]
    pub fn id(mut self, id: &str) -> Self {
        self.id = id.to_string();
        self
    }

    /// Set the visible/accessible text of the hamburger toggle button
    /// (default `"Menu"`).
    #[must_use]
    pub fn toggle_label(mut self, label: &str) -> Self {
        self.toggle_label = label.to_string();
        self
    }

    /// Add extra CSS class(es) to the root `autumn-nav` element.
    #[must_use]
    pub fn class(mut self, class: impl Into<String>) -> Self {
        self.class = Some(class.into());
        self
    }
}

/// Render an accessible top navigation bar (or sidebar, via [`NavBarLayout::Sidebar`]).
///
/// Emits a labelled `<nav>` landmark containing an optional brand, primary
/// items, a hidden-until-enhanced hamburger toggle, and an optional
/// right-aligned trailing item list.
///
/// Every [`NavItem::Link`] built with [`NavItem::link`] / `link_matched`
/// renders through [`nav_link_matched`], so it carries `class="active"` and
/// `aria-current="page"` when its `href` matches `current_path` — pair with
/// the [`CurrentPath`](crate::extract::CurrentPath) extractor to get
/// `current_path` from the incoming request. [`NavItem::plain_link`] never
/// becomes active, for links to unrelated tools that shouldn't claim
/// "you are here" (e.g. an external or admin-actuator link).
///
/// [`NavItem::menu`] renders a `<button>` trigger plus a `<ul>` of sub-links.
/// Before JavaScript enhances the page, the hamburger toggle is hidden (a
/// horizontal bar with no toggle has nothing to hide) and every dropdown is
/// rendered open (`aria-expanded="true"`, no `hidden` attribute) so **all
/// links stay visible with JavaScript off** — the framework's
/// `/static/js/autumn-widgets.js` runtime (same-origin, CSP-safe under the
/// default `script-src 'self'`) then reveals the hamburger and wires the
/// toggle/dropdown/Escape-to-close behavior. No app-authored JavaScript, and
/// no inline `<script>` or `style=` attributes, are ever required.
///
/// # CSS hooks
///
/// | Selector | Element |
/// |---|---|
/// | `.autumn-nav` | Root `<nav>` landmark |
/// | `.autumn-nav--sidebar` | Modifier added by [`NavBarLayout::Sidebar`] |
/// | `.autumn-nav--enhanced` | Added by the JS runtime once initialized |
/// | `.autumn-nav__brand` | Brand link (`<a>`) or wordmark (`<span>`) |
/// | `.autumn-nav__toggle` | Hamburger `<button>` |
/// | `.autumn-nav__collapse` | Collapsible container wrapping the item lists |
/// | `.autumn-nav__collapse--open` | Added by the JS runtime while expanded |
/// | `.autumn-nav__items` | Primary or trailing `<ul>` |
/// | `.autumn-nav__items--trailing` | Added to the trailing `<ul>` only |
/// | `.autumn-nav__item` | Every `<li>` wrapping a link |
/// | `.autumn-nav__item--menu` | `<li>` wrapping a dropdown menu |
/// | `.autumn-nav__section` | Non-interactive group-label `<li>` |
/// | `.autumn-nav__menu-toggle` | Dropdown trigger `<button>` |
/// | `.autumn-nav__menu` | Dropdown sub-item `<ul>` |
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::{NavBarConfig, NavItem, NavLinkMatch, NavMenu, nav_bar};
///
/// let config = NavBarConfig::new()
///     .brand("Acme", "/")
///     .item(NavItem::link("/", "Home"))
///     .item(NavItem::link_matched("/posts", "Posts", NavLinkMatch::Prefix))
///     .item(NavItem::link("/about", "About"))
///     .item(NavItem::menu(
///         NavMenu::new("Products")
///             .link("/widgets", "Widgets")
///             .link("/gadgets", "Gadgets"),
///     ))
///     .trailing(NavItem::plain_link("/login", "Log in"));
///
/// let html = nav_bar("/posts/3", &config).into_string();
/// assert!(html.contains(r#"aria-current="page""#));
/// assert!(!html.contains("style="));
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn nav_bar(current_path: &str, config: &NavBarConfig) -> maud::Markup {
    let root_class = merge_class(
        match config.layout {
            NavBarLayout::Horizontal => "autumn-nav",
            NavBarLayout::Sidebar => "autumn-nav autumn-nav--sidebar",
        },
        config.class.as_deref(),
    );
    let collapse_id = format!("{}-collapse", config.id);
    let mut menu_counter = 0usize;

    maud::html! {
        nav id=(config.id) class=(root_class) aria-label=(config.aria_label) data-autumn-nav {
            @if let Some((brand, href)) = &config.brand {
                @if let Some(href) = href {
                    a class="autumn-nav__brand" href=(href) { (brand) }
                } @else {
                    span class="autumn-nav__brand" { (brand) }
                }
            }
            button type="button" class="autumn-nav__toggle" aria-expanded="false"
                aria-controls=(collapse_id) hidden data-nav-toggle {
                (config.toggle_label)
            }
            div id=(collapse_id) class="autumn-nav__collapse" {
                @if !config.items.is_empty() {
                    ul class="autumn-nav__items" {
                        (render_nav_items(current_path, &config.items, &config.id, &mut menu_counter))
                    }
                }
                @if !config.trailing.is_empty() {
                    ul class="autumn-nav__items autumn-nav__items--trailing" {
                        (render_nav_items(current_path, &config.trailing, &config.id, &mut menu_counter))
                    }
                }
            }
        }
    }
}

/// Render the `<li>` items for one [`nav_bar`] item list (primary or trailing).
#[cfg(feature = "maud")]
fn render_nav_items(
    current_path: &str,
    items: &[NavItem],
    id: &str,
    menu_counter: &mut usize,
) -> maud::Markup {
    maud::html! {
        @for item in items {
            @match item {
                NavItem::Link(link) => {
                    li class="autumn-nav__item" { (render_nav_link_item(current_path, link)) }
                }
                NavItem::Section(label) => {
                    li class="autumn-nav__section" { (label) }
                }
                NavItem::Menu(menu) => {
                    (render_nav_menu(current_path, menu, id, menu_counter))
                }
            }
        }
    }
}

/// Render a single nav link: through [`nav_link_matched`] when it has a
/// [`NavLinkMatch`] mode, or as a plain never-active anchor otherwise.
#[cfg(feature = "maud")]
fn render_nav_link_item(current_path: &str, link: &NavLinkItem) -> maud::Markup {
    if let Some(mode) = link.match_mode {
        return nav_link_matched(current_path, &link.href, &link.label, mode);
    }
    maud::html! { a href=(link.href) { (link.label) } }
}

/// Render a dropdown menu `<li>`: a trigger `<button>` plus its `<ul>` of
/// sub-links, using and incrementing `menu_counter` to derive a unique id.
#[cfg(feature = "maud")]
fn render_nav_menu(
    current_path: &str,
    menu: &NavMenu,
    id: &str,
    menu_counter: &mut usize,
) -> maud::Markup {
    *menu_counter += 1;
    let menu_id = format!("{id}-menu-{menu_counter}");
    maud::html! {
        li class="autumn-nav__item autumn-nav__item--menu" {
            button type="button" class="autumn-nav__menu-toggle" aria-expanded="true"
                aria-controls=(menu_id) data-nav-menu-toggle {
                (menu.label)
            }
            ul id=(menu_id) class="autumn-nav__menu" {
                @for link in &menu.links {
                    li class="autumn-nav__item" { (render_nav_link_item(current_path, link)) }
                }
            }
        }
    }
}

// ── card ──────────────────────────────────────────────────────────────────

/// Heading level for the title element inside a [`card`].
///
/// Used by [`CardConfig::level`] to pick the `<h1>`–`<h6>` element that wraps
/// the card title. Defaults to `<h2>`.
#[cfg(feature = "maud")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HeadingLevel {
    /// `<h1>`
    H1,
    /// `<h2>` (default)
    #[default]
    H2,
    /// `<h3>`
    H3,
    /// `<h4>`
    H4,
    /// `<h5>`
    H5,
    /// `<h6>`
    H6,
}

/// Configuration for a [`card`] widget.
///
/// Build with [`CardConfig::new`] and chain builder methods for optional slots.
///
/// # Example
///
/// ```rust,ignore
/// use autumn_web::widgets::{CardConfig, HeadingLevel, card};
/// use maud::html;
///
/// let action = html! { a class="btn" href="/posts/new" { "+ New" } };
/// let config = CardConfig::new()
///     .title("Posts")
///     .level(HeadingLevel::H2)
///     .header_action(action);
/// let markup = card(&html! { p { "body" } }, &config);
/// ```
#[cfg(feature = "maud")]
#[derive(Debug, Clone, Default)]
pub struct CardConfig<'a> {
    /// Optional title text rendered in a `<hN class="card-title">` element.
    /// Set via [`CardConfig::title`] (HTML-escaped) or [`CardConfig::title_html`]
    /// (pre-built [`maud::Markup`] for rich content).
    title: Option<maud::Markup>,
    /// Heading level for the title element (default [`HeadingLevel::H2`]).
    level: HeadingLevel,
    /// Optional right-side header slot — e.g. action buttons.
    /// Rendered inside `card-header` alongside the title.
    header_action: Option<maud::Markup>,
    /// Optional footer content rendered in `<div class="card-footer">`.
    footer: Option<maud::Markup>,
    /// Extra CSS class(es) appended to the root `card` element.
    class: Option<&'a str>,
}

#[cfg(feature = "maud")]
impl<'a> CardConfig<'a> {
    /// Create a new card configuration with all slots empty and heading level `H2`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            title: None,
            level: HeadingLevel::H2,
            header_action: None,
            footer: None,
            class: None,
        }
    }

    /// Set the card title from a plain `&str`. The text is HTML-escaped by Maud.
    ///
    /// Use [`CardConfig::title_html`] when you need rich markup inside the heading.
    #[must_use]
    pub fn title(mut self, title: &str) -> Self {
        self.title = Some(maud::html! { (title) });
        self
    }

    /// Set the card title from pre-built [`maud::Markup`] for rich heading content
    /// (e.g. a title with an inline badge or count).
    ///
    /// Callers are responsible for escaping any user-supplied content inside `title`.
    #[must_use]
    pub fn title_html(mut self, title: maud::Markup) -> Self {
        self.title = Some(title);
        self
    }

    /// Set the heading level for the title element (default [`HeadingLevel::H2`]).
    #[must_use]
    pub const fn level(mut self, level: HeadingLevel) -> Self {
        self.level = level;
        self
    }

    /// Set the right-side header action slot (e.g. buttons/links).
    #[must_use]
    pub fn header_action(mut self, action: maud::Markup) -> Self {
        self.header_action = Some(action);
        self
    }

    /// Set the footer content rendered in `<div class="card-footer">`.
    #[must_use]
    pub fn footer(mut self, footer: maud::Markup) -> Self {
        self.footer = Some(footer);
        self
    }

    /// Add extra CSS class(es) to the root `card` element.
    #[must_use]
    pub const fn class(mut self, class: &'a str) -> Self {
        self.class = Some(class);
        self
    }
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

/// Render a composable card container.
///
/// Emits a `<div class="card">` with an optional header (title + action slot),
/// a `<div class="card-body">` wrapping `body`, and an optional
/// `<div class="card-footer">`.
///
/// The header is rendered only when a title or `header_action` is set.
/// The title is wrapped in a heading element (`<h2>` by default, configurable
/// via [`CardConfig::level`]) so screen readers can navigate card titles.
///
/// All class names (`card`, `card-header`, `card-title`, `card-body`,
/// `card-footer`) are stable so existing CSS and the admin plugin can adopt
/// the widget with no restyling.
///
/// # CSS hooks
///
/// | Selector | Element |
/// |---|---|
/// | `.card` | Root wrapper |
/// | `.card-header` | Header row (title + action) |
/// | `.card-title` | Title heading element |
/// | `.card-body` | Body wrapper |
/// | `.card-footer` | Footer wrapper |
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::{
///     card, CardConfig, property_list, data_table, Column, DataTableConfig,
/// };
/// use maud::html;
///
/// struct Post { id: i64, title: String }
/// let posts = vec![Post { id: 1, title: "Hello".into() }];
///
/// let summary = property_list(&[
///     ("Total", html! { (posts.len()) }),
/// ]);
///
/// let cols: Vec<Column<Post>> = vec![
///     Column::new("ID",    |p: &Post| html! { (p.id) }),
///     Column::new("Title", |p: &Post| html! { (p.title.as_str()) }),
/// ];
/// let table = data_table(&posts, &cols, &DataTableConfig::new("No posts."));
///
/// let new_btn = html! { a class="btn" href="/posts/new" { "New post" } };
/// let config = CardConfig::new().title("Posts").header_action(new_btn);
///
/// let out = card(&html! { (summary) (table) }, &config).into_string();
/// assert!(out.contains(r#"class="card-header""#));
/// assert!(out.contains(r#"<h2 class="card-title">Posts</h2>"#));
/// assert!(out.contains(r#"class="card-body""#));
/// assert!(out.contains(r#"class="autumn-property-list""#));
/// assert!(out.contains("<table"));
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn card(body: &maud::Markup, config: &CardConfig<'_>) -> maud::Markup {
    let root_class = merge_class("card", config.class);
    let has_header = config.title.is_some() || config.header_action.is_some();
    maud::html! {
        div class=(root_class) {
            @if has_header {
                div class="card-header" {
                    @if let Some(title) = &config.title {
                        (heading(config.level, None, "card-title", title))
                    }
                    @if let Some(action) = &config.header_action {
                        (action)
                    }
                }
            }
            div class="card-body" { (body) }
            @if let Some(footer) = &config.footer {
                div class="card-footer" { (footer) }
            }
        }
    }
}

/// Render a metric stat-card tile: label, value, and an optional "view all" link.
///
/// Emits a `<div class="stat-card">` matching the pattern used by the admin
/// dashboard for model-count tiles. Both `label` and `value` are HTML-escaped.
///
/// `link` is `(href, link_text)`; omit with `None` to render without the link row.
///
/// # CSS hooks
///
/// | Selector | Element |
/// |---|---|
/// | `.stat-card` | Root tile |
/// | `.stat-label` | Metric label |
/// | `.stat-value` | Metric number/value |
/// | `.stat-link` | Link row (only present when `link` is `Some`) |
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::stat_card;
///
/// let html = stat_card("Users", "1 024", Some(("/users", "View all →"))).into_string();
/// assert!(html.contains(r#"class="stat-card""#));
/// assert!(html.contains("1 024"));
/// assert!(html.contains(r#"href="/users""#));
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn stat_card(label: &str, value: &str, link: Option<(&str, &str)>) -> maud::Markup {
    maud::html! {
        div class="stat-card" {
            div class="stat-label" { (label) }
            div class="stat-value" { (value) }
            @if let Some((href, text)) = link {
                div class="stat-link" {
                    a href=(href) { (text) }
                }
            }
        }
    }
}

// ── hero ──────────────────────────────────────────────────────────────────

/// Primary/secondary style hint for a [`Cta`] link rendered by [`hero`].
#[cfg(feature = "maud")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CtaStyle {
    /// The main call-to-action (default) — rendered with `.autumn-hero__cta--primary`.
    #[default]
    Primary,
    /// A secondary, lower-emphasis call-to-action — rendered with
    /// `.autumn-hero__cta--secondary`.
    Secondary,
}

/// A single call-to-action link rendered by [`hero`].
///
/// Build with [`Cta::primary`] or [`Cta::secondary`]. `label` and `href`
/// accept anything convertible to `String` (a `&str` literal or an owned
/// `String` built from request data, e.g. `format!("/posts/{id}")`) so
/// building a CTA from computed values never fights the borrow checker.
#[cfg(feature = "maud")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cta {
    /// Visible link text. HTML-escaped by Maud.
    pub label: String,
    /// Link target URL.
    pub href: String,
    /// Visual style hint (default [`CtaStyle::Primary`]).
    pub style: CtaStyle,
}

#[cfg(feature = "maud")]
impl Cta {
    /// Create a primary call-to-action link.
    #[must_use]
    pub fn primary(label: impl Into<String>, href: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            href: href.into(),
            style: CtaStyle::Primary,
        }
    }

    /// Create a secondary call-to-action link.
    #[must_use]
    pub fn secondary(label: impl Into<String>, href: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            href: href.into(),
            style: CtaStyle::Secondary,
        }
    }
}

/// Configuration for a [`hero`] widget.
///
/// Build with [`HeroConfig::new`] (title is required) and chain builder
/// methods for the optional subtitle and CTAs.
///
/// # Example
///
/// ```rust,ignore
/// use autumn_web::widgets::{Cta, HeroConfig, hero};
///
/// let config = HeroConfig::new("Welcome to the Blog")
///     .subtitle("Thoughts, tutorials, and stories.")
///     .cta(Cta::primary("New post", "/admin/new"))
///     .cta(Cta::secondary("About", "/about"));
///
/// let markup = hero(&config);
/// ```
#[cfg(feature = "maud")]
#[derive(Debug, Clone)]
pub struct HeroConfig {
    /// Headline text/markup, rendered in a single top-level heading element.
    title: maud::Markup,
    /// Heading level for the title element (default [`HeadingLevel::H1`]).
    level: HeadingLevel,
    /// Optional subtitle/lede rendered below the title.
    subtitle: Option<maud::Markup>,
    /// Zero or more call-to-action links rendered below the subtitle.
    ctas: Vec<Cta>,
    /// Extra CSS class(es) appended to the root `autumn-hero` element.
    class: Option<String>,
}

#[cfg(feature = "maud")]
impl HeroConfig {
    /// Create a new hero configuration with the required `title`, heading
    /// level `H1`, and no subtitle or CTAs.
    #[must_use]
    pub fn new(title: &str) -> Self {
        Self {
            title: maud::html! { (title) },
            level: HeadingLevel::H1,
            subtitle: None,
            ctas: Vec::new(),
            class: None,
        }
    }

    /// Set the headline from pre-built [`maud::Markup`] for rich heading
    /// content (e.g. a title with an inline highlight span).
    ///
    /// Callers are responsible for escaping any user-supplied content inside `title`.
    #[must_use]
    pub fn title_html(mut self, title: maud::Markup) -> Self {
        self.title = title;
        self
    }

    /// Set the heading level for the title element (default [`HeadingLevel::H1`]).
    #[must_use]
    pub const fn level(mut self, level: HeadingLevel) -> Self {
        self.level = level;
        self
    }

    /// Set the subtitle/lede from a plain `&str`. The text is HTML-escaped by Maud.
    ///
    /// Use [`HeroConfig::subtitle_html`] when you need rich markup in the subtitle.
    #[must_use]
    pub fn subtitle(mut self, subtitle: &str) -> Self {
        self.subtitle = Some(maud::html! { (subtitle) });
        self
    }

    /// Set the subtitle/lede from pre-built [`maud::Markup`].
    ///
    /// Callers are responsible for escaping any user-supplied content inside `subtitle`.
    #[must_use]
    pub fn subtitle_html(mut self, subtitle: maud::Markup) -> Self {
        self.subtitle = Some(subtitle);
        self
    }

    /// Append a call-to-action link. May be called multiple times; an empty
    /// CTA list is valid and renders no button container.
    #[must_use]
    pub fn cta(mut self, cta: Cta) -> Self {
        self.ctas.push(cta);
        self
    }

    /// Add extra CSS class(es) to the root `autumn-hero` element.
    #[must_use]
    pub fn class(mut self, class: impl Into<String>) -> Self {
        self.class = Some(class.into());
        self
    }
}

/// Render a landing-page hero banner: headline, optional subtitle, and zero
/// or more call-to-action links.
///
/// Emits a `<section class="autumn-hero">` containing a single top-level
/// heading (`<h1>` by default, configurable via [`HeroConfig::level`]), an
/// optional `<p class="autumn-hero__subtitle">`, and — only when at least one
/// CTA is configured — a `<div class="autumn-hero__actions">` of `<a>` links.
/// A hero with zero CTAs renders no actions container at all.
///
/// # CSS hooks
///
/// | Selector | Element |
/// |---|---|
/// | `.autumn-hero` | Root `<section>` wrapper |
/// | `.autumn-hero__title` | Headline heading element |
/// | `.autumn-hero__subtitle` | Subtitle/lede paragraph (only when set) |
/// | `.autumn-hero__actions` | CTA container (only when non-empty) |
/// | `.autumn-hero__cta` | Every CTA link |
/// | `.autumn-hero__cta--primary` / `.autumn-hero__cta--secondary` | Style hint per [`CtaStyle`] |
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::{Cta, HeroConfig, hero};
///
/// let config = HeroConfig::new("Welcome to the Blog")
///     .subtitle("Thoughts, tutorials, and stories.")
///     .cta(Cta::primary("New post", "/admin/new"));
///
/// let html = hero(&config).into_string();
/// assert!(html.contains(r#"class="autumn-hero""#));
/// assert!(html.contains("<h1"));
/// assert!(html.contains(r#"class="autumn-hero__subtitle""#));
/// assert!(html.contains(r#"class="autumn-hero__actions""#));
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn hero(config: &HeroConfig) -> maud::Markup {
    let root_class = merge_class("autumn-hero", config.class.as_deref());
    maud::html! {
        section class=(root_class) {
            (heading(config.level, None, "autumn-hero__title", &config.title))
            @if let Some(subtitle) = &config.subtitle {
                p class="autumn-hero__subtitle" { (subtitle) }
            }
            @if !config.ctas.is_empty() {
                div class="autumn-hero__actions" {
                    @for cta in &config.ctas {
                        @match cta.style {
                            CtaStyle::Primary => {
                                a href=(cta.href) class="autumn-hero__cta autumn-hero__cta--primary" { (cta.label) }
                            }
                            CtaStyle::Secondary => {
                                a href=(cta.href) class="autumn-hero__cta autumn-hero__cta--secondary" { (cta.label) }
                            }
                        }
                    }
                }
            }
        }
    }
}

// ── modal / confirm_action ───────────────────────────────────────────────

/// Configuration for a [`modal`] dialog.
///
/// Build with [`ModalConfig::new`] and chain builder methods for optional slots.
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::{ModalConfig, modal};
/// use maud::html;
///
/// let body = html! { p { "Are you sure you want to continue?" } };
/// let footer = html! { button { "OK" } };
/// let config = ModalConfig::new().footer(footer);
/// let dialog = modal("confirm-dialog", "Confirm", &body, &config);
/// ```
#[cfg(feature = "maud")]
#[derive(Debug, Clone, Default)]
pub struct ModalConfig {
    /// Optional footer content (e.g. action buttons) rendered in
    /// `<div class="autumn-modal__footer">`.
    footer: Option<maud::Markup>,
    /// Heading level for the title element (default [`HeadingLevel::H2`]).
    level: HeadingLevel,
    /// Extra CSS class(es) appended to the root `autumn-modal` element.
    class: Option<String>,
    /// When `true`, emits `closedby="any"` so clicking the backdrop or
    /// pressing Escape dismisses the dialog. Off by default so a
    /// destructive confirm isn't accidentally dismissed by a stray click.
    light_dismiss: bool,
}

#[cfg(feature = "maud")]
impl ModalConfig {
    /// Create a new modal configuration with no footer, `<h2>` title, and
    /// light-dismiss disabled.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            footer: None,
            level: HeadingLevel::H2,
            class: None,
            light_dismiss: false,
        }
    }

    /// Set the footer content (e.g. action buttons) rendered in
    /// `<div class="autumn-modal__footer">`.
    #[must_use]
    pub fn footer(mut self, footer: maud::Markup) -> Self {
        self.footer = Some(footer);
        self
    }

    /// Set the heading level for the title element (default [`HeadingLevel::H2`]).
    #[must_use]
    pub const fn level(mut self, level: HeadingLevel) -> Self {
        self.level = level;
        self
    }

    /// Add extra CSS class(es) to the root `autumn-modal` element.
    #[must_use]
    pub fn class(mut self, class: impl Into<String>) -> Self {
        self.class = Some(class.into());
        self
    }

    /// When `true`, emits `closedby="any"` on the `<dialog>` so the backdrop
    /// and Escape dismiss it (default `false`). Escape-to-close is universal
    /// `<dialog>` behavior; `closedby="any"` itself has limited browser
    /// support (unsupported in Firefox and Safari as of this writing), so
    /// `autumn-widgets.js` ships a backdrop-click polyfill scoped to
    /// `dialog[closedby="any"]` for browsers that don't honor the attribute
    /// natively yet.
    #[must_use]
    pub const fn light_dismiss(mut self, light_dismiss: bool) -> Self {
        self.light_dismiss = light_dismiss;
        self
    }
}

/// Render a native `<dialog>` modal: an `id`, a labelled title, `aria-modal`,
/// a body slot, and an optional footer/actions slot.
///
/// Opened via `showModal()` — either the native HTML Invoker Commands API
/// (`command="show-modal"` / `commandfor`, see [`modal_trigger`]) or the
/// framework-shipped fallback in `autumn-widgets.js` — the dialog gets ESC-close,
/// a focus trap, focus-into-dialog on open, and focus-return-to-trigger on
/// close for free from the browser: no app-authored JavaScript is required.
///
/// # CSS hooks
///
/// | Selector | Element |
/// |---|---|
/// | `.autumn-modal` | Root `<dialog>` |
/// | `.autumn-modal__content` | Inner content wrapper |
/// | `.autumn-modal__title` | Title heading |
/// | `.autumn-modal__body` | Body wrapper |
/// | `.autumn-modal__footer` | Footer wrapper (only present when configured) |
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::{ModalConfig, modal};
/// use maud::html;
///
/// let body = html! { p { "This action cannot be undone." } };
/// let html = modal("delete-confirm", "Delete this post?", &body, &ModalConfig::new())
///     .into_string();
/// assert!(html.contains("<dialog"));
/// assert!(html.contains(r#"aria-labelledby="delete-confirm-title""#));
/// assert!(html.contains(r#"aria-modal="true""#));
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn modal(id: &str, title: &str, body: &maud::Markup, config: &ModalConfig) -> maud::Markup {
    let root_class = merge_class("autumn-modal", config.class.as_deref());
    let title_id = format!("{id}-title");
    let closedby = config.light_dismiss.then_some("any");
    maud::html! {
        dialog id=(id) class=(root_class) role="dialog" aria-modal="true"
            aria-labelledby=(title_id) closedby=[closedby] {
            div class="autumn-modal__content" {
                (heading(config.level, Some(title_id.as_str()), "autumn-modal__title", &maud::html! { (title) }))
                div class="autumn-modal__body" { (body) }
                @if let Some(footer) = &config.footer {
                    div class="autumn-modal__footer" { (footer) }
                }
            }
        }
    }
}

/// Render a `<button>` that opens the `<dialog>` identified by `dialog_id`.
///
/// Uses the native HTML Invoker Commands API (`command="show-modal"` +
/// `commandfor`) with a `data-modal-open` fallback attribute wired by
/// `autumn-widgets.js` for browsers that don't yet support it — no
/// app-authored JavaScript is required either way.
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::modal_trigger;
///
/// let html = modal_trigger("Delete", "delete-confirm", Some("btn btn-danger")).into_string();
/// assert!(html.contains(r#"commandfor="delete-confirm""#));
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn modal_trigger(label: &str, dialog_id: &str, class: Option<&str>) -> maud::Markup {
    maud::html! {
        button type="button" class=[class]
            command="show-modal" commandfor=(dialog_id) data-modal-open=(dialog_id) {
            (label)
        }
    }
}

/// Render a `<button>` that closes the `<dialog>` identified by `dialog_id`.
///
/// Uses the native HTML Invoker Commands API (`command="close"` +
/// `commandfor`) with a `data-modal-close` fallback attribute wired by
/// `autumn-widgets.js` for browsers that don't yet support it.
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::modal_close_button;
///
/// let html = modal_close_button("Cancel", "delete-confirm", None).into_string();
/// assert!(html.contains(r#"command="close""#));
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn modal_close_button(label: &str, dialog_id: &str, class: Option<&str>) -> maud::Markup {
    maud::html! {
        button type="button" class=[class]
            command="close" commandfor=(dialog_id) data-modal-close=(dialog_id) {
            (label)
        }
    }
}

/// Configuration for a [`confirm_action`] confirmation dialog.
///
/// Build with [`ConfirmActionConfig::new`] and chain builder methods for
/// optional overrides. `title` defaults to `"Are you sure?"`, `cancel_label`
/// to `"Cancel"`, and `danger` (the confirm button's semantic styling) to
/// `true`.
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::ConfirmActionConfig;
/// use maud::html;
///
/// let config = ConfirmActionConfig::new()
///     .title("Delete this post?")
///     .message(html! { p { "This action cannot be undone." } })
///     .trigger_class("btn btn-danger")
///     .confirm_class("btn btn-danger");
/// ```
#[cfg(feature = "maud")]
#[derive(Debug, Clone)]
pub struct ConfirmActionConfig<'a> {
    /// Dialog title (default `"Are you sure?"`).
    title: &'a str,
    /// Optional body content rendered above the confirm/cancel actions.
    message: Option<maud::Markup>,
    /// Confirm button label; defaults to the trigger's label when unset.
    confirm_label: Option<&'a str>,
    /// Cancel button label (default `"Cancel"`).
    cancel_label: &'a str,
    /// `class` attribute for the trigger button that opens the dialog.
    trigger_class: Option<&'a str>,
    /// Extra `class`(es) appended to the confirm button, alongside
    /// `autumn-modal__confirm` and (when [`ConfirmActionConfig::danger`] is
    /// `true`) `autumn-modal__confirm--danger`.
    confirm_class: Option<&'a str>,
    /// Whether the confirm button carries the danger semantic class
    /// (default `true` — most uses of `confirm_action` are destructive).
    danger: bool,
    /// CSRF form field name; defaults to `"_csrf"`. Pass the configured
    /// [`CsrfFormField`](crate::security::CsrfFormField) value when
    /// `security.csrf.form_field` is customized.
    csrf_field: Option<&'a str>,
    /// Extra attributes rendered on the confirm `<button>`, e.g. `hx-*`, `data-*`.
    attrs: &'a [(&'a str, &'a str)],
    /// Heading level for the dialog title, forwarded to [`ModalConfig::level`]
    /// (default [`HeadingLevel::H2`]).
    level: HeadingLevel,
    /// Extra CSS class(es) appended to the dialog's root `autumn-modal`
    /// element, forwarded to [`ModalConfig::class`].
    modal_class: Option<&'a str>,
    /// When `true`, the dialog's backdrop and Escape dismiss it, forwarded
    /// to [`ModalConfig::light_dismiss`] (default `false`).
    light_dismiss: bool,
}

#[cfg(feature = "maud")]
impl<'a> ConfirmActionConfig<'a> {
    /// Create a confirm-action configuration with default title ("Are you
    /// sure?"), cancel label ("Cancel"), and `danger` styling enabled.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            title: "Are you sure?",
            message: None,
            confirm_label: None,
            cancel_label: "Cancel",
            trigger_class: None,
            confirm_class: None,
            danger: true,
            csrf_field: None,
            attrs: &[],
            level: HeadingLevel::H2,
            modal_class: None,
            light_dismiss: false,
        }
    }

    /// Set the dialog title (default `"Are you sure?"`).
    #[must_use]
    pub const fn title(mut self, title: &'a str) -> Self {
        self.title = title;
        self
    }

    /// Set optional body content rendered above the confirm/cancel actions.
    #[must_use]
    pub fn message(mut self, message: maud::Markup) -> Self {
        self.message = Some(message);
        self
    }

    /// Override the confirm button's label (defaults to the trigger's label).
    #[must_use]
    pub const fn confirm_label(mut self, label: &'a str) -> Self {
        self.confirm_label = Some(label);
        self
    }

    /// Set the cancel button's label (default `"Cancel"`).
    #[must_use]
    pub const fn cancel_label(mut self, label: &'a str) -> Self {
        self.cancel_label = label;
        self
    }

    /// Set the `class` attribute for the trigger button that opens the dialog.
    #[must_use]
    pub const fn trigger_class(mut self, class: &'a str) -> Self {
        self.trigger_class = Some(class);
        self
    }

    /// Add extra `class`(es) to the confirm button, alongside the built-in
    /// `autumn-modal__confirm`(`--danger`) classes.
    #[must_use]
    pub const fn confirm_class(mut self, class: &'a str) -> Self {
        self.confirm_class = Some(class);
        self
    }

    /// Set whether the confirm button carries the danger semantic class
    /// (default `true`).
    #[must_use]
    pub const fn danger(mut self, danger: bool) -> Self {
        self.danger = danger;
        self
    }

    /// Override the CSRF hidden-input field name (default `"_csrf"`).
    #[must_use]
    pub const fn csrf_field(mut self, csrf_field: &'a str) -> Self {
        self.csrf_field = Some(csrf_field);
        self
    }

    /// Set extra attributes rendered on the confirm `<button>` (e.g.
    /// `hx-*`, `data-*`). Values are HTML-escaped; names must be valid
    /// attribute names (see [`crate::links::button_to_with`] panics).
    #[must_use]
    pub const fn attrs(mut self, attrs: &'a [(&'a str, &'a str)]) -> Self {
        self.attrs = attrs;
        self
    }

    /// Set the heading level for the dialog title (default [`HeadingLevel::H2`]).
    #[must_use]
    pub const fn level(mut self, level: HeadingLevel) -> Self {
        self.level = level;
        self
    }

    /// Add extra CSS class(es) to the dialog's root `autumn-modal` element.
    #[must_use]
    pub const fn modal_class(mut self, class: &'a str) -> Self {
        self.modal_class = Some(class);
        self
    }

    /// When `true`, the dialog's backdrop and Escape dismiss it (default
    /// `false` — a destructive confirm isn't accidentally dismissed by a
    /// stray click). See [`ModalConfig::light_dismiss`] for browser support
    /// notes on the underlying `closedby="any"` attribute.
    #[must_use]
    pub const fn light_dismiss(mut self, light_dismiss: bool) -> Self {
        self.light_dismiss = light_dismiss;
        self
    }
}

#[cfg(feature = "maud")]
impl Default for ConfirmActionConfig<'_> {
    fn default() -> Self {
        Self::new()
    }
}

/// Render a trigger button plus a confirmation `<dialog>` for a
/// state-changing action.
///
/// Opening the dialog requires no app-authored JavaScript
/// ([`modal_trigger`]/[`modal_close_button`]), and the confirm
/// button is a [`crate::links::button_to_with`] submit — the correct HTTP
/// method (via the `_method` override) and CSRF token are always present. A
/// `<noscript>` fallback renders the same submit button directly (no
/// confirmation, matching plain HTML forms), so the action stays reachable
/// with JavaScript disabled — before JS runs, the trigger's `command`
/// attribute alone can't open the dialog.
///
/// This is the framework's answer to `window.confirm()` / htmx's
/// `hx-confirm`: the whole confirmation UI is server-rendered, so a
/// [`crate::test::TestClient`] test can assert the dialog, its title, and
/// the confirm button's action/method/CSRF token — impossible with the
/// native browser dialog.
///
/// # CSS hooks
///
/// | Selector | Element |
/// |---|---|
/// | `.autumn-confirm` | Root wrapper (trigger + dialog) |
/// | `.autumn-modal__confirm-form` | The confirm `<form>` |
/// | `.autumn-modal__confirm` | The confirm `<button>` |
/// | `.autumn-modal__confirm--danger` | Added to the confirm button when [`ConfirmActionConfig::danger`] is `true` (the default) |
/// | `.autumn-modal__cancel` | The cancel `<button>` |
///
/// # Example
///
/// Confirm-delete on a scaffolded resource — a `Post` detail page's delete
/// button, with zero lines of app-authored JavaScript:
///
/// ```rust,ignore
/// use autumn_web::prelude::*;
/// use autumn_web::widgets::{ConfirmActionConfig, confirm_action};
/// use http::Method;
///
/// #[get("/posts/{id}")]
/// async fn show(Path(id): Path<i64>, csrf: CsrfToken, repo: PgPostRepository) -> AutumnResult<Markup> {
///     let post = repo.find(id).await?;
///     let action = format!("/posts/{id}");
///     let config = ConfirmActionConfig::new()
///         .title("Delete this post?")
///         .message(html! { p { "This action cannot be undone." } })
///         .trigger_class("btn btn-danger")
///         .confirm_class("btn btn-danger");
///     Ok(html! {
///         h1 { (post.title) }
///         (confirm_action("delete-post", "Delete", &action, Method::DELETE, csrf.token(), &config))
///     })
/// }
/// ```
///
/// ```rust
/// use autumn_web::widgets::{ConfirmActionConfig, confirm_action};
/// use http::Method;
///
/// let html = confirm_action(
///     "delete-post",
///     "Delete",
///     "/posts/42",
///     Method::DELETE,
///     "tok123",
///     &ConfirmActionConfig::new()
///         .title("Delete this post?")
///         .trigger_class("btn btn-danger")
///         .confirm_class("btn btn-danger"),
/// )
/// .into_string();
/// assert!(html.contains(r#"<dialog id="delete-post""#));
/// assert!(html.contains("Delete this post?"));
/// assert!(html.contains(r#"<form action="/posts/42" method="post""#));
/// assert!(html.contains(r#"name="_method" value="DELETE""#));
/// assert!(html.contains(r#"name="_csrf" value="tok123""#));
/// assert!(html.contains("autumn-modal__confirm--danger"));
/// ```
#[cfg(feature = "maud")]
#[must_use]
#[allow(clippy::needless_pass_by_value)]
pub fn confirm_action(
    id: &str,
    trigger_label: &str,
    action: &str,
    method: http::Method,
    csrf_token: &str,
    config: &ConfirmActionConfig<'_>,
) -> maud::Markup {
    let confirm_label = config.confirm_label.unwrap_or(trigger_label);

    let danger_class = config.danger.then_some("autumn-modal__confirm--danger");
    let confirm_class = merge_class(
        &merge_class("autumn-modal__confirm", danger_class),
        config.confirm_class,
    );

    let mut button_options = crate::links::ButtonToOptions::new()
        .class(confirm_class.as_str())
        .form_class("autumn-modal__confirm-form")
        .attrs(config.attrs);
    if let Some(field) = config.csrf_field {
        button_options = button_options.csrf_field(field);
    }

    let confirm_button = crate::links::button_to_with(
        confirm_label,
        action,
        method.clone(),
        csrf_token,
        &button_options,
    );

    let footer = maud::html! {
        (modal_close_button(config.cancel_label, id, Some("autumn-modal__cancel")))
        (confirm_button)
    };

    let body = config.message.clone().unwrap_or_else(|| maud::html! {});
    let mut modal_config = ModalConfig::new()
        .footer(footer)
        .level(config.level)
        .light_dismiss(config.light_dismiss);
    if let Some(class) = config.modal_class {
        modal_config = modal_config.class(class);
    }

    // No-JS / no-Invoker-Commands fallback: the trigger above can't open the
    // dialog without either the native `command`/`commandfor` API or the
    // `autumn-widgets.js` fallback running, so this plain, always-working
    // (unconfirmed) submit button keeps the action reachable — the same
    // fallback pattern `active_search`/`autocomplete_input` use.
    let mut noscript_options = crate::links::ButtonToOptions::new().attrs(config.attrs);
    if let Some(class) = config.trigger_class {
        noscript_options = noscript_options.class(class);
    }
    if let Some(field) = config.csrf_field {
        noscript_options = noscript_options.csrf_field(field);
    }
    let noscript_button =
        crate::links::button_to_with(trigger_label, action, method, csrf_token, &noscript_options);

    maud::html! {
        div class="autumn-confirm" {
            (modal_trigger(trigger_label, id, config.trigger_class))
            noscript { (noscript_button) }
            (modal(id, config.title, &body, &modal_config))
        }
    }
}

// ── tabs ─────────────────────────────────────────────────────────────────

/// Render a no-JavaScript tabs widget: a `tablist` strip plus its panels,
/// from an ordered list of `(id, label, panel_body)` tuples plus the id of
/// the tab that should be active by default.
///
/// `active_id` selects the default-active tab/panel: `Some(panel_id)` that
/// matches one of the panels makes that panel default-active; `None` (or an
/// id that matches no panel) falls back to the first panel.
///
/// Switching is pure CSS: each tab is an `<a href="#panel-id">` and each
/// panel is targeted by `:target` in `input.css`, so URL fragments
/// (`/settings#security`) select a tab with zero JavaScript. The
/// default-active panel additionally carries `autumn-tabs__panel--active` so
/// a panel is always visible even when no fragment is present, and the
/// active-tab highlight tracks the actually-`:target`ed panel by position for
/// up to the first 6 tabs (see `input.css` — ids are arbitrary caller
/// strings, so this can only be done positionally in a shared stylesheet).
///
/// # CSS hooks
///
/// | Selector | Element |
/// |---|---|
/// | `.autumn-tabs` | Root wrapper |
/// | `.autumn-tabs__list` | Tab strip (`role="tablist"`) |
/// | `.autumn-tabs__tab` | Individual tab link |
/// | `.autumn-tabs__tab--active` | The initially-selected tab |
/// | `.autumn-tabs__panel` | Individual panel |
/// | `.autumn-tabs__panel--active` | The initially-visible panel |
///
/// # Accessibility
///
/// The tab strip carries `role="tablist"`; each tab carries `role="tab"`,
/// `aria-controls` pointing at its panel's `id`, and `aria-selected`; each
/// panel carries `role="tabpanel"`, `aria-labelledby` pointing at its tab's
/// `id`, and `tabindex="0"` so it can receive keyboard focus directly.
///
/// `aria-selected` reflects the *server's default selection* (the
/// `active_id` tab, or the first tab) only. The server never sees the URL
/// fragment (fragments aren't
/// sent in HTTP requests), so it cannot know which tab a client-side
/// `:target` navigation actually landed on — correcting `aria-selected` on
/// navigation would require JavaScript, which this widget deliberately has
/// none of. This is a known, accepted limitation of fragment-based no-JS
/// tabs: a screen reader always hears the first tab announced as
/// "selected", even when a different panel is what's visually shown.
///
/// # Panel `id`s
///
/// A panel's `id` is the raw id from its tuple (not namespaced by `id`) so
/// it can be targeted directly by a URL fragment. Each tab's own element
/// `id` is `"{id}-tab-{panel_id}"`. Duplicate panel ids within one call are
/// rendered as given — the caller is responsible for uniqueness if
/// fragment targeting of a specific panel matters.
///
/// **Rendering more than one `tabs()` widget on the same page:** panel ids
/// must be unique across the *entire page*, not just within one call —
/// two different `tabs()` calls that both use a panel id like `"overview"`
/// will emit two elements with `id="overview"`, which is invalid duplicate-id
/// HTML and makes `:target`/`aria-controls`/`aria-labelledby` lookups
/// ambiguous. Prefix panel ids per widget instance (e.g. `"post-overview"`,
/// `"related-overview"`) if more than one `tabs()` appears on a page.
///
/// # Nesting a `tabs()` widget inside another `tabs()` panel
///
/// This is supported for panel *visibility*: `input.css` reveals the whole
/// ancestor chain down to a deep-linked inner panel, so content nested
/// inside any outer panel (not just the outer widget's default one) is
/// reachable via its own fragment. The outer widget's *active-tab visual
/// highlight*, however, can only be synced when the shown outer panel is
/// itself the direct `:target` — CSS forbids nesting `:has()` inside
/// another `:has()`, so a panel that's shown only because it *contains* a
/// nested target (rather than being the target itself) can't be
/// attributed to a specific outer tab position. In that case no outer tab
/// is highlighted as active (a graceful degrade — no tab looks active,
/// rather than the wrong one looking active).
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::tabs;
/// use maud::html;
///
/// let panels = [
///     ("profile", "Profile", html! { p { "Profile settings" } }),
///     ("security", "Security", html! { p { "Security settings" } }),
///     ("billing", "Billing", html! { p { "Billing settings" } }),
/// ];
/// // Open on the "security" tab by default; pass `None` to default to the first.
/// let widget = tabs("settings-tabs", Some("security"), &panels).into_string();
/// assert!(widget.contains(r#"role="tablist""#));
/// assert!(widget.contains(r#"aria-controls="security""#));
/// assert!(widget.contains(r##"href="#billing""##));
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn tabs(
    id: &str,
    active_id: Option<&str>,
    panels: &[(&str, &str, maud::Markup)],
) -> maud::Markup {
    let tab_ids: Vec<String> = panels
        .iter()
        .map(|(panel_id, _, _)| format!("{id}-tab-{panel_id}"))
        .collect();
    // Which panel is default-active. `Some(id)` that matches a panel selects
    // it; `None` or an unknown id falls back to the first panel (the historic
    // behavior). The chosen index only sets the server's *default* selection —
    // `:target` deep-linking still overrides it in pure CSS.
    let active_index = active_id
        .and_then(|target| {
            panels
                .iter()
                .position(|(panel_id, _, _)| *panel_id == target)
        })
        .unwrap_or(0);
    maud::html! {
        div id=(id) class="autumn-tabs" {
            div class="autumn-tabs__list" role="tablist" {
                @for (i, (panel_id, label, _)) in panels.iter().enumerate() {
                    @let tab_class = merge_class(
                        "autumn-tabs__tab",
                        (i == active_index).then_some("autumn-tabs__tab--active"),
                    );
                    a id=(tab_ids[i]) class=(tab_class) role="tab" href=(format!("#{panel_id}"))
                        aria-controls=(panel_id) aria-selected=(if i == active_index { "true" } else { "false" }) {
                        (label)
                    }
                }
            }
            @for (i, (panel_id, _, body)) in panels.iter().enumerate() {
                @let panel_class = merge_class(
                    "autumn-tabs__panel",
                    (i == active_index).then_some("autumn-tabs__panel--active"),
                );
                section id=(panel_id) class=(panel_class) role="tabpanel"
                    aria-labelledby=(tab_ids[i]) tabindex="0" {
                    (body)
                }
            }
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

// ── badge / status_tag (#1259) ──────────────────────────────────────────────

/// Semantic color for a [`badge`] / [`status_tag`].
///
/// Each variant emits a stable, documented class (`badge--<variant>`) so the
/// framework stylesheet themes it; there are no inline styles. Color is never
/// the *only* signal — the badge's text label always communicates the state.
#[cfg(feature = "maud")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BadgeVariant {
    /// Grey, no strong connotation — the default (e.g. "archived", "draft").
    #[default]
    Neutral,
    /// Blue, informational (e.g. "new", "open").
    Info,
    /// Green, positive (e.g. "published", "active", "paid").
    Success,
    /// Amber, needs attention (e.g. "pending review", "trial").
    Warning,
    /// Red, negative (e.g. "failed", "rejected", "overdue").
    Danger,
}

#[cfg(feature = "maud")]
impl BadgeVariant {
    /// The variant name as a lowercase string (matches the CSS class suffix).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Neutral => "neutral",
            Self::Info => "info",
            Self::Success => "success",
            Self::Warning => "warning",
            Self::Danger => "danger",
        }
    }

    /// Map an arbitrary status string to a deterministic variant.
    ///
    /// Common status vocabularies (`published`, `active`, `failed`, …) map to
    /// their natural color; any other string is hashed to one of the colored
    /// variants so an enum/status column gets a **stable** color per value
    /// without the author enumerating every case. The mapping is
    /// case-insensitive and whitespace-trimmed, and is guaranteed
    /// deterministic: the same input always yields the same variant.
    #[must_use]
    pub fn for_label(label: &str) -> Self {
        match label.trim().to_ascii_lowercase().as_str() {
            "success" | "active" | "published" | "done" | "complete" | "completed" | "approved"
            | "enabled" | "online" | "paid" | "live" | "ok" | "passed" | "ready" => Self::Success,
            "info" | "new" | "open" | "in_progress" | "in progress" | "processing" | "queued"
            | "running" | "scheduled" => Self::Info,
            "warning" | "warn" | "draft" | "pending" | "pending_review" | "review" | "paused"
            | "trial" | "expiring" | "on_hold" => Self::Warning,
            "danger" | "error" | "failed" | "failure" | "rejected" | "cancelled" | "canceled"
            | "disabled" | "offline" | "overdue" | "banned" | "deleted" | "blocked" => Self::Danger,
            "" | "neutral" | "inactive" | "unknown" | "archived" | "closed" | "none" => {
                Self::Neutral
            }
            other => {
                // Deterministic fallback over the colored (non-neutral) set.
                const SET: [BadgeVariant; 4] = [
                    BadgeVariant::Info,
                    BadgeVariant::Success,
                    BadgeVariant::Warning,
                    BadgeVariant::Danger,
                ];
                SET[(fnv1a_hash(other) % 4) as usize]
            }
        }
    }
}

/// Rendering options for [`badge_with`].
#[cfg(feature = "maud")]
#[derive(Debug, Clone, Default)]
pub struct BadgeConfig<'a> {
    title: Option<&'a str>,
    aria_label: Option<&'a str>,
    class: Option<&'a str>,
}

#[cfg(feature = "maud")]
impl<'a> BadgeConfig<'a> {
    /// A default config: no `title`, no `aria-label`, no extra class.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set a `title` (tooltip) — useful when the visible label is abbreviated.
    #[must_use]
    pub const fn title(mut self, title: &'a str) -> Self {
        self.title = Some(title);
        self
    }

    /// Set an `aria-label` — the accessible name announced for an abbreviated
    /// or icon-only label.
    #[must_use]
    pub const fn aria_label(mut self, label: &'a str) -> Self {
        self.aria_label = Some(label);
        self
    }

    /// Append an extra class to the root `<span>`.
    #[must_use]
    pub const fn class(mut self, class: &'a str) -> Self {
        self.class = Some(class);
        self
    }
}

/// Render a small semantic status pill.
///
/// Emits `<span class="badge badge--<variant>">label</span>`. The label is
/// text content (screen-reader legible) and HTML-escaped by Maud; color is
/// never the sole signal. Style the `.badge` / `.badge--*` classes via the
/// framework stylesheet — no inline styles are emitted.
///
/// For a tooltip / accessible name on an abbreviated label, use
/// [`badge_with`] with [`BadgeConfig::title`] / [`BadgeConfig::aria_label`].
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::{badge, BadgeVariant, Column, DataTableConfig, data_table};
/// use maud::html;
///
/// // Inline in a template — color chosen deterministically from the value:
/// let pill = badge("published", BadgeVariant::for_label("published"));
/// assert!(pill.into_string().contains(r#"class="badge badge--success""#));
///
/// // Inside a `data_table` cell:
/// struct Post { title: String, status: String }
/// let posts = vec![Post { title: "Hi".into(), status: "draft".into() }];
/// let cols: Vec<Column<Post>> = vec![
///     Column::new("Title",  |p: &Post| html! { (p.title.as_str()) }),
///     Column::new("Status", |p: &Post| badge(&p.status, BadgeVariant::for_label(&p.status))),
/// ];
/// let table = data_table(&posts, &cols, &DataTableConfig::new("No posts.")).into_string();
/// assert!(table.contains("badge--warning")); // "draft" → Warning
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn badge(label: &str, variant: BadgeVariant) -> maud::Markup {
    badge_with(label, variant, &BadgeConfig::new())
}

/// Render a status pill with a [`BadgeConfig`] (tooltip / `aria-label` / extra
/// class). See [`badge`] for the common case.
#[cfg(feature = "maud")]
#[must_use]
pub fn badge_with(label: &str, variant: BadgeVariant, config: &BadgeConfig<'_>) -> maud::Markup {
    let root = merge_class(&format!("badge badge--{}", variant.as_str()), config.class);
    maud::html! {
        span class=(root) title=[config.title] aria-label=[config.aria_label] { (label) }
    }
}

/// Render a neutral status pill in one call — shorthand for the common
/// "just show this value as a badge" case.
///
/// Equivalent to `badge(label, BadgeVariant::Neutral)`. For a color chosen from
/// the value, use `badge(label, BadgeVariant::for_label(label))`.
///
/// The admin plugin's hand-rolled `badge badge-{op}` audit-log spans are the
/// migration target for this helper; the `.badge` class contract here is a
/// superset that can replace them.
#[cfg(feature = "maud")]
#[must_use]
pub fn status_tag(label: &str) -> maud::Markup {
    badge(label, BadgeVariant::Neutral)
}

// ── avatar (#1263) ──────────────────────────────────────────────────────────

/// Named avatar size. Each maps to a fixed, square pixel dimension.
#[cfg(feature = "maud")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AvatarSize {
    /// 24×24 — inline with text, dense lists.
    Small,
    /// 40×40 — the default, member rows and nav bars.
    #[default]
    Medium,
    /// 64×64 — profile headers.
    Large,
}

#[cfg(feature = "maud")]
impl AvatarSize {
    /// The square edge length in CSS pixels.
    #[must_use]
    pub const fn px(self) -> u32 {
        match self {
            Self::Small => 24,
            Self::Medium => 40,
            Self::Large => 64,
        }
    }

    /// The size modifier class.
    const fn class(self) -> &'static str {
        match self {
            Self::Small => "autumn-avatar--sm",
            Self::Medium => "autumn-avatar--md",
            Self::Large => "autumn-avatar--lg",
        }
    }
}

/// Rendering options for [`avatar`].
///
/// Build with [`AvatarConfig::new`] and chain the setters. With no image URL,
/// [`avatar`] renders a deterministic colored-initials badge instead.
#[cfg(feature = "maud")]
#[derive(Debug, Clone, Default)]
pub struct AvatarConfig<'a> {
    image_url: Option<&'a str>,
    size: AvatarSize,
    class: Option<&'a str>,
}

#[cfg(feature = "maud")]
impl<'a> AvatarConfig<'a> {
    /// A default config: no image (initials fallback), [`AvatarSize::Medium`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the image URL. An empty/whitespace-only URL is treated as absent
    /// (falls back to initials), so a `Some("")` from the database never
    /// renders a broken image.
    #[must_use]
    pub const fn image(mut self, url: &'a str) -> Self {
        self.image_url = Some(url);
        self
    }

    /// Set the avatar size (default [`AvatarSize::Medium`]).
    #[must_use]
    pub const fn size(mut self, size: AvatarSize) -> Self {
        self.size = size;
        self
    }

    /// Append an extra class to the root element.
    #[must_use]
    pub const fn class(mut self, class: &'a str) -> Self {
        self.class = Some(class);
        self
    }
}

/// Derive 1–2 uppercase initials from a display name.
///
/// `"Ada Lovelace"` → `"AL"`, `"cher"` → `"C"`, `"  José  "` → `"J"`. Unicode
/// safe (uppercases the first *character*, not byte) and never panics: an
/// empty or whitespace-only name yields a neutral placeholder glyph.
#[cfg(feature = "maud")]
fn avatar_initials(name: &str) -> String {
    let words: Vec<&str> = name.split_whitespace().collect();
    let first_char = |w: &str| w.chars().next();
    match words.as_slice() {
        [] => "?".to_string(),
        [only] => {
            first_char(only).map_or_else(|| "?".to_string(), |c| c.to_uppercase().to_string())
        }
        [first, .., last] => match (first_char(first), first_char(last)) {
            (Some(a), Some(b)) => format!("{}{}", a.to_uppercase(), b.to_uppercase()),
            _ => "?".to_string(),
        },
    }
}

/// The deterministic avatar background-color palette. Each entry is a CSS class
/// (backed by a rule in the widget stylesheet) rather than an inline color, so
/// the per-name color survives a nonce-based Content-Security-Policy — which
/// forbids inline `style` attributes (`style-src` gains a nonce and drops
/// `'unsafe-inline'`; nonces cover `<style>` elements, not attribute styles).
#[cfg(feature = "maud")]
const AVATAR_PALETTE: [&str; 12] = [
    "autumn-avatar--c0",
    "autumn-avatar--c1",
    "autumn-avatar--c2",
    "autumn-avatar--c3",
    "autumn-avatar--c4",
    "autumn-avatar--c5",
    "autumn-avatar--c6",
    "autumn-avatar--c7",
    "autumn-avatar--c8",
    "autumn-avatar--c9",
    "autumn-avatar--c10",
    "autumn-avatar--c11",
];

/// Deterministic background-color class for a name's initials badge. The same
/// name always yields the same class across renders/machines, and the color is
/// applied via the stylesheet (no inline `style` attribute) so it is not
/// stripped by a nonce-based CSP.
#[cfg(feature = "maud")]
fn avatar_palette_class(name: &str) -> &'static str {
    // `% 12` (a literal) keeps the index in range without a truncating `usize`
    // cast; the assertion keeps the literal in sync with the palette length.
    const _: () = assert!(AVATAR_PALETTE.len() == 12);
    AVATAR_PALETTE[(fnv1a_hash(name.trim()) % 12) as usize]
}

/// Render a person's avatar: an image when one is supplied, or a deterministic
/// colored-initials badge when it isn't.
///
/// * **Image branch** — `<img>` with the given `src`, a non-empty `alt` derived
///   from `name`, `loading="lazy"`, and square `width`/`height` for the chosen
///   size (no layout shift).
/// * **Initials branch** — a square badge with 1–2 uppercase initials and a
///   background color chosen deterministically from `name` (via a stable
///   `autumn-avatar--cN` palette class), so the same person always gets the
///   same color and there is never a broken-image request.
///
/// Both `name` and the image URL are HTML-escaped by Maud (no XSS via a crafted
/// display name). All output carries `autumn-avatar*` classes backed by the
/// framework stylesheet and emits **no inline `style` attribute** — the per-name
/// color is a palette class, so it survives a nonce-based Content-Security-Policy
/// that forbids inline styles.
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::{avatar, AvatarConfig, AvatarSize};
/// use maud::html;
///
/// // A member row: avatar + name.
/// let row = html! {
///     div class="member" {
///         (avatar("Ada Lovelace", &AvatarConfig::new().size(AvatarSize::Small)))
///         span { "Ada Lovelace" }
///     }
/// };
/// let out = row.into_string();
/// assert!(out.contains("autumn-avatar--initials")); // no image → initials
/// assert!(out.contains(">AL<"));                      // deterministic initials
///
/// // With an image:
/// let img = avatar("Ada", &AvatarConfig::new().image("/u/ada.png")).into_string();
/// assert!(img.contains(r#"loading="lazy""#));
/// assert!(img.contains(r#"alt="Ada""#));
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn avatar(name: &str, config: &AvatarConfig<'_>) -> maud::Markup {
    let px = config.size.px();
    let size_class = config.size.class();
    let alt = if name.trim().is_empty() {
        "avatar".to_string()
    } else {
        name.to_string()
    };
    match config.image_url {
        Some(url) if !url.trim().is_empty() => {
            let root = merge_class(
                &format!("autumn-avatar autumn-avatar--image {size_class}"),
                config.class,
            );
            maud::html! {
                img class=(root) src=(url) alt=(alt)
                    loading="lazy" width=(px) height=(px);
            }
        }
        _ => {
            let palette = avatar_palette_class(name);
            let root = merge_class(
                &format!("autumn-avatar autumn-avatar--initials {size_class} {palette}"),
                config.class,
            );
            maud::html! {
                span class=(root) role="img" aria-label=(alt) title=(alt) {
                    span class="autumn-avatar__initials" aria-hidden="true" { (avatar_initials(name)) }
                }
            }
        }
    }
}

// ── alert / callout (#1314) ─────────────────────────────────────────────────

/// Semantic variant for an [`alert`] / [`error_summary`] callout.
///
/// Names match the flash levels (`info`/`success`/`warning`/`error`) so both
/// share one stylesheet. `Error`/`Warning` render `role="alert"`;
/// `Info`/`Success` render `role="status"`.
#[cfg(feature = "maud")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AlertVariant {
    /// Neutral information (blue). The default.
    #[default]
    Info,
    /// Positive confirmation (green).
    Success,
    /// Something needs attention (amber).
    Warning,
    /// Something went wrong (red).
    Error,
}

#[cfg(feature = "maud")]
impl AlertVariant {
    /// The variant name as a lowercase string (matches the CSS class suffix and
    /// the flash-level names).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Success => "success",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }

    /// The ARIA live-region role: `alert` for error/warning (assertive),
    /// `status` for info/success (polite).
    const fn role(self) -> &'static str {
        match self {
            Self::Error | Self::Warning => "alert",
            Self::Info | Self::Success => "status",
        }
    }

    /// A decorative inline-SVG icon for this variant (no icon-font dependency).
    fn icon(self) -> maud::Markup {
        let glyph = match self {
            Self::Info => "i",
            Self::Success => "✓",
            Self::Warning => "!",
            Self::Error => "×",
        };
        maud::html! {
            svg class="alert__icon-svg" viewBox="0 0 20 20" width="20" height="20"
                aria-hidden="true" focusable="false" {
                circle cx="10" cy="10" r="10" fill="currentColor" {}
                text x="10" y="15" text-anchor="middle" font-size="13"
                    font-family="sans-serif" fill="#fff" { (glyph) }
            }
        }
    }
}

/// Builder options for [`alert_with`].
#[cfg(feature = "maud")]
#[derive(Debug, Clone, Default)]
pub struct AlertConfig<'a> {
    title: Option<&'a str>,
    icon: bool,
    dismissible: bool,
    class: Option<&'a str>,
}

#[cfg(feature = "maud")]
impl<'a> AlertConfig<'a> {
    /// A default config: no title, no icon, not dismissible.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set a heading rendered above the body (an `<h2 class="alert__title">`).
    #[must_use]
    pub const fn title(mut self, title: &'a str) -> Self {
        self.title = Some(title);
        self
    }

    /// Render the variant's leading inline-SVG icon.
    #[must_use]
    pub const fn icon(mut self, show: bool) -> Self {
        self.icon = show;
        self
    }

    /// Render a no-JavaScript dismiss control (a `<label>`-wrapped hidden
    /// checkbox; the stylesheet's `:has()` rule hides the alert when checked).
    /// When omitted, no close control and no JS are emitted.
    #[must_use]
    pub const fn dismissible(mut self, yes: bool) -> Self {
        self.dismissible = yes;
        self
    }

    /// Append an extra class to the root element.
    #[must_use]
    pub const fn class(mut self, class: &'a str) -> Self {
        self.class = Some(class);
        self
    }
}

/// Render an inline, block-level contextual message box.
///
/// Emits `<div class="alert alert-<variant>" role="...">` wrapping `body`.
/// `role` is `alert` for `Error`/`Warning`, `status` for `Info`/`Success`. All
/// caller-supplied markup is escaped by Maud. No inline `style=` is emitted and
/// no JavaScript is required. For a title, icon, or dismiss control use
/// [`alert_with`].
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::{alert, AlertVariant};
/// use maud::html;
///
/// let box_ = alert(AlertVariant::Info, html! { "No posts yet — create your first one." });
/// let out = box_.into_string();
/// assert!(out.contains(r#"class="alert alert-info""#));
/// assert!(out.contains(r#"role="status""#));
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn alert(variant: AlertVariant, body: maud::Markup) -> maud::Markup {
    alert_with(variant, body, &AlertConfig::new())
}

/// Render an alert with a [`AlertConfig`] (title, icon, dismiss control).
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::{alert_with, AlertConfig, AlertVariant};
/// use maud::html;
///
/// let out = alert_with(
///     AlertVariant::Error,
///     html! { "Your card was declined." },
///     &AlertConfig::new().title("Payment failed").icon(true).dismissible(true),
/// )
/// .into_string();
/// assert!(out.contains(r#"role="alert""#));
/// assert!(out.contains(r#"<h2 class="alert__title">Payment failed</h2>"#));
/// assert!(out.contains("alert__dismiss"));
/// assert!(!out.contains("<script")); // no JS
/// ```
// `body` is taken by value to match the issue's `alert(variant, body: Markup)`
// signature and keep `alert(v, html! { .. })` ergonomic (no `&` at the call
// site); Maud renders it by reference, hence the allow.
#[cfg(feature = "maud")]
#[must_use]
#[allow(clippy::needless_pass_by_value)]
pub fn alert_with(
    variant: AlertVariant,
    body: maud::Markup,
    config: &AlertConfig<'_>,
) -> maud::Markup {
    let root = merge_class(&format!("alert alert-{}", variant.as_str()), config.class);
    maud::html! {
        div class=(root) role=(variant.role()) {
            @if config.icon {
                span class="alert__icon" { (variant.icon()) }
            }
            div class="alert__content" {
                @if let Some(title) = config.title {
                    (heading(HeadingLevel::H2, None, "alert__title", &maud::html! { (title) }))
                }
                div class="alert__body" { (body) }
            }
            @if config.dismissible {
                // The checkbox stays in tab order (visually hidden but focusable
                // via `.alert__dismiss-toggle`, never `hidden`/`display:none`) so
                // keyboard and screen-reader users can dismiss; the accessible
                // name is on the control itself.
                label class="alert__dismiss" {
                    input type="checkbox" class="alert__dismiss-toggle"
                        aria-label="Dismiss this message";
                    span aria-hidden="true" { "×" }
                }
            }
        }
    }
}

/// Render an [`AlertVariant::Error`] alert summarizing every field error in a
/// [`Changeset`](crate::form::Changeset), or `None` when the changeset is valid.
///
/// The messages are listed in a `<ul>` in a stable (sorted) order — directly
/// usable at the top of a re-rendered form. Each message is HTML-escaped.
///
/// # Example
///
/// ```rust
/// use std::collections::HashMap;
/// use autumn_web::form::Changeset;
/// use autumn_web::widgets::error_summary;
///
/// let valid = Changeset::new(());
/// assert!(error_summary(&valid).is_none());
///
/// let mut errs = HashMap::new();
/// errs.insert("email".to_string(), vec!["is invalid".to_string()]);
/// errs.insert("name".to_string(), vec!["must be <set>".to_string()]);
/// let invalid = Changeset::from_errors((), errs);
/// let out = error_summary(&invalid).unwrap().into_string();
/// assert!(out.contains(r#"class="alert alert-error""#));
/// assert!(out.contains("<ul"));
/// assert!(out.contains("is invalid"));
/// assert!(out.contains("must be &lt;set&gt;")); // caller text escaped
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn error_summary<T>(changeset: &crate::form::Changeset<T>) -> Option<maud::Markup> {
    if changeset.is_valid() {
        return None;
    }
    // Stable order: sort by (field, message) so the summary is deterministic.
    let mut messages: Vec<&str> = Vec::new();
    let mut fields: Vec<&String> = changeset.all_errors().keys().collect();
    fields.sort();
    for field in fields {
        for msg in &changeset.all_errors()[field] {
            messages.push(msg.as_str());
        }
    }
    let body = maud::html! {
        ul class="alert__errors" {
            @for msg in &messages {
                li { (msg) }
            }
        }
    };
    Some(alert_with(
        AlertVariant::Error,
        body,
        &AlertConfig::new()
            .title("Please fix the following errors")
            .icon(true),
    ))
}

// ── toast / toast_region (#1320) ────────────────────────────────────────────

/// The default id for the [`toast_region`] container, and the OOB target
/// [`toast`] appends into.
///
/// Drop `toast_region(DEFAULT_TOAST_REGION_ID)` once in your base layout;
/// `toast(msg, variant)` then appends into it out-of-band.
pub const DEFAULT_TOAST_REGION_ID: &str = "toast-region";

/// Render the persistent, fixed-position live region that toasts append into.
///
/// Drop this **once** into your base layout (typically just before `</body>`)
/// and leave it in place: it is an **empty, persistent `aria-live="polite"`
/// region**. Appending a toast into an already-in-DOM live region is announced
/// reliably by screen readers, whereas inserting an already-populated
/// `aria-live` element is not — which is why the politeness lives here, on the
/// region, rather than on each non-error toast. `aria-atomic="false"` (and no
/// `role="status"`, which would imply `aria-atomic="true"`) means only the
/// newly-appended toast is announced, not every toast still on screen. Position
/// and stacking come from the shipped `.autumn-toast-region` rule — no inline
/// styles.
///
/// Non-error toasts inherit this region's politeness; error toasts announce
/// assertively on their own via `role="alert"`. Use [`DEFAULT_TOAST_REGION_ID`]
/// unless you need more than one region; a custom id must be paired with
/// [`toast_in`] so the OOB target matches.
///
/// # CSS hooks
///
/// | Selector | Element |
/// |---|---|
/// | `.autumn-toast-region` | Fixed-position, persistent polite live region |
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::{toast_region, DEFAULT_TOAST_REGION_ID};
///
/// let html = toast_region(DEFAULT_TOAST_REGION_ID).into_string();
/// assert!(html.contains(r#"id="toast-region""#));
/// assert!(html.contains(r#"class="autumn-toast-region""#));
/// assert!(html.contains(r#"aria-live="polite""#));   // persistent live region
/// assert!(html.contains(r#"aria-atomic="false""#));  // announce only new toasts
/// assert!(!html.contains(r#"role="status""#));       // role=status implies atomic
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn toast_region(id: &str) -> maud::Markup {
    // Accept either a bare id or a `#selector` (a natural caller slip); strip a
    // leading `#` so the element id is always bare, matching the id that
    // `toast_in`'s `beforeend:#…` OOB target points at.
    let id = selector_to_id(id);
    // The region is a PERSISTENT polite live region: a `role="status"`/
    // `aria-live="polite"` element inserted already-populated is not reliably
    // announced, so instead this empty region lives in the layout and toasts are
    // appended into it (screen readers announce the mutation). `aria-atomic` is
    // explicitly `false` so only the newly-appended toast is read, not every
    // toast currently in the region — and `role="status"` is deliberately NOT
    // used because it implies `aria-atomic="true"`, which would re-announce all
    // of them. Non-error toasts inherit this politeness (they carry no own
    // `aria-live`); error toasts announce assertively on their own.
    maud::html! {
        div id=(id) class="autumn-toast-region" aria-live="polite" aria-atomic="false" {}
    }
}

/// Render a single transient toast that appends into the default toast region
/// out-of-band, for htmx action feedback.
///
/// This is the common case: it targets [`DEFAULT_TOAST_REGION_ID`]. Drop
/// [`toast_region`] once in your layout, then return this alongside your normal
/// swapped fragment from an htmx handler; htmx appends it into the region via
/// `hx-swap-oob="beforeend:#toast-region"` — no full page reload, no
/// JavaScript. Auto-dismiss is CSS-only (a `@keyframes` fade in the shipped
/// stylesheet); the widget emits **no `<script>`**.
///
/// `variant` reuses the shared [`AlertVariant`] semantic enum (the alert/badge
/// color lane) — no new color vocabulary. **Accessibility:** `Error` toasts
/// announce assertively on their own (`role="alert"`, `aria-live="assertive"`),
/// which is reliably read on insertion; all other variants carry **no own
/// `role`/`aria-live`** and instead inherit the persistent
/// `aria-live="polite"` of the [`toast_region`] they're appended into (an
/// already-populated polite element inserted fresh is otherwise not reliably
/// announced). Every toast carries `aria-atomic="true"` so its message is read
/// as one unit. The message is HTML-escaped by Maud.
///
/// Use [`toast_in`] instead when you rendered [`toast_region`] with a custom id.
///
/// # CSS hooks
///
/// | Selector | Element |
/// |---|---|
/// | `.autumn-toast` | Root toast (carries the auto-dismiss animation) |
/// | `.autumn-toast--<variant>` | Per-variant accent (`success`/`info`/`warning`/`error`) |
/// | `.autumn-toast__message` | The message text |
///
/// # Example: end-to-end htmx recipe
///
/// ```rust
/// use autumn_web::widgets::{toast, toast_region, AlertVariant, DEFAULT_TOAST_REGION_ID};
/// use maud::html;
///
/// // 1. Once, in your base layout (before `</body>`) — the persistent live region:
/// //    (toast_region(DEFAULT_TOAST_REGION_ID))
/// let layout = toast_region(DEFAULT_TOAST_REGION_ID).into_string();
/// assert!(layout.contains(r#"id="toast-region""#));
/// assert!(layout.contains(r#"aria-live="polite""#));  // politeness lives here
///
/// // 2. From an htmx create/update/delete handler, return the toast next to
/// //    your swapped fragment — htmx appends it into the region OOB:
/// let response = html! {
///     (toast("Saved", AlertVariant::Success))   // hx-swap-oob append
///     tr id="row-42" { td { "…updated row…" } } // your normal swap
/// };
/// let out = response.into_string();
/// assert!(out.contains(r##"hx-swap-oob="beforeend:#toast-region""##));
/// // A non-error toast inherits the region's politeness: no own role/aria-live,
/// // but it is atomic so its message is announced as one unit.
/// assert!(out.contains(r#"aria-atomic="true""#));
/// assert!(!out.contains(r#"role="status""#));
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn toast(message: &str, variant: AlertVariant) -> maud::Markup {
    toast_in(DEFAULT_TOAST_REGION_ID, message, variant)
}

/// Like [`toast`], but appends into a region with a caller-chosen id.
///
/// Use this when your layout renders [`toast_region`] with an id other than
/// [`DEFAULT_TOAST_REGION_ID`] (e.g. separate top/bottom regions). The emitted
/// `hx-swap-oob="beforeend:#<region_id>"` targets that region. `region_id` is
/// HTML-escaped in the attribute like every other caller value.
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::{toast_in, AlertVariant};
///
/// let out = toast_in("top-toasts", "Uploaded", AlertVariant::Info).into_string();
/// assert!(out.contains(r##"hx-swap-oob="beforeend:#top-toasts""##));
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn toast_in(region_id: &str, message: &str, variant: AlertVariant) -> maud::Markup {
    // Error toasts announce assertively on their own — `role="alert"` elements
    // ARE reliably announced on insertion, so they don't depend on the
    // persistent region. Non-error toasts carry NO `role`/`aria-live`: they
    // inherit the `aria-live="polite"` of the `toast_region` they're appended
    // into (a freshly-inserted, already-populated polite element is otherwise
    // not reliably announced). Every toast keeps `aria-atomic="true"` so its
    // message is read as one unit.
    let (role, live): (Option<&str>, Option<&str>) = match variant {
        AlertVariant::Error => (Some("alert"), Some("assertive")),
        AlertVariant::Info | AlertVariant::Success | AlertVariant::Warning => (None, None),
    };
    let class = format!("autumn-toast autumn-toast--{}", variant.as_str());
    // Accept a bare id or a `#selector`; strip one leading `#` so a caller
    // passing `"#toast-region"` doesn't produce an invalid `beforeend:##…`.
    let region_id = selector_to_id(region_id);
    let oob = format!("beforeend:#{region_id}");
    // For a POSITIONAL OOB swap (`beforeend:#…`) htmx inserts the carrier's
    // *children* into the target and discards the carrier itself, so the carrier
    // must be a plain, discardable `<div>` — NOT a `<template>`.
    //
    // Authority — vendored htmx 2.0.4 (`vendor/htmx.min.js`):
    //   * `oobSwap` (minified `He`): for a non-outerHTML style it swaps
    //     `f(clone)`, i.e. the *carrier element* (`f` returns the element as-is),
    //     then `_e("beforeend", …)` → `Be` → `a`, whose insert loop is
    //     `while(n.childNodes.length>0)` — it iterates the carrier's `childNodes`.
    //   * The response OOB scan `ze` matches a top-level `<template hx-swap-oob=…>`
    //     (`parentElement===null`), so the template is processed as the carrier
    //     itself, not unwrapped. A `<template>` keeps its children in `.content`,
    //     so `template.childNodes` is EMPTY → htmx appends nothing and the toast
    //     never shows. (The template-content rescan only helps when `hx-swap-oob`
    //     sits on a CHILD *inside* `.content`, which is not this positional case.)
    //
    // This mirrors the repo's own tested SSE convention in `channels.rs` (see the
    // passing `broadcast_publish_oob_custom_strategy` test →
    // `<div hx-swap-oob="beforeend:#badge"><span>3</span></div>`). The styled/ARIA
    // `.autumn-toast` element stays the carrier's CHILD; putting `hx-swap-oob`
    // directly on it would throw the wrapper away and append only the inner
    // `<span>`.
    maud::html! {
        div hx-swap-oob=(oob) {
            div class=(class) role=[role] aria-live=[live] aria-atomic="true" {
                span class="autumn-toast__message" { (message) }
            }
        }
    }
}

// ── infinite_feed (#1372) ───────────────────────────────────────────────────

/// How the continuation control of an [`infinite_feed`] loads the next page.
///
/// Both modes replace the sentinel with the next page's fragment (its items
/// plus a fresh sentinel), so appends never duplicate already-rendered rows.
#[cfg(feature = "maud")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FeedMode {
    /// Auto-load when the sentinel scrolls into view, and also on a manual
    /// click of the visible link (`hx-trigger="revealed, click"`). The classic
    /// infinite scroll. This is the default.
    #[default]
    Reveal,
    /// Require an explicit click on the keyboard-focusable "Load more" control
    /// (htmx's default `click` trigger); nothing loads until the user asks.
    Button,
}

/// Configuration for [`infinite_feed`] / [`feed_page`].
///
/// Build with [`FeedConfig::new`] and chain setters. `url` is the partial
/// handler that returns the next page fragment; the next cursor is appended to
/// it as a query parameter (`?cursor=…` by default).
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::{FeedConfig, FeedMode};
///
/// let config = FeedConfig::new("/posts/feed")
///     .mode(FeedMode::Button)
///     .load_more_label("Show more posts");
/// assert_eq!(config.cursor_param, "cursor");
/// ```
#[cfg(feature = "maud")]
#[derive(Debug, Clone)]
pub struct FeedConfig<'a> {
    /// URL of the partial handler that returns the next page fragment.
    pub url: &'a str,
    /// How the continuation control loads (default [`FeedMode::Reveal`]).
    pub mode: FeedMode,
    /// Query-parameter name carrying the cursor token (default `"cursor"`).
    pub cursor_param: &'a str,
    /// Visible label / accessible name of the continuation control
    /// (default `"Load more"`).
    pub load_more_label: &'a str,
}

#[cfg(feature = "maud")]
impl<'a> FeedConfig<'a> {
    /// Start a config for the partial handler at `url` with sensible defaults
    /// (reveal mode, `cursor` param, "Load more" label).
    #[must_use]
    pub const fn new(url: &'a str) -> Self {
        Self {
            url,
            mode: FeedMode::Reveal,
            cursor_param: "cursor",
            load_more_label: "Load more",
        }
    }

    /// Set the load [`FeedMode`].
    #[must_use]
    pub const fn mode(mut self, mode: FeedMode) -> Self {
        self.mode = mode;
        self
    }

    /// Shorthand for `.mode(FeedMode::Button)`.
    #[must_use]
    pub const fn button(mut self) -> Self {
        self.mode = FeedMode::Button;
        self
    }

    /// Override the cursor query-parameter name (default `"cursor"`).
    #[must_use]
    pub const fn cursor_param(mut self, param: &'a str) -> Self {
        self.cursor_param = param;
        self
    }

    /// Override the continuation control's label (default `"Load more"`).
    #[must_use]
    pub const fn load_more_label(mut self, label: &'a str) -> Self {
        self.load_more_label = label;
        self
    }
}

/// Percent-encode a query-string component (param name or value).
///
/// Cursor tokens produced by [`CursorPage`](crate::pagination::CursorPage) are
/// URL-safe base64 (signed cursors add a `.`-delimited signature, also
/// URL-safe), but [`infinite_feed`]/[`feed_page`] accept an arbitrary
/// `Option<&str>`, so a token containing `&`, `#`, `=`, `+`, or a space must be
/// encoded to avoid corrupting the query string or injecting extra params. The
/// unreserved set (`A–Z a–z 0–9 - _ . ~`) — which covers every real cursor —
/// passes through unchanged.
#[cfg(feature = "maud")]
fn encode_query_component(value: &str) -> String {
    use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
    // Everything outside the RFC 3986 unreserved set.
    const QUERY_COMPONENT: &AsciiSet = &CONTROLS
        .add(b' ')
        .add(b'"')
        .add(b'#')
        .add(b'%')
        .add(b'&')
        .add(b'+')
        .add(b'/')
        .add(b';')
        .add(b'=')
        .add(b'?')
        .add(b'@')
        .add(b'[')
        .add(b'\\')
        .add(b']')
        .add(b'^')
        .add(b'`')
        .add(b'{')
        .add(b'|')
        .add(b'}')
        .add(b'<')
        .add(b'>');
    utf8_percent_encode(value, QUERY_COMPONENT).to_string()
}

/// Build the next-page URL by appending `?<cursor_param>=<cursor>` to the config
/// URL (using `&` if it already has a query string), with both the param name
/// and the cursor percent-encoded via [`encode_query_component`]. Any existing
/// `#fragment` is preserved after the query (the query must precede it). Maud
/// additionally HTML-escapes the whole value in the attribute.
#[cfg(feature = "maud")]
fn feed_next_url(config: &FeedConfig<'_>, cursor: &str) -> String {
    // The query must go before any `#fragment`.
    let (base, fragment) = match config.url.split_once('#') {
        Some((base, frag)) => (base, Some(frag)),
        None => (config.url, None),
    };
    let sep = if base.contains('?') { '&' } else { '?' };
    let param = encode_query_component(config.cursor_param);
    let value = encode_query_component(cursor);
    let mut url = format!("{base}{sep}{param}={value}");
    if let Some(frag) = fragment {
        url.push('#');
        url.push_str(frag);
    }
    url
}

/// Render the continuation sentinel for a `next_cursor`.
///
/// A single control that is BOTH a progressive-enhancement `<a href>` to the
/// next-cursor URL (works with htmx/JS unavailable) AND htmx-wired to fetch
/// that URL and replace the whole sentinel (`hx-swap="outerHTML"`, targeting
/// the closest `.autumn-feed__sentinel`) with the returned fragment. In
/// [`FeedMode::Reveal`] it fires on `revealed, click` — auto-loading when
/// scrolled into view, but also swapping in place (rather than navigating to
/// the bare partial) if a user activates the visible link first; in
/// [`FeedMode::Button`] it uses htmx's default `click` trigger. htmx
/// `preventDefault`s the anchor navigation whenever it handles the trigger, so
/// the `<a href>` stays a pure no-JS fallback.
#[cfg(feature = "maud")]
fn feed_sentinel(next_cursor: &str, config: &FeedConfig<'_>) -> maud::Markup {
    let next_url = feed_next_url(config, next_cursor);
    let trigger = match config.mode {
        // Include `click` so a user activating the visible "Load more" link
        // before `revealed` fires swaps in place instead of following `href`.
        FeedMode::Reveal => Some("revealed, click"),
        FeedMode::Button => None,
    };
    maud::html! {
        div class="autumn-feed__sentinel" {
            a class="autumn-feed__more" href=(next_url)
                hx-get=(next_url) hx-target="closest .autumn-feed__sentinel"
                hx-swap="outerHTML" hx-trigger=[trigger] {
                (config.load_more_label)
            }
        }
    }
}

/// Render just the next-page fragment: the already-rendered `items` followed by
/// a fresh sentinel (only when `next_cursor` is `Some`).
///
/// This is the companion helper a handler returns for **each append** — and it
/// is exactly what [`infinite_feed`] wraps for the initial view, so one partial
/// serves both. Because the sentinel swaps itself via `outerHTML`, returning
/// `items + new sentinel` inserts the new rows where the old sentinel was and
/// re-arms the loop; when `next_cursor` is `None` no sentinel is emitted, so no
/// further request fires (the feed terminates cleanly).
///
/// `items` is caller-rendered [`Markup`](maud::Markup) (already escaped by your
/// own Maud); pass the cursor from
/// [`CursorPage::next_cursor`](crate::pagination::CursorPage) via `.as_deref()`.
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::{feed_page, FeedConfig};
/// use maud::html;
///
/// let items = html! { article { "row 1" } article { "row 2" } };
/// let config = FeedConfig::new("/feed");
/// // Middle page: has a next cursor → emits a sentinel.
/// let more = feed_page(items, Some("eyJpZCI6Mn0"), &config).into_string();
/// assert!(more.contains("autumn-feed__sentinel"));
/// assert!(more.contains(r##"hx-get="/feed?cursor=eyJpZCI6Mn0""##));
///
/// // Last page: no next cursor → no sentinel, no further request.
/// let last = feed_page(html! { article { "row 3" } }, None, &config).into_string();
/// assert!(!last.contains("autumn-feed__sentinel"));
/// ```
// `items` is taken by value to keep `feed_page(html! { .. }, ..)` ergonomic (no
// `&` at the call site, matching `alert`); Maud renders it by reference, hence
// the allow.
#[cfg(feature = "maud")]
#[must_use]
#[allow(clippy::needless_pass_by_value)]
pub fn feed_page(
    items: maud::Markup,
    next_cursor: Option<&str>,
    config: &FeedConfig<'_>,
) -> maud::Markup {
    maud::html! {
        (items)
        @if let Some(cursor) = next_cursor {
            (feed_sentinel(cursor, config))
        }
    }
}

/// Render an htmx infinite-scroll / "Load more" feed for a cursor-paginated
/// list: a `.autumn-feed` container holding the rendered `items` and, when
/// there is a next page, a continuation sentinel.
///
/// Driven by a [`CursorPage`](crate::pagination::CursorPage): pass your rendered
/// items and `page.next_cursor.as_deref()`. When the cursor is `Some`, the
/// sentinel issues a single `hx-get` to `config.url` carrying the cursor and
/// appends the returned fragment in place — no full-page reload, no duplicated
/// rows (keyset cursors are stable under concurrent inserts). When it is `None`
/// (`has_next == false`) no sentinel is emitted, so no terminal request fires.
///
/// Two modes ([`FeedMode`]): auto-load on reveal, or an explicit,
/// keyboard-focusable "Load more" control. Either way the control is a real
/// `<a href>` to the next-cursor URL, so the next page is reachable with
/// htmx/JS unavailable. The append fragment is produced by [`feed_page`], which
/// a handler returns for every subsequent page.
///
/// # CSS hooks
///
/// | Selector | Element |
/// |---|---|
/// | `.autumn-feed` | Feed container |
/// | `.autumn-feed__sentinel` | Continuation sentinel (swapped out on load) |
/// | `.autumn-feed__more` | The `<a>`/"Load more" control |
///
/// # Progressive enhancement caveat
///
/// The `<a href>` fallback points at your partial handler (`config.url`), which
/// normally returns just the next-page *fragment*. With JavaScript off, clicking
/// it therefore lands on that bare fragment unless the handler detects a
/// non-htmx request (no `HX-Request` header) and renders a full page for it.
///
/// # Example: initial view + append handler
///
/// ```rust
/// use autumn_web::widgets::{infinite_feed, feed_page, FeedConfig, FeedMode};
/// use autumn_web::pagination::CursorPage;
/// use maud::html;
///
/// // Render each item however you like (this is caller-owned Markup):
/// fn render(rows: &[&str]) -> maud::Markup {
///     html! { @for r in rows { article class="post" { (r) } } }
/// }
///
/// // Page 1 (has a next page):
/// let page = CursorPage { content: vec!["a", "b"], size: 2,
///     next_cursor: Some("eyJpZCI6Mn0".into()), has_next: true };
/// let config = FeedConfig::new("/posts/feed").mode(FeedMode::Reveal);
/// let view = infinite_feed(render(&page.content), page.next_cursor.as_deref(), &config)
///     .into_string();
/// assert!(view.contains(r#"class="autumn-feed""#));
/// assert!(view.contains(r#"hx-trigger="revealed, click""#));
/// assert!(view.contains(r##"hx-get="/posts/feed?cursor=eyJpZCI6Mn0""##));
///
/// // Your `GET /posts/feed?cursor=…` handler returns ONE partial for each
/// // append — the same fragment shape as above, minus the outer container:
/// let last = CursorPage::<&str> { content: vec![], size: 2, next_cursor: None, has_next: false };
/// let append = feed_page(render(&last.content), last.next_cursor.as_deref(), &config)
///     .into_string();
/// assert!(!append.contains("autumn-feed__sentinel")); // last page → loop stops
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn infinite_feed(
    items: maud::Markup,
    next_cursor: Option<&str>,
    config: &FeedConfig<'_>,
) -> maud::Markup {
    maud::html! {
        div class="autumn-feed" {
            (feed_page(items, next_cursor, config))
        }
    }
}

// ── charts: sparkline / bar_chart / line_chart (#1231) ──────────────────────

/// Rendering options shared by [`sparkline_with`], [`bar_chart_with`], and
/// [`line_chart_with`].
///
/// Build with [`ChartConfig::new`] and chain the setters. Every option is
/// off/`None` by default: the chart auto-scales its axis from the data, uses a
/// sensible default accessible name, and omits the table fallback.
///
/// Charts never emit a `style=` attribute — colors and strokes come from the
/// `.autumn-chart__*` classes referencing design tokens, so they re-theme via
/// `tokens.css` and survive a strict `style-src`/`nonce` CSP.
#[cfg(feature = "maud")]
#[derive(Debug, Clone, Default)]
pub struct ChartConfig<'a> {
    class: Option<&'a str>,
    title: Option<&'a str>,
    desc: Option<&'a str>,
    min: Option<f64>,
    max: Option<f64>,
    table_fallback: bool,
}

#[cfg(feature = "maud")]
impl<'a> ChartConfig<'a> {
    /// A default config: auto-scaled axis, default accessible name, no table
    /// fallback, no extra class.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append an extra class to the root `autumn-chart` element (merged via the
    /// framework's class helper).
    #[must_use]
    pub const fn class(mut self, class: &'a str) -> Self {
        self.class = Some(class);
        self
    }

    /// Set the accessible name — used for the `<svg>` `aria-label` and the
    /// embedded `<title>`. Falls back to a per-kind default (`"Sparkline"`,
    /// `"Bar chart"`, `"Line chart"`) when unset.
    #[must_use]
    pub const fn title(mut self, title: &'a str) -> Self {
        self.title = Some(title);
        self
    }

    /// Set the long description emitted as the SVG `<desc>`. Falls back to a
    /// generated one-line summary of the series when unset.
    #[must_use]
    pub const fn desc(mut self, desc: &'a str) -> Self {
        self.desc = Some(desc);
        self
    }

    /// Override the axis minimum. When unset the minimum is taken from the data
    /// (auto-scaling).
    #[must_use]
    pub const fn min(mut self, min: f64) -> Self {
        self.min = Some(min);
        self
    }

    /// Override the axis maximum. When unset the maximum is taken from the data
    /// (auto-scaling).
    #[must_use]
    pub const fn max(mut self, max: f64) -> Self {
        self.max = Some(max);
        self
    }

    /// Also emit a visually-hidden `<table>` of the `(label, value)` data after
    /// the chart, so assistive tech and no-CSS clients get the exact numbers.
    #[must_use]
    pub const fn with_table(mut self) -> Self {
        self.table_fallback = true;
        self
    }
}

/// Format a computed SVG coordinate to at most three decimals, trimming
/// trailing zeros so the output is compact and deterministic (raw `f64`
/// `Display` produces long, noisy tails).
#[cfg(feature = "maud")]
fn fmt_coord(v: f64) -> String {
    let s = format!("{v:.3}");
    let trimmed = s.trim_end_matches('0').trim_end_matches('.');
    if trimmed.is_empty() || trimmed == "-0" {
        "0".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Format a data value for the visually-hidden table fallback — up to three
/// decimals, trailing zeros trimmed.
#[cfg(feature = "maud")]
fn fmt_value(v: f64) -> String {
    fmt_coord(v)
}

/// Resolve the `(min, max)` axis bounds: caller overrides win, otherwise the
/// bounds are taken from the data. Guarantees `min <= max` even under
/// contradictory overrides, so downstream scaling never divides by a negative
/// span.
#[cfg(feature = "maud")]
fn resolve_bounds(points: &[(&str, f64)], config: &ChartConfig<'_>) -> (f64, f64) {
    let mut data_min = f64::INFINITY;
    let mut data_max = f64::NEG_INFINITY;
    for (_, v) in points {
        if v.is_finite() {
            data_min = data_min.min(*v);
            data_max = data_max.max(*v);
        }
    }
    if !data_min.is_finite() {
        // No finite data points: fall back to a unit range.
        data_min = 0.0;
        data_max = 1.0;
    }
    let mut min = config.min.filter(|v| v.is_finite()).unwrap_or(data_min);
    let mut max = config.max.filter(|v| v.is_finite()).unwrap_or(data_max);
    if max < min {
        std::mem::swap(&mut min, &mut max);
    }
    (min, max)
}

/// Resolve the `(min, max)` axis bounds for a **bar** chart. Identical to
/// [`resolve_bounds`] except that an *automatic* bound (one the caller did not
/// pin via [`ChartConfig::min`] / [`ChartConfig::max`]) is anchored at zero:
/// the automatic lower bound is `min(0.0, data_min)` and the automatic upper
/// bound is `max(0.0, data_max)`. This is the standard bar convention — bars
/// measure magnitude from a zero baseline, so the smallest bar of an
/// all-positive series (e.g. data whose minimum is well above zero) still
/// renders with a visible, proportional height instead of collapsing to zero.
/// An explicit caller override is respected unchanged on the side it pins (no
/// zero is injected there).
#[cfg(feature = "maud")]
fn resolve_bar_bounds(points: &[(&str, f64)], config: &ChartConfig<'_>) -> (f64, f64) {
    let mut data_min = f64::INFINITY;
    let mut data_max = f64::NEG_INFINITY;
    for (_, v) in points {
        if v.is_finite() {
            data_min = data_min.min(*v);
            data_max = data_max.max(*v);
        }
    }
    if !data_min.is_finite() {
        // No finite data points: fall back to a unit range.
        data_min = 0.0;
        data_max = 1.0;
    }
    let mut min = config
        .min
        .filter(|v| v.is_finite())
        .unwrap_or_else(|| data_min.min(0.0));
    let mut max = config
        .max
        .filter(|v| v.is_finite())
        .unwrap_or_else(|| data_max.max(0.0));
    if max < min {
        std::mem::swap(&mut min, &mut max);
    }
    (min, max)
}

/// Map a value to a `0.0..=1.0` fraction of the axis. Returns `0.5` when the
/// span is zero (single point, all-equal values, or `min == max` override),
/// avoiding a divide-by-zero and placing the datum at mid-height.
#[cfg(feature = "maud")]
fn norm_frac(v: f64, min: f64, max: f64) -> f64 {
    let span = max - min;
    if span <= 0.0 || !v.is_finite() {
        0.5
    } else {
        ((v - min) / span).clamp(0.0, 1.0)
    }
}

/// Compute the plotted `(x, y)` pixel coordinates for each point within a
/// `w × h` viewBox, inset by `pad` on every edge.
///
/// The `usize`→`f64` index casts are exact for any realistic point count, and
/// the geometry reads clearer as `base + frac * span` than as `mul_add`.
#[cfg(feature = "maud")]
#[allow(clippy::cast_precision_loss, clippy::suboptimal_flops)]
fn chart_coords(
    points: &[(&str, f64)],
    width: f64,
    height: f64,
    pad: f64,
    min: f64,
    max: f64,
) -> Vec<(f64, f64)> {
    let n = points.len();
    let usable_w = width - 2.0 * pad;
    let usable_h = height - 2.0 * pad;
    points
        .iter()
        .enumerate()
        .map(|(i, (_, value))| {
            let x = if n <= 1 {
                width / 2.0
            } else {
                pad + (i as f64 / (n - 1) as f64) * usable_w
            };
            let frac = norm_frac(*value, min, max);
            let y = pad + (1.0 - frac) * usable_h;
            (x, y)
        })
        .collect()
}

/// The default accessible name / `<title>` per chart kind.
#[cfg(feature = "maud")]
fn chart_default_title(kind: &str) -> &'static str {
    match kind {
        "bar" => "Bar chart",
        "line" => "Line chart",
        _ => "Sparkline",
    }
}

/// Build the SVG `<desc>` text: caller override, else a generated one-line
/// summary (or `"No data"` for an empty series).
#[cfg(feature = "maud")]
fn chart_desc(points: &[(&str, f64)], config: &ChartConfig<'_>) -> String {
    if let Some(desc) = config.desc {
        return desc.to_string();
    }
    if points.is_empty() {
        return "No data".to_string();
    }
    // Report the actual data range from the finite point values, independent of
    // any `config.min`/`config.max` axis override (which controls plotting only).
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for (_, v) in points {
        if v.is_finite() {
            min = min.min(*v);
            max = max.max(*v);
        }
    }
    if !min.is_finite() {
        // No finite data points: fall back to a unit range, matching axis bounds.
        min = 0.0;
        max = 1.0;
    }
    format!(
        "{} data points, ranging from {} to {}.",
        points.len(),
        fmt_value(min),
        fmt_value(max)
    )
}

/// The visually-hidden `(label, value)` table fallback (AC #5).
#[cfg(feature = "maud")]
fn chart_table(points: &[(&str, f64)]) -> maud::Markup {
    maud::html! {
        table class="autumn-chart__table autumn-visually-hidden" {
            thead { tr { th { "Label" } th { "Value" } } }
            tbody {
                @for (label, value) in points {
                    tr {
                        td { (label) }
                        td { (fmt_value(*value)) }
                    }
                }
            }
        }
    }
}

/// Wrap chart `body` in the accessible `autumn-chart` root: a `<div>` carrying
/// the kind modifier and merged class, an `<svg role="img">` whose first
/// children are `<title>`/`<desc>`, and an optional table fallback.
#[cfg(feature = "maud")]
fn chart_root(
    kind: &str,
    view_box: &str,
    points: &[(&str, f64)],
    config: &ChartConfig<'_>,
    body: &maud::Markup,
) -> maud::Markup {
    let root = merge_class(&format!("autumn-chart autumn-chart--{kind}"), config.class);
    let label = config.title.unwrap_or_else(|| chart_default_title(kind));
    let desc = chart_desc(points, config);
    maud::html! {
        div class=(root) {
            svg class="autumn-chart__svg" viewBox=(view_box)
                preserveAspectRatio="none" role="img" aria-label=(label)
                focusable="false" {
                title { (label) }
                desc { (desc) }
                (body)
            }
            @if config.table_fallback {
                (chart_table(points))
            }
        }
    }
}

/// Render a compact, single-series **sparkline** as inline SVG — a small trend
/// line with no axes, ideal inline with text or inside a table cell.
///
/// Shorthand for [`sparkline_with`] with a default [`ChartConfig`]. See that
/// function for axis overrides, an accessible name, and the table fallback.
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::sparkline;
///
/// let html = sparkline(&[("Mon", 3.0), ("Tue", 5.0), ("Wed", 4.0)]).into_string();
/// assert!(html.contains(r#"role="img""#));
/// assert!(html.contains("autumn-chart--sparkline"));
/// assert!(!html.contains("<script"));
/// assert!(!html.contains("style="));
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn sparkline(points: &[(&str, f64)]) -> maud::Markup {
    sparkline_with(points, &ChartConfig::default())
}

/// Render a sparkline with a [`ChartConfig`] (axis override, accessible name,
/// table fallback). See [`sparkline`] for the common case.
#[cfg(feature = "maud")]
#[must_use]
pub fn sparkline_with(points: &[(&str, f64)], config: &ChartConfig<'_>) -> maud::Markup {
    const W: f64 = 100.0;
    const H: f64 = 30.0;
    const PAD: f64 = 2.0;
    let (min, max) = resolve_bounds(points, config);
    let coords = chart_coords(points, W, H, PAD, min, max);
    let body = maud::html! {
        @if coords.len() == 1 {
            circle class="autumn-chart__point" cx=(fmt_coord(coords[0].0))
                cy=(fmt_coord(coords[0].1)) r="1.5" {}
        } @else if coords.len() > 1 {
            polyline class="autumn-chart__line" points=(polyline_points(&coords)) {}
        }
    };
    chart_root("sparkline", "0 0 100 30", points, config, &body)
}

/// Join plotted coordinates into an SVG `points` attribute value
/// (`"x1,y1 x2,y2 …"`). Built as a `String` so raw `f64` values are never
/// interpolated directly into markup.
#[cfg(feature = "maud")]
fn polyline_points(coords: &[(f64, f64)]) -> String {
    let mut s = String::new();
    for (i, (x, y)) in coords.iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        s.push_str(&fmt_coord(*x));
        s.push(',');
        s.push_str(&fmt_coord(*y));
    }
    s
}

/// Render a single-series **bar chart** as inline SVG.
///
/// Shorthand for [`bar_chart_with`] with a default [`ChartConfig`]. Each
/// `(label, value)` becomes one `<rect>`; bar heights auto-scale from the data
/// unless overridden via [`ChartConfig::min`] / [`ChartConfig::max`].
///
/// # Example
///
/// ```rust
/// use autumn_web::widgets::bar_chart;
///
/// let html = bar_chart(&[("Q1", 12.0), ("Q2", 19.0), ("Q3", 7.0)]).into_string();
/// assert!(html.contains("autumn-chart--bar"));
/// assert!(html.contains("<rect"));
/// assert!(!html.contains("style="));
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn bar_chart(series: &[(&str, f64)]) -> maud::Markup {
    bar_chart_with(series, &ChartConfig::default())
}

/// Render a bar chart with a [`ChartConfig`]. See [`bar_chart`] for the common
/// case.
#[cfg(feature = "maud")]
#[must_use]
#[allow(clippy::cast_precision_loss, clippy::suboptimal_flops)]
pub fn bar_chart_with(series: &[(&str, f64)], config: &ChartConfig<'_>) -> maud::Markup {
    const W: f64 = 100.0;
    const H: f64 = 40.0;
    const PAD: f64 = 2.0;
    let (min, max) = resolve_bar_bounds(series, config);
    let usable_h = H - 2.0 * PAD;
    let bottom = H - PAD;
    let span = max - min;
    // Bars diverge from the zero baseline: positive bars grow up from the zero
    // line, negative bars grow down, and each bar's length is proportional to
    // its magnitude. A degenerate span (`<= 0`, only reachable as an all-zero
    // series once automatic bounds are zero-anchored) collapses every bar onto
    // the baseline with zero height.
    let zero_frac = if span > 0.0 {
        ((0.0 - min) / span).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let y_zero = bottom - zero_frac * usable_h;
    let n = series.len();
    let body = maud::html! {
        @if n > 0 {
            @let slot = (W - 2.0 * PAD) / n as f64;
            @let bar_w = slot * 0.7;
            @for (i, (_, v)) in series.iter().enumerate() {
                @let v_frac = if span > 0.0 && v.is_finite() {
                    ((*v - min) / span).clamp(0.0, 1.0)
                } else {
                    zero_frac
                };
                @let y_v = bottom - v_frac * usable_h;
                @let y = y_zero.min(y_v);
                @let bar_h = (y_zero - y_v).abs();
                @let x = PAD + i as f64 * slot + (slot - bar_w) / 2.0;
                rect class="autumn-chart__bar" x=(fmt_coord(x)) y=(fmt_coord(y))
                    width=(fmt_coord(bar_w)) height=(fmt_coord(bar_h)) {}
            }
        }
    };
    chart_root("bar", "0 0 100 40", series, config, &body)
}

/// Render a single-series **line chart** as inline SVG — a trend line with a
/// marker at each data point.
///
/// Shorthand for [`line_chart_with`] with a default [`ChartConfig`]. The axis
/// auto-scales from the data unless overridden.
///
/// # Example
///
/// A 30-point trend series (e.g. daily signups), rendered to accessible,
/// script-free inline SVG:
///
/// ```rust
/// use autumn_web::widgets::line_chart;
///
/// // Build 30 points with a loop — a smooth demo trend.
/// let owned: Vec<(String, f64)> = (0..30)
///     .map(|day| {
///         let label = format!("Day {}", day + 1);
///         let value = (day as f64 * 0.4).sin() * 10.0 + 20.0;
///         (label, value)
///     })
///     .collect();
/// let points: Vec<(&str, f64)> = owned.iter().map(|(l, v)| (l.as_str(), *v)).collect();
///
/// let html = line_chart(&points).into_string();
/// assert!(html.contains(r#"role="img""#));
/// assert!(html.contains("autumn-chart--line"));
/// assert!(html.contains("<polyline"));
/// // Pure inline SVG: no scripts, no inline styles, no external assets.
/// assert!(!html.contains("<script"));
/// assert!(!html.contains("style="));
/// ```
///
/// ## Pairing with a grouped-aggregate query
///
/// Map the `Vec<(K, V)>` a repository grouped-aggregate returns into
/// `(&str, f64)` chart points. `count_*` yields `i64`; `sum_*`/`avg_*` yield an
/// `Option`, so coalesce with `.unwrap_or(0)` before converting:
///
/// ```rust,ignore
/// use autumn_web::aggregate::DateBucket;
/// use autumn_web::widgets::line_chart;
///
/// // A day-bucketed signup time series.
/// let per_day: Vec<(chrono::NaiveDateTime, i64)> = repo
///     .count_grouped_by_created_at()
///     .bucket(DateBucket::Day)
///     .filter_range(window_start, window_end)
///     .load()
///     .await?;
///
/// let labels: Vec<String> =
///     per_day.iter().map(|(day, _)| day.format("%b %d").to_string()).collect();
/// let points: Vec<(&str, f64)> = per_day
///     .iter()
///     .zip(&labels)
///     .map(|((_, count), label)| (label.as_str(), *count as f64))
///     .collect();
/// let svg = line_chart(&points).into_string();
///
/// // For a nullable aggregate (sum/avg), coalesce the Option first:
/// // let revenue: Vec<(chrono::NaiveDateTime, Option<i64>)> = ...;
/// // let v = *value; let point = v.unwrap_or(0) as f64;
/// ```
#[cfg(feature = "maud")]
#[must_use]
pub fn line_chart(series: &[(&str, f64)]) -> maud::Markup {
    line_chart_with(series, &ChartConfig::default())
}

/// Render a line chart with a [`ChartConfig`]. See [`line_chart`] for the
/// common case.
#[cfg(feature = "maud")]
#[must_use]
pub fn line_chart_with(series: &[(&str, f64)], config: &ChartConfig<'_>) -> maud::Markup {
    const W: f64 = 100.0;
    const H: f64 = 40.0;
    const PAD: f64 = 3.0;
    let (min, max) = resolve_bounds(series, config);
    let coords = chart_coords(series, W, H, PAD, min, max);
    let body = maud::html! {
        @if coords.len() > 1 {
            polyline class="autumn-chart__line" points=(polyline_points(&coords)) {}
        }
        @for (x, y) in &coords {
            circle class="autumn-chart__point" cx=(fmt_coord(*x)) cy=(fmt_coord(*y)) r="1.5" {}
        }
    };
    chart_root("line", "0 0 100 40", series, config, &body)
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(all(test, feature = "maud"))]
mod tests {
    use super::*;

    // ── transition_controls ────────────────────────────────────────────

    /// A plain literal transition graph: `draft -> published` (guarded),
    /// `published -> archived` (guard-less). No real model required.
    const fn sample_transitions() -> &'static [(&'static str, &'static str, Option<&'static str>)] {
        &[
            ("draft", "published", Some("can_publish")),
            ("published", "archived", None),
        ]
    }

    #[test]
    fn transition_controls_only_renders_edges_from_current() {
        // current = "draft": only the draft -> published edge should appear.
        let html = transition_controls(
            "/orders/42/transitions/status",
            "status",
            "draft",
            sample_transitions(),
            |_to| true,
            None,
            None,
        )
        .into_string();
        assert!(html.contains(r#"value="published""#), "{html}");
        // The published -> archived edge is from a different state: absent.
        assert!(!html.contains(r#"value="archived""#), "{html}");
    }

    #[test]
    fn transition_controls_renders_terminal_current_as_empty_group() {
        // current = "archived" is terminal: no edges match, empty group only.
        let html = transition_controls(
            "/orders/42/transitions/status",
            "status",
            "archived",
            sample_transitions(),
            |_to| true,
            None,
            None,
        )
        .into_string();
        assert!(
            html.contains(r#"class="autumn-transition-controls""#),
            "{html}"
        );
        assert!(!html.contains("<form"), "{html}");
        assert!(!html.contains("<button"), "{html}");
    }

    #[test]
    fn transition_controls_guard_true_button_is_enabled() {
        let html = transition_controls(
            "/orders/42/transitions/status",
            "status",
            "draft",
            sample_transitions(),
            |to| to == "published", // guard passes for the draft -> published edge
            None,
            None,
        )
        .into_string();
        assert!(
            html.contains("<button type=\"submit\">Mark as published</button>"),
            "{html}"
        );
        assert!(!html.contains("disabled"), "{html}");
    }

    #[test]
    fn transition_controls_guard_false_button_is_disabled() {
        let html = transition_controls(
            "/orders/42/transitions/status",
            "status",
            "draft",
            sample_transitions(),
            |_to| false, // guard fails: legal edge, but currently blocked
            None,
            None,
        )
        .into_string();
        // Still rendered, but disabled.
        assert!(html.contains(r#"value="published""#), "{html}");
        assert!(html.contains("disabled"), "{html}");
    }

    #[test]
    fn transition_controls_guardless_edge_from_current_is_enabled() {
        // current = "published": the guard-less published -> archived edge shows,
        // enabled (its guard slot is None, and `can` returns true).
        let html = transition_controls(
            "/orders/42/transitions/status",
            "status",
            "published",
            sample_transitions(),
            |_to| true,
            None,
            None,
        )
        .into_string();
        assert!(html.contains(r#"value="archived""#), "{html}");
        assert!(
            html.contains("<button type=\"submit\">Mark as archived</button>"),
            "{html}"
        );
        assert!(!html.contains("disabled"), "{html}");
    }

    #[test]
    fn transition_controls_form_is_post_no_js_and_carries_target_in_body() {
        let html = transition_controls(
            "/orders/42/transitions/status",
            "status",
            "draft",
            sample_transitions(),
            |_to| true,
            None,
            None,
        )
        .into_string();
        assert!(
            html.contains(r#"<form method="post" action="/orders/42/transitions/status""#),
            "{html}"
        );
        // Target state carried in the body as a hidden input under the field name.
        assert!(
            html.contains(r#"<input type="hidden" name="status" value="published">"#),
            "{html}"
        );
        // No JavaScript.
        assert!(!html.contains("onclick"), "{html}");
        assert!(!html.contains("hx-"), "{html}");
        assert!(!html.contains("<script"), "{html}");
    }

    #[test]
    fn transition_controls_csrf_hidden_input_present_when_token_supplied() {
        let token = crate::security::CsrfToken::new("secret-token".to_string());
        let html = transition_controls(
            "/orders/42/transitions/status",
            "status",
            "draft",
            sample_transitions(),
            |_to| true,
            Some(&token),
            None,
        )
        .into_string();
        assert!(
            html.contains(r#"<input type="hidden" name="_csrf" value="secret-token">"#),
            "{html}"
        );
    }

    #[test]
    fn transition_controls_csrf_absent_when_no_token() {
        let html = transition_controls(
            "/orders/42/transitions/status",
            "status",
            "draft",
            sample_transitions(),
            |_to| true,
            None,
            None,
        )
        .into_string();
        assert!(!html.contains("_csrf"), "{html}");
    }

    #[test]
    fn transition_controls_honors_custom_csrf_field_name() {
        let token = crate::security::CsrfToken::new("secret-token".to_string());
        let field = crate::security::CsrfFormField("csrf_custom".to_string());
        let html = transition_controls(
            "/orders/42/transitions/status",
            "status",
            "draft",
            sample_transitions(),
            |_to| true,
            Some(&token),
            Some(&field),
        )
        .into_string();
        assert!(
            html.contains(r#"<input type="hidden" name="csrf_custom" value="secret-token">"#),
            "{html}"
        );
        assert!(!html.contains(r#"name="_csrf""#), "{html}");
    }

    // ── reaction_controls CSRF sugar ───────────────────────────────────

    /// The `.csrf(Option<&CsrfToken>, Option<&CsrfFormField>)` sugar is the
    /// path every real handler uses (a `CsrfToken` cannot be constructed
    /// outside this crate, so the integration tests can only exercise the
    /// `.csrf_token(&str)` primitive). reddit-clone's `vote_controls` calls it
    /// with the page's extractor, which is what puts the hidden input in the
    /// post-detail markup and makes the no-JS form POST pass CSRF.
    #[test]
    fn reaction_controls_csrf_sugar_renders_the_hidden_input_in_every_form() {
        let token = crate::security::CsrfToken::new("secret-token".to_string());
        let html = reaction_controls(
            &ReactionControls::votes("votes-42", "/posts/42/upvote", "/posts/42/downvote")
                .aggregate(7)
                .csrf(Some(&token), None),
        )
        .into_string();
        assert_eq!(
            html.matches(r#"<input type="hidden" name="_csrf" value="secret-token">"#)
                .count(),
            2,
            "{html}"
        );

        // `None` is the SSE-fragment case: htmx-only, no hidden input.
        let anonymous = reaction_controls(
            &ReactionControls::votes("votes-42", "/posts/42/upvote", "/posts/42/downvote")
                .aggregate(7)
                .csrf(None, None),
        )
        .into_string();
        assert!(!anonymous.contains("_csrf"), "{anonymous}");
    }

    #[test]
    fn reaction_controls_csrf_sugar_honors_a_custom_field_name() {
        let token = crate::security::CsrfToken::new("secret-token".to_string());
        let field = crate::security::CsrfFormField("csrf_custom".to_string());
        let html = reaction_controls(
            &ReactionControls::likes("likes-42", "/posts/42/like").csrf(Some(&token), Some(&field)),
        )
        .into_string();
        assert!(
            html.contains(r#"<input type="hidden" name="csrf_custom" value="secret-token">"#),
            "{html}"
        );
        assert!(!html.contains(r#"name="_csrf""#), "{html}");
    }

    // ── build_trigger ──────────────────────────────────────────────────

    #[test]
    fn trigger_has_debounce() {
        let t = build_trigger(300, false);
        assert!(t.contains("delay:300ms"), "{t}");
    }

    #[test]
    fn trigger_has_changed_modifier() {
        let t = build_trigger(300, false);
        assert!(t.contains("changed"), "{t}");
    }

    #[test]
    fn trigger_has_no_filter_expressions() {
        // No [condition] filters are emitted — min_length is server-side only.
        // This ensures the trigger works under Autumn's default CSP (no unsafe-eval).
        let t = build_trigger(300, false);
        assert!(!t.contains("this.value.length"), "{t}");
        assert!(!t.contains('['), "{t}");
    }

    #[test]
    fn trigger_initial_load_appends_load() {
        let t = build_trigger(300, true);
        assert!(t.contains(", load"), "{t}");
    }

    #[test]
    fn trigger_no_initial_load_by_default() {
        let t = build_trigger(300, false);
        assert!(!t.contains("load"), "{t}");
    }

    #[test]
    fn trigger_custom_debounce() {
        let t = build_trigger(750, false);
        assert!(t.contains("delay:750ms"), "{t}");
    }

    // ── selector_to_id ─────────────────────────────────────────────────

    #[test]
    fn selector_to_id_strips_hash() {
        assert_eq!(selector_to_id("#my-results"), "my-results");
    }

    #[test]
    fn selector_to_id_passthrough_without_hash() {
        assert_eq!(selector_to_id("results"), "results");
    }

    // ── ActiveSearchConfig builder ─────────────────────────────────────

    #[test]
    fn config_defaults() {
        let c = ActiveSearchConfig::new("/s", "#r");
        assert_eq!(c.method, SearchMethod::Get);
        assert_eq!(c.debounce_ms, 300);
        assert_eq!(c.min_length, 1);
        assert_eq!(c.param_name, "q");
        assert!(!c.initial_load);
        assert!(c.indicator.is_none());
        assert!(c.placeholder.is_none());
    }

    #[test]
    fn config_post_builder() {
        assert_eq!(
            ActiveSearchConfig::new("/s", "#r").post().method,
            SearchMethod::Post
        );
    }

    #[test]
    fn config_debounce_builder() {
        assert_eq!(
            ActiveSearchConfig::new("/s", "#r")
                .debounce(500)
                .debounce_ms,
            500
        );
    }

    #[test]
    fn config_min_length_builder() {
        assert_eq!(
            ActiveSearchConfig::new("/s", "#r").min_length(3).min_length,
            3
        );
    }

    #[test]
    fn config_initial_load_builder() {
        assert!(
            ActiveSearchConfig::new("/s", "#r")
                .initial_load()
                .initial_load
        );
    }

    #[test]
    fn config_indicator_builder() {
        assert_eq!(
            ActiveSearchConfig::new("/s", "#r")
                .indicator("#spin")
                .indicator,
            Some("#spin")
        );
    }

    #[test]
    fn config_placeholder_builder() {
        assert_eq!(
            ActiveSearchConfig::new("/s", "#r")
                .placeholder("hint")
                .placeholder,
            Some("hint")
        );
    }

    #[test]
    fn config_param_name_builder() {
        assert_eq!(
            ActiveSearchConfig::new("/s", "#r")
                .param_name("query")
                .param_name,
            "query"
        );
    }

    // ── active_search_input ────────────────────────────────────────────

    #[test]
    fn input_defaults_to_hx_get() {
        let config = ActiveSearchConfig::new("/search", "#results");
        let html = active_search_input("q", "Search", &config).into_string();
        assert!(html.contains(r#"hx-get="/search""#), "{html}");
        assert!(!html.contains("hx-post"), "{html}");
    }

    #[test]
    fn input_uses_hx_post_when_configured() {
        let config = ActiveSearchConfig::new("/search", "#results").post();
        let html = active_search_input("q", "Search", &config).into_string();
        assert!(html.contains(r#"hx-post="/search""#), "{html}");
        assert!(!html.contains("hx-get"), "{html}");
    }

    #[test]
    fn input_trigger_has_default_debounce() {
        let config = ActiveSearchConfig::new("/search", "#results");
        let html = active_search_input("q", "Search", &config).into_string();
        assert!(html.contains("delay:300ms"), "{html}");
    }

    #[test]
    fn input_configurable_debounce() {
        let config = ActiveSearchConfig::new("/search", "#results").debounce(500);
        let html = active_search_input("q", "Search", &config).into_string();
        assert!(html.contains("delay:500ms"), "{html}");
    }

    #[test]
    fn input_configurable_min_length() {
        let config = ActiveSearchConfig::new("/search", "#results").min_length(3);
        let html = active_search_input("q", "Search", &config).into_string();
        // min_length is enforced server-side; no filter expression in the trigger
        assert!(html.contains("hx-trigger"), "{html}");
        assert!(!html.contains("this.value.length"), "{html}");
    }

    #[test]
    fn input_no_filter_when_min_length_zero() {
        let config = ActiveSearchConfig::new("/search", "#results").min_length(0);
        let html = active_search_input("q", "Search", &config).into_string();
        assert!(!html.contains("this.value.length"), "{html}");
    }

    #[test]
    fn input_initial_load_in_trigger() {
        let config = ActiveSearchConfig::new("/search", "#results").initial_load();
        let html = active_search_input("q", "Search", &config).into_string();
        assert!(html.contains(", load"), "{html}");
    }

    #[test]
    fn input_no_initial_load_by_default() {
        let config = ActiveSearchConfig::new("/search", "#results");
        let html = active_search_input("q", "Search", &config).into_string();
        assert!(!html.contains(", load"), "{html}");
    }

    #[test]
    fn input_target_selector() {
        let config = ActiveSearchConfig::new("/search", "#my-results");
        let html = active_search_input("q", "Search", &config).into_string();
        assert!(html.contains("hx-target=\"#my-results\""), "{html}");
    }

    #[test]
    fn input_indicator_when_configured() {
        let config = ActiveSearchConfig::new("/search", "#results").indicator("#spinner");
        let html = active_search_input("q", "Search", &config).into_string();
        assert!(html.contains("hx-indicator=\"#spinner\""), "{html}");
    }

    #[test]
    fn input_no_indicator_by_default() {
        let config = ActiveSearchConfig::new("/search", "#results");
        let html = active_search_input("q", "Search", &config).into_string();
        assert!(!html.contains("hx-indicator"), "{html}");
    }

    #[test]
    fn input_renders_label() {
        let config = ActiveSearchConfig::new("/search", "#results");
        let html = active_search_input("q", "Search Posts", &config).into_string();
        assert!(html.contains("Search Posts"), "{html}");
        assert!(html.contains("<label"), "{html}");
    }

    #[test]
    fn input_label_for_matches_id() {
        let config = ActiveSearchConfig::new("/search", "#results");
        let html = active_search_input("my-search", "Search", &config).into_string();
        assert!(html.contains(r#"for="my-search""#), "{html}");
        assert!(html.contains(r#"id="my-search""#), "{html}");
    }

    #[test]
    fn input_has_aria_controls() {
        let config = ActiveSearchConfig::new("/search", "#my-results");
        let html = active_search_input("q", "Search", &config).into_string();
        assert!(html.contains("aria-controls"), "{html}");
        assert!(html.contains("my-results"), "{html}");
    }

    #[test]
    fn input_type_is_search() {
        let config = ActiveSearchConfig::new("/search", "#results");
        let html = active_search_input("q", "Search", &config).into_string();
        assert!(html.contains(r#"type="search""#), "{html}");
    }

    #[test]
    fn input_placeholder_when_configured() {
        let config = ActiveSearchConfig::new("/search", "#results").placeholder("Type to search…");
        let html = active_search_input("q", "Search", &config).into_string();
        assert!(html.contains("Type to search"), "{html}");
    }

    #[test]
    fn input_no_placeholder_by_default() {
        let config = ActiveSearchConfig::new("/search", "#results");
        let html = active_search_input("q", "Search", &config).into_string();
        assert!(!html.contains("placeholder"), "{html}");
    }

    #[test]
    fn input_custom_param_name() {
        let config = ActiveSearchConfig::new("/search", "#results").param_name("query");
        let html = active_search_input("q", "Search", &config).into_string();
        assert!(html.contains(r#"name="query""#), "{html}");
    }

    // ── active_search_results ──────────────────────────────────────────

    #[test]
    fn results_correct_id() {
        let html = active_search_results("my-results").into_string();
        assert!(html.contains(r#"id="my-results""#), "{html}");
    }

    #[test]
    fn results_role_status() {
        let html = active_search_results("r").into_string();
        assert!(html.contains(r#"role="status""#), "{html}");
    }

    #[test]
    fn results_aria_live_polite() {
        let html = active_search_results("r").into_string();
        assert!(html.contains(r#"aria-live="polite""#), "{html}");
    }

    #[test]
    fn results_aria_atomic() {
        let html = active_search_results("r").into_string();
        assert!(html.contains(r#"aria-atomic="true""#), "{html}");
    }

    // ── active_search (full widget) ────────────────────────────────────

    #[test]
    fn widget_includes_input_and_results() {
        let config = ActiveSearchConfig::new("/search", "#s-results");
        let html = active_search("s", "Search", &config).into_string();
        assert!(html.contains(r#"type="search""#), "{html}");
        assert!(html.contains(r#"id="s-results""#), "{html}");
    }

    #[test]
    fn widget_results_id_matches_target_selector() {
        // Callers pass any target; the generated container must match.
        let config = ActiveSearchConfig::new("/search", "#custom-results");
        let html = active_search("search-widget", "Search", &config).into_string();
        assert!(html.contains(r#"id="custom-results""#), "{html}");
    }

    #[test]
    fn widget_has_noscript_fallback() {
        let config = ActiveSearchConfig::new("/search", "#results");
        let html = active_search("s", "Search", &config).into_string();
        assert!(html.contains("<noscript>"), "{html}");
        assert!(html.contains("<form"), "{html}");
        assert!(html.contains(r#"type="submit""#), "{html}");
    }

    #[test]
    fn widget_noscript_get_by_default() {
        let config = ActiveSearchConfig::new("/search", "#results");
        let html = active_search("s", "Search", &config).into_string();
        assert!(html.contains(r#"method="get""#), "{html}");
    }

    #[test]
    fn widget_noscript_post_when_configured() {
        let config = ActiveSearchConfig::new("/search", "#results").post();
        let html = active_search("s", "Search", &config).into_string();
        assert!(html.contains(r#"method="post""#), "{html}");
    }

    // ── autocomplete_input ─────────────────────────────────────────────

    #[test]
    fn autocomplete_visible_search_input() {
        let config = AutocompleteConfig::new("/ac", "value_field");
        let html = autocomplete_input("x", "Label", &config).into_string();
        assert!(html.contains(r#"type="search""#), "{html}");
        // visible input uses query_param (default "q") so htmx sends ?q=...
        assert!(html.contains(r#"name="q""#), "{html}");
    }

    #[test]
    fn autocomplete_visible_input_uses_query_param() {
        let config = AutocompleteConfig::new("/ac", "value_field").query_param("search");
        let html = autocomplete_input("x", "Label", &config).into_string();
        assert!(html.contains(r#"name="search""#), "{html}");
    }

    #[test]
    fn autocomplete_hidden_value_field() {
        let config = AutocompleteConfig::new("/ac", "value_field");
        let html = autocomplete_input("x", "Label", &config).into_string();
        assert!(html.contains(r#"type="hidden""#), "{html}");
        // The hidden input has no name in HTML; name is set by JS on first interaction
        // so no-JS forms don't see a duplicate field alongside the noscript <select>.
        assert!(html.contains(r#"id="x-value""#), "{html}");
    }

    #[test]
    fn autocomplete_hidden_field_empty_initial_value() {
        let config = AutocompleteConfig::new("/ac", "value_field");
        let html = autocomplete_input("x", "Label", &config).into_string();
        assert!(html.contains(r#"type="hidden""#), "{html}");
        assert!(html.contains(r#"value="""#), "{html}");
    }

    #[test]
    fn autocomplete_listbox_container() {
        let config = AutocompleteConfig::new("/ac", "value_field");
        let html = autocomplete_input("x", "Label", &config).into_string();
        assert!(html.contains(r#"role="listbox""#), "{html}");
    }

    #[test]
    fn autocomplete_wrapper_has_data_attributes_for_runtime() {
        let config = AutocompleteConfig::new("/ac", "value_field");
        let html = autocomplete_input("x", "Label", &config).into_string();
        // The external autumn-widgets.js reads these to wire up interactions.
        assert!(html.contains(r#"data-ac-value-id="x-value""#), "{html}");
        assert!(
            html.contains(r#"data-ac-value-name="value_field""#),
            "{html}"
        );
        assert!(html.contains("data-ac-query"), "{html}");
        assert!(html.contains("data-ac-min-length"), "{html}");
    }

    #[test]
    fn autocomplete_free_text_mode_sets_data_attribute() {
        let config = AutocompleteConfig::new("/ac", "value_field").free_text();
        let html = autocomplete_input("x", "Label", &config).into_string();
        assert!(html.contains("data-ac-free-text"), "{html}");
    }

    #[test]
    fn autocomplete_id_mode_no_free_text_attribute() {
        let config = AutocompleteConfig::new("/ac", "value_field");
        let html = autocomplete_input("x", "Label", &config).into_string();
        assert!(!html.contains("data-ac-free-text"), "{html}");
    }

    #[test]
    fn autocomplete_combobox_role() {
        let config = AutocompleteConfig::new("/ac", "value_field");
        let html = autocomplete_input("x", "Label", &config).into_string();
        assert!(html.contains(r#"role="combobox""#), "{html}");
    }

    #[test]
    fn autocomplete_aria_expanded_false() {
        let config = AutocompleteConfig::new("/ac", "value_field");
        let html = autocomplete_input("x", "Label", &config).into_string();
        assert!(html.contains(r#"aria-expanded="false""#), "{html}");
    }

    #[test]
    fn autocomplete_aria_autocomplete_list() {
        let config = AutocompleteConfig::new("/ac", "value_field");
        let html = autocomplete_input("x", "Label", &config).into_string();
        assert!(html.contains(r#"aria-autocomplete="list""#), "{html}");
    }

    #[test]
    fn autocomplete_has_aria_controls() {
        let config = AutocompleteConfig::new("/ac", "value_field");
        let html = autocomplete_input("x", "Label", &config).into_string();
        assert!(html.contains("aria-controls"), "{html}");
    }

    #[test]
    fn autocomplete_renders_label() {
        let config = AutocompleteConfig::new("/ac", "value_field");
        let html = autocomplete_input("x", "My Label", &config).into_string();
        assert!(html.contains("My Label"), "{html}");
        assert!(html.contains("<label"), "{html}");
    }

    #[test]
    fn autocomplete_label_for_matches_query_input_id() {
        let config = AutocompleteConfig::new("/ac", "value_field");
        let html = autocomplete_input("tag", "Tag", &config).into_string();
        assert!(html.contains(r#"for="tag-query""#), "{html}");
        assert!(html.contains(r#"id="tag-query""#), "{html}");
    }

    #[test]
    fn autocomplete_has_noscript_fallback() {
        let config = AutocompleteConfig::new("/ac", "value_field");
        let html = autocomplete_input("x", "Label", &config).into_string();
        assert!(html.contains("<noscript>"), "{html}");
        assert!(html.contains("<select"), "{html}");
    }

    #[test]
    fn autocomplete_noscript_select_uses_value_name() {
        let config = AutocompleteConfig::new("/ac", "value_field");
        let html = autocomplete_input("x", "Label", &config).into_string();
        assert!(html.contains(r#"name="value_field""#), "{html}");
    }

    #[test]
    fn autocomplete_fallback_options_rendered_in_noscript() {
        let opts: &[(&str, &str)] = &[("1", "Alpha"), ("2", "Beta")];
        let config = AutocompleteConfig::new("/ac", "value_field").fallback_options(opts);
        let html = autocomplete_input("x", "Label", &config).into_string();
        assert!(html.contains("Alpha"), "{html}");
        assert!(html.contains("Beta"), "{html}");
        assert!(html.contains(r#"value="1""#), "{html}");
        assert!(html.contains(r#"value="2""#), "{html}");
    }

    #[test]
    fn autocomplete_noscript_select_marks_initial_value_selected() {
        // ID-mode (default, non-free-text): a prior selection seeded via
        // `initial_value` must survive a no-JS resubmission too, not just the
        // JS-driven hidden-input path.
        let opts: &[(&str, &str)] = &[("1", "Alpha"), ("2", "Beta")];
        let config = AutocompleteConfig::new("/ac", "value_field")
            .fallback_options(opts)
            .initial_value("2");
        let html = autocomplete_input("x", "Label", &config).into_string();
        assert!(
            html.contains(r#"value="2" selected"#),
            "the option matching initial_value must be marked selected: {html}"
        );
        assert!(
            !html.contains(r#"value="1" selected"#),
            "only the matching option should be selected: {html}"
        );
    }

    #[test]
    fn autocomplete_has_hx_get() {
        let config = AutocompleteConfig::new("/ac", "value_field");
        let html = autocomplete_input("x", "Label", &config).into_string();
        assert!(html.contains(r#"hx-get="/ac""#), "{html}");
    }

    #[test]
    fn autocomplete_hx_trigger_has_debounce() {
        let config = AutocompleteConfig::new("/ac", "value_field");
        let html = autocomplete_input("x", "Label", &config).into_string();
        assert!(html.contains("hx-trigger"), "{html}");
        assert!(html.contains("delay:300ms"), "{html}");
    }

    #[test]
    fn autocomplete_configurable_debounce() {
        let config = AutocompleteConfig::new("/ac", "value_field").debounce(600);
        let html = autocomplete_input("x", "Label", &config).into_string();
        assert!(html.contains("delay:600ms"), "{html}");
    }

    #[test]
    fn autocomplete_configurable_min_length() {
        let config = AutocompleteConfig::new("/ac", "value_field").min_length(2);
        let html = autocomplete_input("x", "Label", &config).into_string();
        // min_length is carried as a data attribute for the autumn-widgets.js runtime
        // to enforce client-side (via htmx:configRequest) and server-side.
        assert!(html.contains(r#"data-ac-min-length="2""#), "{html}");
        assert!(!html.contains("this.value.length"), "{html}");
    }

    #[test]
    fn autocomplete_indicator_when_configured() {
        let config = AutocompleteConfig::new("/ac", "value_field").indicator("#ld");
        let html = autocomplete_input("x", "Label", &config).into_string();
        assert!(html.contains("hx-indicator=\"#ld\""), "{html}");
    }

    #[test]
    fn autocomplete_no_indicator_by_default() {
        let config = AutocompleteConfig::new("/ac", "value_field");
        let html = autocomplete_input("x", "Label", &config).into_string();
        assert!(!html.contains("hx-indicator"), "{html}");
    }

    #[test]
    fn autocomplete_listbox_has_aria_live() {
        let config = AutocompleteConfig::new("/ac", "value_field");
        let html = autocomplete_input("x", "Label", &config).into_string();
        assert!(html.contains("aria-live"), "{html}");
    }

    #[test]
    fn autocomplete_listbox_has_no_inline_handlers() {
        // All interaction is wired by autumn-widgets.js, not inline hx-on:* attributes.
        let config = AutocompleteConfig::new("/ac", "value_field");
        let html = autocomplete_input("x", "Label", &config).into_string();
        assert!(!html.contains("hx-on:keydown"), "{html}");
        assert!(!html.contains("hx-on:click"), "{html}");
        assert!(!html.contains("hx-on:input"), "{html}");
        assert!(!html.contains("oninput"), "{html}");
    }

    // ── autocomplete_option ────────────────────────────────────────────

    #[test]
    fn option_renders_label_and_value() {
        let html = autocomplete_option("42", "My Tag").into_string();
        assert!(html.contains("My Tag"), "{html}");
        assert!(html.contains(r#"data-value="42""#), "{html}");
    }

    #[test]
    fn option_has_role_option() {
        let html = autocomplete_option("1", "Option").into_string();
        assert!(html.contains(r#"role="option""#), "{html}");
    }

    #[test]
    fn option_is_keyboard_focusable() {
        let html = autocomplete_option("1", "Option").into_string();
        assert!(html.contains("tabindex"), "{html}");
    }

    // ── autocomplete_empty_state ───────────────────────────────────────

    #[test]
    fn ac_empty_state_renders_message() {
        let html = autocomplete_empty_state("No results found").into_string();
        assert!(html.contains("No results found"), "{html}");
    }

    #[test]
    fn ac_empty_state_announced_to_screen_readers() {
        let html = autocomplete_empty_state("No results").into_string();
        assert!(
            html.contains(r#"role="status""#) || html.contains("aria-live"),
            "{html}"
        );
    }

    // ── active_search_empty_state ──────────────────────────────────────

    #[test]
    fn search_empty_state_renders_message() {
        let html = active_search_empty_state("No matching posts").into_string();
        assert!(html.contains("No matching posts"), "{html}");
    }

    #[test]
    fn search_empty_state_announced_to_screen_readers() {
        let html = active_search_empty_state("Nothing found").into_string();
        assert!(
            html.contains(r#"role="status""#) || html.contains("aria-live"),
            "{html}"
        );
    }

    // ── breadcrumb ─────────────────────────────────────────────────────

    #[test]
    fn breadcrumb_empty_slice_renders_nothing_no_panic() {
        let html = breadcrumb(&[]).into_string();
        assert!(html.is_empty(), "expected empty output, got: {html}");
    }

    #[test]
    fn breadcrumb_wrapped_in_nav_with_aria_label() {
        let crumbs = [Crumb::link("Home", "/"), Crumb::current("Posts")];
        let html = breadcrumb(&crumbs).into_string();
        assert!(html.contains("<nav"), "{html}");
        assert!(html.contains("aria-label"), "{html}");
    }

    #[test]
    fn breadcrumb_contains_ordered_list() {
        let crumbs = [Crumb::link("Home", "/"), Crumb::current("Posts")];
        let html = breadcrumb(&crumbs).into_string();
        assert!(html.contains("<ol"), "{html}");
        assert!(html.contains("</ol>"), "{html}");
    }

    #[test]
    fn breadcrumb_last_crumb_has_aria_current_page() {
        let crumbs = [
            Crumb::link("Home", "/"),
            Crumb::link("Posts", "/posts"),
            Crumb::current("My Post"),
        ];
        let html = breadcrumb(&crumbs).into_string();
        assert!(html.contains(r#"aria-current="page""#), "{html}");
    }

    #[test]
    fn breadcrumb_preceding_crumbs_render_as_links() {
        let crumbs = [
            Crumb::link("Home", "/"),
            Crumb::link("Posts", "/posts"),
            Crumb::current("My Post"),
        ];
        let html = breadcrumb(&crumbs).into_string();
        assert!(html.contains(r#"href="/""#), "{html}");
        assert!(html.contains(r#"href="/posts""#), "{html}");
    }

    #[test]
    fn breadcrumb_last_crumb_is_not_a_link() {
        let crumbs = [Crumb::link("Home", "/"), Crumb::current("Current Page")];
        let html = breadcrumb(&crumbs).into_string();
        // "Current Page" must not be wrapped in an <a>; it carries aria-current="page" instead.
        assert!(html.contains("Current Page"), "{html}");
        // The current page span must not itself be a link.
        assert!(!html.contains("<a href=\"\">Current Page"), "{html}");
        assert!(html.contains("aria-current=\"page\""), "{html}");
        assert!(html.contains("autumn-breadcrumb__current"), "{html}");
    }

    #[test]
    fn breadcrumb_single_crumb_no_leading_separator() {
        let crumbs = [Crumb::current("Only Page")];
        let html = breadcrumb(&crumbs).into_string();
        // No separator before the first (and only) item
        assert!(!html.contains("aria-hidden"), "{html}");
        assert!(html.contains("Only Page"), "{html}");
    }

    #[test]
    fn breadcrumb_single_crumb_has_aria_current_page() {
        let crumbs = [Crumb::current("Only Page")];
        let html = breadcrumb(&crumbs).into_string();
        assert!(html.contains(r#"aria-current="page""#), "{html}");
    }

    #[test]
    fn breadcrumb_separators_are_aria_hidden() {
        let crumbs = [
            Crumb::link("Home", "/"),
            Crumb::link("Posts", "/posts"),
            Crumb::current("Current"),
        ];
        let html = breadcrumb(&crumbs).into_string();
        // Every separator must carry aria-hidden="true"
        assert!(html.contains(r#"aria-hidden="true""#), "{html}");
    }

    #[test]
    fn breadcrumb_label_containing_html_is_escaped() {
        let crumbs = [
            Crumb::link("<script>alert(1)</script>", "/"),
            Crumb::current("Safe"),
        ];
        let html = breadcrumb(&crumbs).into_string();
        assert!(!html.contains("<script>"), "{html}");
        assert!(html.contains("&lt;script&gt;"), "{html}");
    }

    #[test]
    fn breadcrumb_href_containing_html_is_escaped() {
        let crumbs = [
            Crumb::link("Home", r#"/" onerror="bad"#),
            Crumb::current("Page"),
        ];
        let html = breadcrumb(&crumbs).into_string();
        assert!(!html.contains("onerror=\"bad"), "{html}");
    }

    #[test]
    fn breadcrumb_crumb_link_constructor() {
        let c = Crumb::link("Posts", "/posts");
        assert_eq!(c.label, "Posts");
        assert_eq!(c.href, Some("/posts"));
    }

    #[test]
    fn breadcrumb_crumb_current_constructor() {
        let c = Crumb::current("My Page");
        assert_eq!(c.label, "My Page");
        assert!(c.href.is_none());
    }

    #[test]
    fn breadcrumb_non_last_crumb_without_href_renders_span_not_bare_text() {
        // A Crumb with href:None in a non-last position must still be wrapped
        // in a <span> so it has an accessible element, not invisible bare text.
        let crumbs = [Crumb::current("Unlinked Middle"), Crumb::current("Current")];
        let html = breadcrumb(&crumbs).into_string();
        assert!(
            html.contains("<span class=\"autumn-breadcrumb__text\">Unlinked Middle</span>"),
            "{html}"
        );
        // Only the last item carries aria-current
        assert_eq!(html.matches(r#"aria-current="page""#).count(), 1, "{html}");
    }

    // ── nav_link ──────────────────────────────────────────────────────────

    #[test]
    fn nav_link_exact_match_is_active() {
        let html = nav_link("/admin/posts", "/admin/posts", "Posts").into_string();
        assert!(html.contains(r#"class="active""#), "{html}");
        assert!(html.contains(r#"aria-current="page""#), "{html}");
    }

    #[test]
    fn nav_link_no_match_is_inactive() {
        let html = nav_link("/admin/jobs", "/admin/posts", "Posts").into_string();
        assert!(!html.contains("active"), "{html}");
        assert!(!html.contains("aria-current"), "{html}");
    }

    #[test]
    fn nav_link_exact_mode_rejects_sub_paths() {
        let html = nav_link("/admin/posts/3", "/admin/posts", "Posts").into_string();
        assert!(!html.contains("active"), "{html}");
        assert!(!html.contains("aria-current"), "{html}");
    }

    #[test]
    fn nav_link_prefix_match_is_active() {
        let html = nav_link_matched(
            "/admin/posts/3/edit",
            "/admin/posts",
            "Posts",
            NavLinkMatch::Prefix,
        )
        .into_string();
        assert!(html.contains(r#"class="active""#), "{html}");
        assert!(html.contains(r#"aria-current="page""#), "{html}");
    }

    #[test]
    fn nav_link_prefix_mode_rejects_false_positive() {
        // "/admin/postsX" must NOT match "/admin/posts" as a prefix.
        let html = nav_link_matched(
            "/admin/postsX",
            "/admin/posts",
            "Posts",
            NavLinkMatch::Prefix,
        )
        .into_string();
        assert!(!html.contains("active"), "{html}");
        assert!(!html.contains("aria-current"), "{html}");
    }

    #[test]
    fn nav_link_prefix_mode_exact_path_is_active() {
        let html = nav_link_matched(
            "/admin/posts",
            "/admin/posts",
            "Posts",
            NavLinkMatch::Prefix,
        )
        .into_string();
        assert!(html.contains(r#"class="active""#), "{html}");
    }

    #[test]
    fn nav_link_prefix_mode_trailing_slash_href_still_matches() {
        // href with a trailing slash must behave the same as without one.
        let html = nav_link_matched(
            "/admin/posts/3/edit",
            "/admin/posts/",
            "Posts",
            NavLinkMatch::Prefix,
        )
        .into_string();
        assert!(html.contains(r#"class="active""#), "{html}");
    }

    #[test]
    fn nav_link_prefix_mode_empty_href_never_matches() {
        // An empty href must never act as a wildcard that matches every path.
        let html = nav_link_matched("/admin/posts/3/edit", "", "Posts", NavLinkMatch::Prefix)
            .into_string();
        assert!(!html.contains("active"), "{html}");
        assert!(!html.contains("aria-current"), "{html}");
    }

    #[test]
    fn nav_link_prefix_mode_root_href_does_not_match_every_path() {
        // href="/" must not wildcard-match every path either; only an exact "/"
        // current_path is active.
        let html =
            nav_link_matched("/admin/posts", "/", "Home", NavLinkMatch::Prefix).into_string();
        assert!(!html.contains("active"), "{html}");

        let html = nav_link_matched("/", "/", "Home", NavLinkMatch::Prefix).into_string();
        assert!(html.contains(r#"class="active""#), "{html}");
    }

    #[test]
    fn nav_link_renders_label_and_href() {
        let html = nav_link("/somewhere/else", "/admin/posts", "Posts").into_string();
        assert!(html.contains(r#"href="/admin/posts""#), "{html}");
        assert!(html.contains(">Posts<"), "{html}");
    }

    // ── locale switcher (issue #1251) ─────────────────────────────────────

    #[test]
    fn localized_path_prepends_locale_segment() {
        assert_eq!(localized_path("/posts", "es"), "/es/posts");
    }

    #[test]
    fn localized_path_preserves_query_string() {
        assert_eq!(
            localized_path("/posts?sort=asc&page=2", "es"),
            "/es/posts?sort=asc&page=2"
        );
    }

    #[test]
    fn localized_path_handles_root() {
        // No trailing slash: axum's `nest("/es", router)` makes bare "/es"
        // match the inner router's "/" route, while "/es/" 404s.
        assert_eq!(localized_path("/", "es"), "/es");
    }

    #[test]
    fn localized_path_handles_root_with_query() {
        assert_eq!(
            localized_path("/?ref=newsletter", "es"),
            "/es?ref=newsletter"
        );
    }

    #[test]
    fn locale_switcher_links_every_locale_except_current() {
        let supported = vec!["en".to_owned(), "es".to_owned(), "fr".to_owned()];
        let html = locale_switcher("/posts", "es", &supported).into_string();
        assert!(html.contains(r#"href="/en/posts""#), "{html}");
        assert!(html.contains(r#"href="/fr/posts""#), "{html}");
        assert!(
            !html.contains(r#"href="/es/posts""#),
            "current locale must not self-link: {html}"
        );
        assert!(html.contains(r#"aria-current="true""#), "{html}");
    }

    #[test]
    fn locale_switcher_preserves_query_string_in_every_link() {
        let supported = vec!["en".to_owned(), "es".to_owned()];
        let html = locale_switcher("/posts?sort=asc", "en", &supported).into_string();
        assert!(html.contains(r#"href="/es/posts?sort=asc""#), "{html}");
    }

    // ── nav_bar ──────────────────────────────────────────────────────────

    #[test]
    fn nav_bar_renders_single_nav_landmark_with_default_label() {
        let config = NavBarConfig::new();
        let html = nav_bar("/", &config).into_string();
        assert_eq!(html.matches("<nav").count(), 1, "{html}");
        assert!(html.contains(r#"aria-label="Main""#), "{html}");
        assert!(html.contains(r#"class="autumn-nav""#), "{html}");
        assert!(html.contains("data-autumn-nav"), "{html}");
    }

    #[test]
    fn nav_bar_custom_aria_label() {
        let config = NavBarConfig::new().aria_label("Admin navigation");
        let html = nav_bar("/", &config).into_string();
        assert!(html.contains(r#"aria-label="Admin navigation""#), "{html}");
    }

    #[test]
    fn nav_bar_brand_text_renders_brand_link() {
        let config = NavBarConfig::new().brand("Acme", "/");
        let html = nav_bar("/", &config).into_string();
        assert!(
            html.contains(r#"<a class="autumn-nav__brand" href="/">Acme</a>"#),
            "{html}"
        );
    }

    #[test]
    fn nav_bar_brand_html_without_href_renders_span() {
        let config = NavBarConfig::new().brand_html(maud::html! { "Acme" }, None);
        let html = nav_bar("/", &config).into_string();
        assert!(
            html.contains(r#"<span class="autumn-nav__brand">Acme</span>"#),
            "{html}"
        );
        assert!(!html.contains("<a class=\"autumn-nav__brand\""), "{html}");
    }

    #[test]
    fn nav_bar_without_brand_omits_brand_element() {
        let config = NavBarConfig::new();
        let html = nav_bar("/", &config).into_string();
        assert!(!html.contains("autumn-nav__brand"), "{html}");
    }

    #[test]
    fn nav_bar_active_link_gets_aria_current() {
        let config = NavBarConfig::new().item(NavItem::link("/posts", "Posts"));
        let html = nav_bar("/posts", &config).into_string();
        assert_eq!(html.matches(r#"aria-current="page""#).count(), 1, "{html}");
        assert!(html.contains(r#"class="active""#), "{html}");
    }

    #[test]
    fn nav_bar_prefix_match_mode_activates_on_subpath() {
        let config = NavBarConfig::new().item(NavItem::link_matched(
            "/posts",
            "Posts",
            NavLinkMatch::Prefix,
        ));
        let html = nav_bar("/posts/3/edit", &config).into_string();
        assert!(html.contains(r#"aria-current="page""#), "{html}");
    }

    #[test]
    fn nav_bar_plain_link_never_active() {
        let config = NavBarConfig::new().item(NavItem::plain_link("/actuator/ui", "Actuator"));
        let html = nav_bar("/actuator/ui", &config).into_string();
        assert!(!html.contains("aria-current"), "{html}");
        assert!(!html.contains(r#"class="active""#), "{html}");
        assert!(html.contains(r#"href="/actuator/ui""#), "{html}");
    }

    #[test]
    fn nav_bar_section_renders_label_li() {
        let config = NavBarConfig::new().item(NavItem::section("System"));
        let html = nav_bar("/", &config).into_string();
        assert!(
            html.contains(r#"<li class="autumn-nav__section">System</li>"#),
            "{html}"
        );
    }

    #[test]
    fn nav_bar_menu_renders_button_controlling_ul() {
        let config = NavBarConfig::new().item(NavItem::menu(
            NavMenu::new("Products").link("/widgets", "Widgets"),
        ));
        let html = nav_bar("/", &config).into_string();
        assert!(html.contains(r#"type="button""#), "{html}");
        assert!(html.contains("data-nav-menu-toggle"), "{html}");
        assert!(html.contains(r#"aria-expanded="true""#), "{html}");
        assert!(
            html.contains(r#"aria-controls="autumn-nav-menu-1""#),
            "{html}"
        );
        assert!(
            html.contains(r#"<ul id="autumn-nav-menu-1" class="autumn-nav__menu""#),
            "{html}"
        );
    }

    #[test]
    fn nav_bar_menu_sub_link_active_gets_aria_current() {
        let config = NavBarConfig::new().item(NavItem::menu(
            NavMenu::new("Products").link("/widgets", "Widgets"),
        ));
        let html = nav_bar("/widgets", &config).into_string();
        assert!(html.contains(r#"aria-current="page""#), "{html}");
    }

    #[test]
    fn nav_bar_menu_ids_unique_and_respect_custom_id() {
        let config = NavBarConfig::new()
            .item(NavItem::menu(NavMenu::new("A").link("/a", "A")))
            .item(NavItem::menu(NavMenu::new("B").link("/b", "B")))
            .id("top");
        let html = nav_bar("/", &config).into_string();
        assert!(html.contains(r#"aria-controls="top-menu-1""#), "{html}");
        assert!(html.contains(r#"id="top-menu-1""#), "{html}");
        assert!(html.contains(r#"aria-controls="top-menu-2""#), "{html}");
        assert!(html.contains(r#"id="top-menu-2""#), "{html}");
        assert!(html.contains(r#"id="top-collapse""#), "{html}");
    }

    #[test]
    fn nav_bar_toggle_button_is_hidden_with_accessible_name() {
        let config = NavBarConfig::new();
        let html = nav_bar("/", &config).into_string();
        assert!(html.contains("data-nav-toggle"), "{html}");
        assert!(html.contains("hidden"), "{html}");
        assert!(html.contains(r#"aria-expanded="false""#), "{html}");
        assert!(
            html.contains(r#"aria-controls="autumn-nav-collapse""#),
            "{html}"
        );
        assert!(html.contains(">Menu<"), "{html}");

        let config = NavBarConfig::new().toggle_label("Navigation");
        let html = nav_bar("/", &config).into_string();
        assert!(html.contains(">Navigation<"), "{html}");
    }

    #[test]
    fn nav_bar_trailing_slot_renders_separate_list() {
        let config = NavBarConfig::new().trailing(NavItem::plain_link("/login", "Log in"));
        let html = nav_bar("/", &config).into_string();
        assert!(html.contains("autumn-nav__items--trailing"), "{html}");
        assert!(html.contains(r#"href="/login""#), "{html}");

        let config = NavBarConfig::new();
        let html = nav_bar("/", &config).into_string();
        assert!(!html.contains("autumn-nav__items--trailing"), "{html}");
    }

    #[test]
    fn nav_bar_sidebar_layout_adds_modifier() {
        let config = NavBarConfig::new().layout(NavBarLayout::Sidebar);
        let html = nav_bar("/", &config).into_string();
        assert!(html.contains("autumn-nav--sidebar"), "{html}");

        let config = NavBarConfig::new();
        let html = nav_bar("/", &config).into_string();
        assert!(!html.contains("autumn-nav--sidebar"), "{html}");
    }

    #[test]
    fn nav_bar_class_merges_extra() {
        let config = NavBarConfig::new().class("admin-sidebar");
        let html = nav_bar("/", &config).into_string();
        assert!(
            html.contains(r#"class="autumn-nav admin-sidebar""#),
            "{html}"
        );
    }

    #[test]
    fn nav_item_constructors_accept_owned_strings_without_extra_copy() {
        // href/label should accept `impl Into<String>` (like Cta::primary),
        // not force callers with an already-owned String (e.g. from
        // format!()) through an extra borrow-then-reallocate round trip.
        let post_id = 3;
        let href: String = format!("/posts/{post_id}");
        let label: String = "Posts".to_string();
        let config = NavBarConfig::new()
            .item(NavItem::link(href, label))
            .item(NavItem::plain_link("/x".to_string(), "X".to_string()))
            .item(NavItem::section("System".to_string()))
            .item(NavItem::menu(
                NavMenu::new("Products".to_string())
                    .link(format!("/widgets/{post_id}"), "Widgets".to_string())
                    .plain_link("/external".to_string(), "External".to_string()),
            ));
        let html = nav_bar("/posts/3", &config).into_string();
        assert!(html.contains(r#"href="/posts/3""#), "{html}");
        assert!(html.contains(r#"aria-current="page""#), "{html}");
    }

    #[test]
    fn nav_bar_escapes_labels() {
        let config = NavBarConfig::new()
            .brand("<script>alert(1)</script>", "/")
            .item(NavItem::link("/posts", "<script>alert(2)</script>"))
            .item(NavItem::menu(
                NavMenu::new("<script>alert(3)</script>").link("/x", "<script>alert(4)</script>"),
            ));
        let html = nav_bar("/", &config).into_string();
        assert!(!html.contains("<script>"), "{html}");
        assert!(html.contains("&lt;script&gt;"), "{html}");
    }

    #[test]
    fn nav_bar_emits_no_inline_style_or_script() {
        let config = NavBarConfig::new()
            .brand("Acme", "/")
            .item(NavItem::link("/", "Home"))
            .item(NavItem::menu(
                NavMenu::new("Products").link("/widgets", "Widgets"),
            ))
            .trailing(NavItem::plain_link("/login", "Log in"));
        let html = nav_bar("/", &config).into_string();
        assert!(!html.contains("style="), "{html}");
        assert!(!html.contains("<script"), "{html}");
    }

    // ── property_list ──────────────────────────────────────────────────

    #[test]
    fn property_list_empty_slice_renders_empty_dl_never_panics() {
        let html = property_list(&[]).into_string();
        assert!(html.contains("<dl"), "{html}");
        assert!(!html.contains("<dt"), "{html}");
    }

    #[test]
    fn property_list_has_autumn_property_list_class() {
        let html = property_list(&[]).into_string();
        assert!(html.contains(r#"class="autumn-property-list""#), "{html}");
    }

    #[test]
    fn property_list_renders_dt_and_dd_per_row() {
        let rows = vec![
            ("Title", maud::html! { "My Post" }),
            ("Published", maud::html! { "true" }),
        ];
        let html = property_list(&rows).into_string();
        assert!(html.contains("<dt>Title</dt>"), "{html}");
        assert!(html.contains("<dt>Published</dt>"), "{html}");
        assert!(html.contains("<dd>My Post</dd>"), "{html}");
        assert!(html.contains("<dd>true</dd>"), "{html}");
    }

    #[test]
    fn property_list_escapes_label() {
        let rows = vec![("<script>alert(1)</script>", maud::html! { "safe" })];
        let html = property_list(&rows).into_string();
        assert!(!html.contains("<script>"), "{html}");
        assert!(html.contains("&lt;script&gt;"), "{html}");
    }

    #[test]
    fn property_list_renders_markup_value_unescaped() {
        let rows = vec![("Link", maud::html! { a href="/foo" { "click" } })];
        let html = property_list(&rows).into_string();
        assert!(html.contains(r#"<a href="/foo">click</a>"#), "{html}");
    }

    #[test]
    fn property_list_multiple_rows_all_rendered() {
        let rows = vec![
            ("A", maud::html! { "1" }),
            ("B", maud::html! { "2" }),
            ("C", maud::html! { "3" }),
        ];
        let html = property_list(&rows).into_string();
        assert_eq!(html.matches("<dt>").count(), 3, "{html}");
        assert_eq!(html.matches("<dd>").count(), 3, "{html}");
    }

    // ── data_table ─────────────────────────────────────────────────────

    #[test]
    fn sortdir_param_value() {
        assert_eq!(SortDir::Asc.param_value(), "asc");
        assert_eq!(SortDir::Desc.param_value(), "desc");
    }

    #[test]
    fn sortdir_aria_value() {
        assert_eq!(SortDir::Asc.aria_value(), "ascending");
        assert_eq!(SortDir::Desc.aria_value(), "descending");
    }

    #[test]
    fn sortdir_toggled() {
        assert_eq!(SortDir::Asc.toggled(), SortDir::Desc);
        assert_eq!(SortDir::Desc.toggled(), SortDir::Asc);
    }

    #[test]
    fn data_table_config_defaults() {
        let cfg = DataTableConfig::new("No items");
        assert_eq!(cfg.empty_message, "No items");
        assert_eq!(cfg.sort_param, "sort");
        assert_eq!(cfg.dir_param, "dir");
        assert_eq!(cfg.page_param, "page");
        assert!(cfg.active_sort.is_none());
        assert_eq!(cfg.active_dir, SortDir::Asc);
        assert!(cfg.caption.is_none());
        assert!(cfg.class.is_none());
        assert_eq!(cfg.base_path, "");
        assert_eq!(cfg.query, "");
    }

    #[test]
    fn data_table_config_builders() {
        let cfg = DataTableConfig::new("No items")
            .base_path("/posts")
            .query("q=foo")
            .caption("Posts table")
            .active_sort("title")
            .active_dir(SortDir::Desc)
            .class("my-table");
        assert_eq!(cfg.base_path, "/posts");
        assert_eq!(cfg.query, "q=foo");
        assert_eq!(cfg.caption, Some("Posts table"));
        assert_eq!(cfg.active_sort, Some("title"));
        assert_eq!(cfg.active_dir, SortDir::Desc);
        assert_eq!(cfg.class, Some("my-table"));
    }

    #[test]
    fn data_table_has_thead_th_scope_col() {
        let cols: Vec<Column<i32>> = vec![
            Column::new("Name", |_| maud::html! { "n" }),
            Column::new("Age", |_| maud::html! { "a" }),
        ];
        let html = data_table(&[1i32], &cols, &DataTableConfig::new("empty")).into_string();
        assert!(html.contains("<thead"), "{html}");
        assert!(html.contains(r#"scope="col""#), "{html}");
        assert!(html.contains("Name"), "{html}");
        assert!(html.contains("Age"), "{html}");
    }

    #[test]
    fn data_table_renders_tbody_rows_and_cells() {
        let cols: Vec<Column<&str>> = vec![Column::new("Word", |row| maud::html! { (*row) })];
        let html =
            data_table(&["hello", "world"], &cols, &DataTableConfig::new("empty")).into_string();
        assert!(html.contains("<tbody"), "{html}");
        assert!(html.contains("hello"), "{html}");
        assert!(html.contains("world"), "{html}");
        // Two data rows
        assert_eq!(
            html.matches("<tr").count(),
            3,
            "1 header row + 2 data rows: {html}"
        );
    }

    #[test]
    fn data_table_caption_when_set() {
        let cols: Vec<Column<i32>> = vec![Column::new("X", |_| maud::html! { "x" })];
        let html = data_table(
            &[1i32],
            &cols,
            &DataTableConfig::new("e").caption("My Table"),
        )
        .into_string();
        assert!(html.contains("<caption"), "{html}");
        assert!(html.contains("My Table"), "{html}");
    }

    #[test]
    fn data_table_no_caption_by_default() {
        let cols: Vec<Column<i32>> = vec![Column::new("X", |_| maud::html! { "x" })];
        let html = data_table(&[1i32], &cols, &DataTableConfig::new("e")).into_string();
        assert!(!html.contains("<caption"), "{html}");
    }

    #[test]
    fn data_table_applies_class_when_set() {
        let cols: Vec<Column<i32>> = vec![Column::new("X", |_| maud::html! { "x" })];
        let html =
            data_table(&[1i32], &cols, &DataTableConfig::new("e").class("styled")).into_string();
        assert!(html.contains(r#"class="styled""#), "{html}");
    }

    #[test]
    fn data_table_empty_renders_single_colspan_row() {
        let cols: Vec<Column<i32>> = vec![
            Column::new("A", |_| maud::html! { "a" }),
            Column::new("B", |_| maud::html! { "b" }),
            Column::new("C", |_| maud::html! { "c" }),
        ];
        let html = data_table(&[], &cols, &DataTableConfig::new("Nothing here")).into_string();
        assert!(html.contains("Nothing here"), "{html}");
        assert!(html.contains(r#"colspan="3""#), "{html}");
        // Only the header tr, no data rows
        assert_eq!(html.matches("<tr").count(), 2, "header + empty row: {html}");
    }

    #[test]
    fn data_table_empty_message_announced() {
        let cols: Vec<Column<i32>> = vec![Column::new("X", |_| maud::html! { "x" })];
        let html = data_table(&[], &cols, &DataTableConfig::new("No results")).into_string();
        assert!(html.contains(r#"role="status""#), "{html}");
    }

    #[test]
    fn data_table_nonsortable_header_is_plain() {
        let cols: Vec<Column<i32>> = vec![Column::new("Title", |_| maud::html! { "t" })];
        let html = data_table(&[1i32], &cols, &DataTableConfig::new("e")).into_string();
        // No sort link in header
        let thead_end = html.find("</thead>").unwrap_or(html.len());
        let thead = &html[..thead_end];
        assert!(!thead.contains("<a "), "{html}");
        assert!(!thead.contains("aria-sort"), "{html}");
    }

    #[test]
    fn data_table_sortable_inactive_has_link_and_aria_sort_none() {
        let cols: Vec<Column<i32>> =
            vec![Column::new("Title", |_| maud::html! { "t" }).sortable("title")];
        let cfg = DataTableConfig::new("e").base_path("/posts");
        let html = data_table(&[1i32], &cols, &cfg).into_string();
        assert!(html.contains(r#"aria-sort="none""#), "{html}");
        assert!(html.contains("sort=title"), "{html}");
        assert!(html.contains("dir=asc"), "{html}");
    }

    #[test]
    fn data_table_sortable_active_reflects_dir_and_toggles() {
        let cols: Vec<Column<i32>> =
            vec![Column::new("Title", |_| maud::html! { "t" }).sortable("title")];
        let cfg = DataTableConfig::new("e")
            .base_path("/posts")
            .active_sort("title")
            .active_dir(SortDir::Asc);
        let html = data_table(&[1i32], &cols, &cfg).into_string();
        assert!(html.contains(r#"aria-sort="ascending""#), "{html}");
        // toggled dir link
        assert!(html.contains("dir=desc"), "{html}");
    }

    #[test]
    fn data_table_sortable_link_preserves_other_query_params() {
        let cols: Vec<Column<i32>> =
            vec![Column::new("Name", |_| maud::html! { "n" }).sortable("name")];
        let cfg = DataTableConfig::new("e")
            .base_path("/posts")
            .query("q=hello");
        let html = data_table(&[1i32], &cols, &cfg).into_string();
        assert!(html.contains("q=hello"), "{html}");
    }

    #[test]
    fn data_table_sortable_link_drops_existing_sort_dir_page() {
        let cols: Vec<Column<i32>> =
            vec![Column::new("Name", |_| maud::html! { "n" }).sortable("name")];
        let cfg = DataTableConfig::new("e")
            .base_path("/posts")
            .query("q=foo&sort=old&dir=asc&page=3");
        let html = data_table(&[1i32], &cols, &cfg).into_string();
        // old sort params stripped, new sort=name present
        assert!(html.contains("sort=name"), "{html}");
        // page stripped (reset to page 1)
        assert!(!html.contains("page=3"), "{html}");
        // q preserved
        assert!(html.contains("q=foo"), "{html}");
        // no duplicate old sort
        assert!(!html.contains("sort=old"), "{html}");
    }

    // ── CardConfig builder ─────────────────────────────────────────────

    #[test]
    fn card_config_defaults() {
        // no title/action → no card-header rendered
        let html = card(&maud::html! {}, &CardConfig::new()).into_string();
        assert!(!html.contains("card-header"), "{html}");
        assert!(!html.contains("card-footer"), "{html}");
        // default heading level is H2: setting a title renders <h2
        let html2 = card(&maud::html! {}, &CardConfig::new().title("X")).into_string();
        assert!(html2.contains("<h2"), "{html2}");
    }

    #[test]
    fn card_config_builders_chain() {
        let html = card(
            &maud::html! {},
            &CardConfig::new()
                .title("T")
                .level(HeadingLevel::H3)
                .class("wide"),
        )
        .into_string();
        assert!(html.contains(r#"class="card wide""#), "{html}");
        assert!(html.contains("<h3"), "{html}");
        assert!(html.contains('T'), "{html}");
    }

    // ── card structure ─────────────────────────────────────────────────

    #[test]
    fn card_has_root_and_body_classes() {
        let body = maud::html! { p { "hello" } };
        let html = card(&body, &CardConfig::new()).into_string();
        assert!(html.contains(r#"class="card""#), "{html}");
        assert!(html.contains(r#"class="card-body""#), "{html}");
        assert!(html.contains("hello"), "{html}");
        assert!(!html.contains("card-header"), "{html}");
        assert!(!html.contains("card-footer"), "{html}");
    }

    #[test]
    fn card_renders_title_as_h2_by_default() {
        let body = maud::html! {};
        let html = card(&body, &CardConfig::new().title("Posts")).into_string();
        assert!(
            html.contains(r#"<h2 class="card-title">Posts</h2>"#),
            "{html}"
        );
        assert!(html.contains(r#"class="card-header""#), "{html}");
    }

    #[test]
    fn card_title_respects_heading_level() {
        let body = maud::html! {};
        let html = card(&body, &CardConfig::new().title("X").level(HeadingLevel::H3)).into_string();
        assert!(html.contains(r#"<h3 class="card-title">"#), "{html}");
        assert!(!html.contains("<h2"), "{html}");
    }

    #[test]
    fn card_omits_header_when_empty() {
        let body = maud::html! {};
        let html = card(&body, &CardConfig::new()).into_string();
        assert!(!html.contains("card-header"), "{html}");
    }

    #[test]
    fn card_renders_header_action() {
        let action = maud::html! { a class="btn" href="/new" { "New" } };
        let body = maud::html! {};
        let html = card(&body, &CardConfig::new().header_action(action)).into_string();
        assert!(html.contains(r#"class="card-header""#), "{html}");
        assert!(html.contains(r#"class="btn""#), "{html}");
        assert!(html.contains(r#"href="/new""#), "{html}");
    }

    #[test]
    fn card_header_action_only_no_title() {
        let action = maud::html! { button { "Click" } };
        let body = maud::html! {};
        let html = card(&body, &CardConfig::new().header_action(action)).into_string();
        assert!(html.contains("card-header"), "{html}");
        assert!(!html.contains("card-title"), "{html}");
        assert!(!html.contains("<h2"), "{html}");
    }

    #[test]
    fn card_renders_footer() {
        let footer = maud::html! { span { "Save" } };
        let body = maud::html! {};
        let html = card(&body, &CardConfig::new().footer(footer)).into_string();
        assert!(html.contains(r#"class="card-footer""#), "{html}");
        assert!(html.contains("Save"), "{html}");
    }

    #[test]
    fn card_omits_footer_when_none() {
        let body = maud::html! {};
        let html = card(&body, &CardConfig::new()).into_string();
        assert!(!html.contains("card-footer"), "{html}");
    }

    #[test]
    fn card_extra_class_escape_hatch() {
        let body = maud::html! {};
        let html = card(&body, &CardConfig::new().class("dashboard")).into_string();
        assert!(html.contains(r#"class="card dashboard""#), "{html}");
    }

    #[test]
    fn card_escapes_title() {
        let body = maud::html! {};
        let html = card(&body, &CardConfig::new().title("<script>alert(1)</script>")).into_string();
        assert!(html.contains("&lt;script&gt;"), "{html}");
        assert!(!html.contains("<script>alert"), "{html}");
    }

    #[test]
    fn card_title_html_passes_markup() {
        let rich = maud::html! {
            span { "Posts" }
            span class="muted" { " (42)" }
        };
        let body = maud::html! {};
        let html = card(&body, &CardConfig::new().title_html(rich)).into_string();
        assert!(html.contains(r#"class="muted""#), "{html}");
        assert!(html.contains("(42)"), "{html}");
    }

    #[test]
    fn card_body_composes_property_list() {
        let rows = vec![("Name", maud::html! { "Alice" })];
        let body = property_list(&rows);
        let html = card(&body, &CardConfig::new().title("Detail")).into_string();
        assert!(html.contains(r#"class="card-body""#), "{html}");
        assert!(html.contains(r#"class="autumn-property-list""#), "{html}");
    }

    // ── stat_card ──────────────────────────────────────────────────────

    #[test]
    fn stat_card_renders_label_value() {
        let html = stat_card("Users", "42", None).into_string();
        assert!(html.contains(r#"class="stat-card""#), "{html}");
        assert!(html.contains(r#"class="stat-label""#), "{html}");
        assert!(html.contains(r#"class="stat-value""#), "{html}");
        assert!(html.contains("Users"), "{html}");
        assert!(html.contains("42"), "{html}");
        assert!(!html.contains("stat-link"), "{html}");
    }

    #[test]
    fn stat_card_renders_optional_link() {
        let html = stat_card("Users", "42", Some(("/users", "View all →"))).into_string();
        assert!(html.contains(r#"class="stat-link""#), "{html}");
        assert!(html.contains(r#"href="/users""#), "{html}");
        assert!(html.contains("View all"), "{html}");
    }

    #[test]
    fn stat_card_escapes_value() {
        let html = stat_card("L", "<b>x</b>", None).into_string();
        assert!(html.contains("&lt;b&gt;"), "{html}");
        assert!(!html.contains("<b>x</b>"), "{html}");
    }

    // ── hero ─────────────────────────────────────────────────────────────

    #[test]
    fn hero_title_only() {
        let html = hero(&HeroConfig::new("Welcome")).into_string();
        assert!(html.contains("Welcome"), "{html}");
        assert!(html.contains(r#"class="autumn-hero""#), "{html}");
        // Single top-level heading, no subtitle, no CTA container.
        assert!(html.contains("<h1"), "{html}");
        assert_eq!(html.matches("<h1").count(), 1, "{html}");
        assert!(!html.contains("autumn-hero__subtitle"), "{html}");
        assert!(!html.contains("autumn-hero__actions"), "{html}");
        assert!(!html.contains("<a"), "{html}");
    }

    #[test]
    fn hero_title_and_subtitle() {
        let html = hero(&HeroConfig::new("Welcome").subtitle("A tagline")).into_string();
        assert!(html.contains("Welcome"), "{html}");
        assert!(html.contains(r#"class="autumn-hero__subtitle""#), "{html}");
        assert!(html.contains("A tagline"), "{html}");
    }

    #[test]
    fn hero_title_subtitle_two_ctas() {
        let html = hero(
            &HeroConfig::new("Welcome")
                .subtitle("A tagline")
                .cta(Cta::primary("Get started", "/signup"))
                .cta(Cta::secondary("Learn more", "/about")),
        )
        .into_string();
        assert!(html.contains(r#"class="autumn-hero__actions""#), "{html}");
        assert!(html.contains(r#"href="/signup""#), "{html}");
        assert!(html.contains("Get started"), "{html}");
        assert!(html.contains(r#"href="/about""#), "{html}");
        assert!(html.contains("Learn more"), "{html}");
        assert!(html.contains("autumn-hero__cta--primary"), "{html}");
        assert!(html.contains("autumn-hero__cta--secondary"), "{html}");
    }

    #[test]
    fn hero_heading_level_configurable() {
        let html = hero(&HeroConfig::new("Welcome").level(HeadingLevel::H2)).into_string();
        assert!(html.contains("<h2"), "{html}");
        assert!(!html.contains("<h1"), "{html}");
    }

    #[test]
    fn hero_default_heading_level_is_h1() {
        let config = HeroConfig::new("Welcome");
        let html = hero(&config).into_string();
        assert!(html.contains("<h1"), "{html}");
    }

    #[test]
    fn hero_escapes_title_and_subtitle() {
        let html = hero(&HeroConfig::new("<b>hi</b>").subtitle("<i>tag</i>")).into_string();
        assert!(html.contains("&lt;b&gt;"), "{html}");
        assert!(!html.contains("<b>hi</b>"), "{html}");
        assert!(html.contains("&lt;i&gt;"), "{html}");
        assert!(!html.contains("<i>tag</i>"), "{html}");
    }

    #[test]
    fn hero_extra_class_escape_hatch() {
        let html = hero(&HeroConfig::new("Welcome").class("centered")).into_string();
        assert!(html.contains(r#"class="autumn-hero centered""#), "{html}");
    }

    #[test]
    fn hero_cta_and_class_accept_owned_strings_built_inline() {
        // Cta::primary/secondary and .class() take `impl Into<String>` so a
        // caller can build them from owned data (e.g. a DB row) in the same
        // expression, without a `let` binding to extend a temporary's lifetime.
        let post_id = 42;
        let html = hero(
            &HeroConfig::new("Welcome")
                .class(format!("theme-{post_id}"))
                .cta(Cta::primary(
                    format!("Read post {post_id}"),
                    format!("/posts/{post_id}"),
                )),
        )
        .into_string();
        assert!(html.contains(r#"class="autumn-hero theme-42""#), "{html}");
        assert!(html.contains(r#"href="/posts/42""#), "{html}");
        assert!(html.contains("Read post 42"), "{html}");
    }

    #[test]
    fn cta_style_default_is_primary() {
        assert_eq!(CtaStyle::default(), CtaStyle::Primary);
    }

    // ── charts (#1231) ─────────────────────────────────────────────────

    #[test]
    fn sparkline_is_accessible_inline_svg() {
        let html = sparkline(&[("a", 1.0), ("b", 2.0), ("c", 1.5)]).into_string();
        assert!(html.contains(r#"role="img""#), "{html}");
        assert!(
            html.contains("autumn-chart autumn-chart--sparkline"),
            "{html}"
        );
        assert!(html.contains("autumn-chart__svg"), "{html}");
        assert!(html.contains("<polyline"), "{html}");
        assert!(html.contains("<title>Sparkline</title>"), "{html}");
        assert!(html.contains("<desc>"), "{html}");
    }

    #[test]
    fn bar_chart_emits_a_rect_per_datum() {
        let html = bar_chart(&[("Q1", 3.0), ("Q2", 5.0), ("Q3", 4.0)]).into_string();
        assert_eq!(html.matches("<rect").count(), 3, "{html}");
        assert!(html.contains("autumn-chart--bar"), "{html}");
        assert!(html.contains("autumn-chart__bar"), "{html}");
    }

    #[test]
    fn line_chart_emits_polyline_and_points() {
        let html = line_chart(&[("a", 1.0), ("b", 9.0), ("c", 4.0)]).into_string();
        assert!(html.contains("autumn-chart--line"), "{html}");
        assert!(html.contains("<polyline"), "{html}");
        assert_eq!(html.matches("<circle").count(), 3, "{html}");
    }

    #[test]
    fn charts_never_emit_script_or_inline_style() {
        for html in [
            sparkline(&[("a", 1.0), ("b", 2.0)]).into_string(),
            bar_chart(&[("a", 1.0), ("b", 2.0)]).into_string(),
            line_chart(&[("a", 1.0), ("b", 2.0)]).into_string(),
        ] {
            assert!(!html.contains("<script"), "no scripts: {html}");
            assert!(!html.contains("style="), "no inline style: {html}");
            assert!(!html.contains("http://"), "no external assets: {html}");
        }
    }

    #[test]
    fn chart_escapes_hostile_labels_in_table_fallback() {
        let html = bar_chart_with(
            &[("<script>alert(1)</script>", 5.0)],
            &ChartConfig::new().with_table(),
        )
        .into_string();
        assert!(html.contains("&lt;script&gt;"), "{html}");
        assert!(!html.contains("<script>alert"), "{html}");
    }

    #[test]
    fn empty_input_renders_valid_svg_without_panic() {
        for html in [
            sparkline(&[]).into_string(),
            bar_chart(&[]).into_string(),
            line_chart(&[]).into_string(),
        ] {
            assert!(html.contains(r#"role="img""#), "{html}");
            assert!(html.contains("<desc>No data</desc>"), "{html}");
            assert!(!html.contains("NaN"), "no NaN coordinates: {html}");
            assert!(!html.contains("<script"), "{html}");
        }
    }

    #[test]
    fn single_point_does_not_divide_by_zero() {
        let html = sparkline(&[("only", 7.0)]).into_string();
        assert!(
            html.contains("<circle"),
            "single point renders a dot: {html}"
        );
        assert!(!html.contains("NaN"), "{html}");
        // Centered horizontally, mid-height (span is zero).
        assert!(html.contains(r#"cx="50""#), "{html}");
    }

    #[test]
    fn all_equal_positive_values_render_full_height_bars() {
        let html = bar_chart(&[("a", 4.0), ("b", 4.0), ("c", 4.0)]).into_string();
        assert!(!html.contains("NaN"), "{html}");
        // Degenerate span (all-equal positive): read as full-magnitude bars, so
        // every bar spans the full usable height (H - 2*PAD = 40 - 4 = 36), not
        // the 0.5 midline that suits a flat line.
        assert_eq!(html.matches(r#"height="36""#).count(), 3, "{html}");
        assert!(!html.contains(r#"height="18""#), "not half height: {html}");
    }

    #[test]
    fn all_zero_bars_render_zero_height() {
        // All-zero (zero-activity) series: the degenerate-span midline would draw
        // misleading half-height bars. Instead the bars must have zero height.
        let html = bar_chart(&[("a", 0.0), ("b", 0.0)]).into_string();
        assert!(!html.contains("NaN"), "{html}");
        assert_eq!(html.matches(r#"height="0""#).count(), 2, "{html}");
        // No positive-height bar rect (in particular, no half-height 18).
        assert!(
            !html.contains(r#"height="18""#),
            "no half-height bars: {html}"
        );
    }

    /// Extract every `<rect>` `height="…"` value, in document (series) order.
    #[cfg(test)]
    fn bar_heights(html: &str) -> Vec<f64> {
        html.match_indices(r#"height=""#)
            .map(|(i, m)| {
                let rest = &html[i + m.len()..];
                let end = rest.find('"').unwrap();
                rest[..end].parse::<f64>().unwrap()
            })
            .collect()
    }

    #[test]
    fn bar_auto_baseline_includes_zero_so_min_positive_bar_is_visible() {
        // All-positive series whose minimum (7) sits well above zero. With the
        // automatic baseline anchored at zero, the smallest bar must still get a
        // proportional, POSITIVE height — not collapse to zero because the data
        // minimum was treated as the axis floor.
        let html = bar_chart(&[("Q1", 12.0), ("Q2", 19.0), ("Q3", 7.0)]).into_string();
        assert!(!html.contains("NaN"), "{html}");
        let heights = bar_heights(&html);
        assert_eq!(heights.len(), 3, "{html}");
        let (q1, q2, q3) = (heights[0], heights[1], heights[2]);
        // Q3 (the smallest bar) is visible: strictly positive height.
        assert!(q3 > 0.0, "smallest bar must be visible, got {q3}: {html}");
        assert!(
            !html.contains(r#"height="0""#),
            "no zero-height bar: {html}"
        );
        // Bars are ordered by magnitude from a zero baseline.
        assert!(q2 > q3, "tallest (Q2=19) taller than Q3=7: {q2} vs {q3}");
        assert!(q2 > q1 && q1 > q3, "12 between 7 and 19: {q1}");
        // Q2 is the max, so it fills the usable height (H - 2*PAD = 36).
        assert!((q2 - 36.0).abs() < 1e-6, "max bar is full height: {q2}");
        // Q3 = 7/19 of full height (~13.26).
        let expected_q3 = 7.0 / 19.0 * 36.0;
        assert!(
            (q3 - expected_q3).abs() < 1e-3,
            "Q3 scaled from zero baseline: {q3}"
        );
    }

    #[test]
    fn explicit_bar_min_override_wins_over_zero_baseline() {
        // When the caller pins `min`, that override is respected unchanged — no
        // zero is injected. Pinning min to the data minimum (7) puts the smallest
        // bar back at zero height, exactly as the caller asked.
        let html = bar_chart_with(
            &[("Q1", 12.0), ("Q2", 19.0), ("Q3", 7.0)],
            &ChartConfig::new().min(7.0),
        )
        .into_string();
        assert!(!html.contains("NaN"), "{html}");
        let heights = bar_heights(&html);
        assert_eq!(heights.len(), 3, "{html}");
        // Q3 == the pinned min, so it renders at zero height (caller override wins).
        assert!(
            (heights[2] - 0.0).abs() < 1e-9,
            "explicit min pins Q3 to zero: {}",
            heights[2]
        );
        assert!(html.contains(r#"height="0""#), "{html}");
    }

    #[test]
    fn negative_bars_diverge_downward_largest_magnitude_is_tallest() {
        // An all-negative series auto-resolves to bounds [-10, 0] (zero-anchored
        // upper bound). Bars must grow DOWN from the zero line, with length
        // proportional to |value| — so the largest-magnitude bar (-10) is the
        // tallest, not collapsed to zero because it sits at the axis floor.
        let html = bar_chart(&[("a", -10.0), ("b", -5.0)]).into_string();
        assert!(!html.contains("NaN"), "{html}");
        let heights = bar_heights(&html);
        assert_eq!(heights.len(), 2, "{html}");
        let (a, b) = (heights[0], heights[1]);
        // -10 is the largest magnitude: it must be visible and TALLER than -5,
        // never zero height.
        assert!(a > 0.0, "largest-magnitude bar must be visible: {a}");
        assert!(a > b, "|-10| taller than |-5|: {a} vs {b}");
        // zero_frac = 1 (zero at the top). -10 -> full usable height (36).
        assert!((a - 36.0).abs() < 1e-6, "-10 fills usable height: {a}");
        // -5 -> half of full height (18).
        assert!((b - 18.0).abs() < 1e-6, "-5 is half height: {b}");
        assert!(
            !html.contains(r#"height="0""#),
            "no zero-height bar for a nonzero series: {html}"
        );
    }

    /// Extract every `<rect>` `y="…"` value, in document (series) order.
    #[cfg(test)]
    fn bar_ys(html: &str) -> Vec<f64> {
        html.match_indices(r#"y=""#)
            .map(|(i, m)| {
                let rest = &html[i + m.len()..];
                let end = rest.find('"').unwrap();
                rest[..end].parse::<f64>().unwrap()
            })
            .collect()
    }

    #[test]
    fn mixed_sign_bars_share_a_zero_baseline() {
        // A mixed series [+10, -5] resolves to bounds [-5, 10]; the zero line
        // sits 1/3 up from the bottom. The +10 bar must sit ABOVE the zero line
        // and the -5 bar BELOW it, both meeting at the shared baseline y.
        let html = bar_chart(&[("up", 10.0), ("down", -5.0)]).into_string();
        assert!(!html.contains("NaN"), "{html}");
        let heights = bar_heights(&html);
        let ys = bar_ys(&html);
        assert_eq!(heights.len(), 2, "{html}");
        assert_eq!(ys.len(), 2, "{html}");
        // Both bars have positive, visible height.
        assert!(heights[0] > 0.0 && heights[1] > 0.0, "both visible: {html}");
        // Shared zero line: bottom (H-PAD=38) - zero_frac*U. With min=-5, max=10,
        // span=15, zero_frac = 5/15 = 1/3, U = 36 -> y_zero = 38 - 12 = 26.
        let y_zero = 26.0;
        // +10 bar: above the zero line, so its BOTTOM edge (y + height) meets it.
        let up_bottom = ys[0] + heights[0];
        assert!(
            (up_bottom - y_zero).abs() < 1e-6,
            "positive bar's bottom edge on the zero line: {up_bottom} vs {y_zero}"
        );
        // -5 bar: below the zero line, so its TOP edge (y) meets it.
        assert!(
            (ys[1] - y_zero).abs() < 1e-6,
            "negative bar's top edge on the zero line: {} vs {y_zero}",
            ys[1]
        );
    }

    #[test]
    fn negative_values_scale_without_panic() {
        let html = line_chart(&[("a", -5.0), ("b", 0.0), ("c", 5.0)]).into_string();
        assert!(!html.contains("NaN"), "{html}");
        assert!(html.contains("<polyline"), "{html}");
        // Min (-5) sits at the bottom, max (5) at the top of the inset area.
        assert!(html.contains(r#"points="3,37"#), "min at bottom: {html}");
    }

    #[test]
    fn explicit_axis_override_changes_coordinates() {
        let data = [("a", 0.0), ("b", 5.0), ("c", 10.0)];
        let auto = sparkline(&data).into_string();
        let overridden =
            sparkline_with(&data, &ChartConfig::new().min(0.0).max(100.0)).into_string();
        assert_ne!(
            auto, overridden,
            "caller min/max override must move the plotted points"
        );
        // Under the 0..100 override the points compress toward the baseline
        // (the auto-scaled chart puts the middle datum at y=15 instead).
        assert!(overridden.contains("50,26.7"), "{overridden}");
        assert!(auto.contains("50,15"), "{auto}");
    }

    #[test]
    fn non_finite_axis_override_is_ignored_no_nan_in_output() {
        let data = [("a", 1.0), ("b", 2.0), ("c", 3.0)];
        // A non-finite caller override (e.g. arriving from an upstream
        // aggregate's `Option<f64>`) must be treated as UNSET: the auto-scaled
        // bound is used instead, so the emitted SVG can never contain invalid
        // coordinates like `NaN` / `inf` / `Infinity`.
        let bad_configs = [
            ChartConfig::new().max(f64::NAN),
            ChartConfig::new().min(f64::INFINITY),
            ChartConfig::new().min(f64::NEG_INFINITY),
            ChartConfig::new().max(f64::INFINITY),
        ];
        let check = |name: &str, render: &dyn Fn(&ChartConfig<'_>) -> String| {
            let auto = render(&ChartConfig::new());
            for config in &bad_configs {
                let overridden = render(config);
                for needle in ["NaN", "inf", "Infinity"] {
                    assert!(
                        !overridden.contains(needle),
                        "{name}: non-finite override leaked `{needle}` into SVG: {overridden}"
                    );
                }
                // The non-finite override is ignored, so the geometry is
                // identical to the un-overridden auto-scaled chart.
                assert_eq!(
                    auto, overridden,
                    "{name}: non-finite override must be ignored (auto-scale used)"
                );
            }
        };
        check("sparkline", &|config| {
            sparkline_with(&data, config).into_string()
        });
        check("line_chart", &|config| {
            line_chart_with(&data, config).into_string()
        });
        check("bar_chart", &|config| {
            bar_chart_with(&data, config).into_string()
        });
    }

    #[test]
    fn desc_reports_data_range_not_axis_override() {
        // Caller pins the axis to 0..100 but leaves `desc` unset. The accessible
        // `<desc>` must announce the actual data range (0..10), not the override.
        let html = sparkline_with(
            &[("a", 0.0), ("b", 5.0), ("c", 10.0)],
            &ChartConfig::new().max(100.0),
        )
        .into_string();
        assert!(
            html.contains("<desc>3 data points, ranging from 0 to 10.</desc>"),
            "{html}"
        );
        assert!(
            !html.contains("ranging from 0 to 100"),
            "must not leak axis override into desc: {html}"
        );
    }

    #[test]
    fn table_fallback_lists_labels_and_values() {
        let html = line_chart_with(
            &[("Mon", 3.0), ("Tue", 5.5)],
            &ChartConfig::new().with_table(),
        )
        .into_string();
        assert!(html.contains("autumn-chart__table"), "{html}");
        assert!(html.contains("autumn-visually-hidden"), "{html}");
        assert!(html.contains("<td>Mon</td>"), "{html}");
        assert!(html.contains("<td>5.5</td>"), "{html}");
    }

    #[test]
    fn config_title_and_desc_feed_accessibility() {
        let html = sparkline_with(
            &[("a", 1.0), ("b", 2.0)],
            &ChartConfig::new()
                .title("Weekly signups")
                .desc("Signups per day for the last week"),
        )
        .into_string();
        assert!(html.contains(r#"aria-label="Weekly signups""#), "{html}");
        assert!(html.contains("<title>Weekly signups</title>"), "{html}");
        assert!(
            html.contains("<desc>Signups per day for the last week</desc>"),
            "{html}"
        );
    }

    #[test]
    fn config_class_is_merged_onto_root() {
        let html = bar_chart_with(&[("a", 1.0)], &ChartConfig::new().class("dashboard-widget"))
            .into_string();
        assert!(
            html.contains("autumn-chart autumn-chart--bar dashboard-widget"),
            "{html}"
        );
    }

    #[test]
    fn fmt_coord_trims_trailing_zeros() {
        assert_eq!(fmt_coord(10.0), "10");
        assert_eq!(fmt_coord(0.5), "0.5");
        assert_eq!(fmt_coord(0.0), "0");
        assert_eq!(fmt_coord(-0.0), "0");
        assert_eq!(fmt_coord(1.2345), "1.234");
    }
}
