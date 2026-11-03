//! Search widgets: active search and autocomplete.
//!
//! Needs the `widgets-search` feature (on by default).

use super::selector_to_id;

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
#[must_use]
pub fn active_search_empty_state(message: &str) -> maud::Markup {
    maud::html! {
        div
            role="status"
            aria-live="polite"
            class="autumn-search-empty" {
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
/// # Keyboard (with `autumn-widgets.js`)
///
/// The listbox binds the keyboard core (`data-autumn-activedescendant`,
/// `data-autumn-dismiss`). Focus stays on the input. `ArrowDown`/`ArrowUp` move
/// `aria-activedescendant` (wrapping) and open a closed popup. `Enter` selects
/// the active option. `Escape`, a click outside, or `Tab` closes the popup.
/// `aria-expanded` is `"true"` only while the popup shows an option.
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
                class="autumn-autocomplete__options"
                data-autumn-activedescendant
                data-autumn-orientation="vertical"
                data-autumn-dismiss {}
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
/// The `autumn-widgets.js` runtime handles clicks on the listbox and keys on
/// the input, and uses `data-value` to populate the hidden field. The option
/// has `tabindex="-1"`: focus stays on the input. The runtime gives the
/// active option an `id` if it has none.
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
#[must_use]
pub fn autocomplete_option(value: &str, label: &str) -> maud::Markup {
    maud::html! {
        div
            role="option"
            tabindex="-1"
            class="autumn-autocomplete__option"
            data-value=(value) {
            (label)
        }
    }
}

/// Render an autocomplete empty-state partial.
///
/// Return this from your autocomplete handler when no records match the query.
#[must_use]
pub fn autocomplete_empty_state(message: &str) -> maud::Markup {
    maud::html! {
        div
            role="status"
            aria-live="polite"
            class="autumn-autocomplete-empty" {
            (message)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widgets::{
        AlertConfig, AlertVariant, CardConfig, alert_with, card, nav_link, stat_card,
    };

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

    #[test]
    fn option_is_not_a_tab_stop() {
        // APG combobox: focus stays on the input. Options are not tab stops.
        let html = autocomplete_option("1", "Option").into_string();
        assert!(html.contains(r#"tabindex="-1""#), "{html}");
    }

    #[test]
    fn autocomplete_listbox_binds_keyboard_core() {
        let config = AutocompleteConfig::new("/ac", "value_field");
        let html = autocomplete_input("x", "Label", &config).into_string();
        let listbox = html
            .split(r#"id="x-options""#)
            .nth(1)
            .and_then(|rest| rest.split('>').next())
            .expect("listbox tag");
        assert!(listbox.contains("data-autumn-activedescendant"), "{html}");
        assert!(listbox.contains("data-autumn-dismiss"), "{html}");
        // The core finds the controller through `aria-controls`.
        assert!(html.contains(r#"aria-controls="x-options""#), "{html}");
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

    // ── widget class namespace (#2354) ──────────────────────────────────

    /// Every semantic class the renamed widgets emit must live in the
    /// `autumn-*` namespace, and the old unprefixed hooks must be gone:
    /// the standalone widget stylesheet only styles namespaced classes.
    #[test]
    fn renamed_widgets_emit_namespaced_classes_only() {
        let body = maud::html! { p { "hello" } };
        let html = card(
            &body,
            &CardConfig::new()
                .title("Posts")
                .header_action(maud::html! { a href="/new" { "New" } })
                .footer(maud::html! { span { "Save" } }),
        )
        .into_string();
        for class in [
            "autumn-card",
            "autumn-card__header",
            "autumn-card__title",
            "autumn-card__body",
            "autumn-card__footer",
        ] {
            assert!(html.contains(class), "missing {class}: {html}");
        }
        for stale in [
            r#"class="card""#,
            "card-header",
            "card-title",
            "card-body",
            "card-footer",
        ] {
            assert!(
                !html.contains(stale),
                "stale unprefixed hook {stale}: {html}"
            );
        }

        let html = stat_card("Users", "42", Some(("/users", "View all"))).into_string();
        for class in [
            "autumn-stat-card",
            "autumn-stat-card__label",
            "autumn-stat-card__value",
            "autumn-stat-card__link",
        ] {
            assert!(html.contains(class), "missing {class}: {html}");
        }
        for stale in [
            r#"class="stat-card""#,
            "stat-label",
            "stat-value",
            "stat-link",
        ] {
            assert!(
                !html.contains(stale),
                "stale unprefixed hook {stale}: {html}"
            );
        }

        let html = nav_link("/posts", "/posts", "Posts").into_string();
        assert!(html.contains(r#"class="autumn-active""#), "{html}");
        assert!(!html.contains(r#"class="active""#), "{html}");

        let html = active_search_empty_state("No results").into_string();
        assert!(html.contains(r#"class="autumn-search-empty""#), "{html}");
        assert!(!html.contains(r#"class="search-empty""#), "{html}");

        let html = autocomplete_empty_state("No matches").into_string();
        assert!(
            html.contains(r#"class="autumn-autocomplete-empty""#),
            "{html}"
        );
        assert!(!html.contains(r#"class="autocomplete-empty""#), "{html}");

        let html = alert_with(
            AlertVariant::Info,
            maud::html! { "hi" },
            &AlertConfig::new().icon(true),
        )
        .into_string();
        assert!(html.contains(r#"class="autumn-alert__icon-svg""#), "{html}");
        assert!(!html.contains(r#"class="alert__icon-svg""#), "{html}");
        // #3396: the glyph color comes from CSS (`--surface`), so it stays
        // visible on the light dark-scheme circle.
        assert!(
            html.contains(r#"class="autumn-alert__icon-glyph""#),
            "{html}"
        );
    }
}
