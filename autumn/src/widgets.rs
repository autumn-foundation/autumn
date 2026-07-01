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
                value="";
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
                            option value=(val) { (lbl) }
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

// ── data_table ────────────────────────────────────────────────────────────

/// Sort direction for a [`data_table`] sortable column header.
///
/// `Asc` is the default. Use [`SortDir::toggled`] to flip the direction
/// when building sort links for the active column.
#[cfg(feature = "maud")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortDir {
    /// Ascending order (default).
    #[default]
    Asc,
    /// Descending order.
    Desc,
}

#[cfg(feature = "maud")]
impl SortDir {
    /// Query-parameter value: `"asc"` or `"desc"`.
    #[must_use]
    pub const fn param_value(self) -> &'static str {
        match self {
            Self::Asc => "asc",
            Self::Desc => "desc",
        }
    }

    /// `aria-sort` attribute value: `"ascending"` or `"descending"`.
    #[must_use]
    pub const fn aria_value(self) -> &'static str {
        match self {
            Self::Asc => "ascending",
            Self::Desc => "descending",
        }
    }

    /// Returns the opposite direction.
    #[must_use]
    pub const fn toggled(self) -> Self {
        match self {
            Self::Asc => Self::Desc,
            Self::Desc => Self::Asc,
        }
    }
}

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
/// `class` applied. Shared by every widget with a configurable heading level
/// (`card`, `hero`, ...) so an `HeadingLevel` variant is matched in one place.
#[cfg(feature = "maud")]
fn heading(level: HeadingLevel, class: &str, content: &maud::Markup) -> maud::Markup {
    maud::html! {
        @match level {
            HeadingLevel::H1 => h1 class=(class) { (content) },
            HeadingLevel::H2 => h2 class=(class) { (content) },
            HeadingLevel::H3 => h3 class=(class) { (content) },
            HeadingLevel::H4 => h4 class=(class) { (content) },
            HeadingLevel::H5 => h5 class=(class) { (content) },
            HeadingLevel::H6 => h6 class=(class) { (content) },
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
                        (heading(config.level, "card-title", title))
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
            (heading(config.level, "autumn-hero__title", &config.title))
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

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(all(test, feature = "maud"))]
mod tests {
    use super::*;

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
}
