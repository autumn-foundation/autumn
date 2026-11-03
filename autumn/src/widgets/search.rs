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
    /// Multi-value (tags) mode (issue #3410). Each pick is a removable
    /// chip. Each chip submits one hidden `{value_name}={value}` input, so a
    /// `Vec<T>` field decodes the repeated keys. Default: `false`.
    pub multiple: bool,
    /// The `(value, label)` chips that show first in multiple mode, for
    /// example from the database on an edit form. Default: none.
    pub selected: &'a [(&'a str, &'a str)],
    /// The first word of the `aria-label` of each chip's remove button. The
    /// chip label comes after it: `"Remove rust"`. Default: `"Remove"`.
    pub remove_label: &'a str,
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
            multiple: false,
            selected: &[],
            remove_label: "Remove",
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

    /// Turn on multi-value (tags) mode. See [`AutocompleteConfig::multiple`]
    /// (the field).
    ///
    /// With [`free_text`](Self::free_text), Enter adds the typed text as a
    /// chip when no option is active.
    #[must_use]
    pub const fn multiple(mut self) -> Self {
        self.multiple = true;
        self
    }

    /// Show these `(value, label)` chips first (multiple mode only).
    ///
    /// To pre-fill from a [`Changeset`](crate::form::Changeset), use
    /// [`autocomplete_input_for`].
    #[must_use]
    pub const fn selected(mut self, chips: &'a [(&'a str, &'a str)]) -> Self {
        self.selected = chips;
        self
    }

    /// Set the first word of each remove button's `aria-label` (default:
    /// `"Remove"`). Use it to translate the widget.
    #[must_use]
    pub const fn remove_label(mut self, text: &'a str) -> Self {
        self.remove_label = text;
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
/// - A hidden `<input>` for storing the selected record's ID (single mode),
///   or a chip list (multiple mode).
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
/// # Multiple (tags) mode
///
/// With [`AutocompleteConfig::multiple`], each pick is a chip with a remove
/// button. Each chip holds one hidden `{value_name}={value}` input, so a
/// `#[serde(default)] Vec<T>` field gets every value. The single hidden
/// input is not rendered. The `<noscript>` fallback is a `<select multiple>`.
///
/// The server renders the chip list `hidden` and the chip inputs
/// `disabled`. The runtime shows and enables them. Thus with JS off, only
/// the `<select>` submits, and no value comes two times.
///
/// Keys (with `autumn-widgets.js`): Enter adds the active option.
/// Backspace in an empty input removes the last chip. ArrowLeft at the
/// start of the input moves to the chips (ArrowRight in RTL). In the chip
/// list, the arrow keys, Home and End move. Delete, Backspace, Enter or
/// Space removes the chip. ArrowRight on the last chip moves back to the
/// input. The chip list is one tab stop.
///
/// Use [`autocomplete_option`] and [`autocomplete_empty_state`] to render
/// option partials returned by your handler. Use
/// [`autocomplete_create_option`] for a "create new" entry.
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
    let value = config.initial_value;
    let seed = Seed {
        query: if config.multiple {
            ""
        } else {
            value.unwrap_or("")
        },
        value,
        chips: config.selected,
    };
    render_autocomplete(id, label, config, &seed)
}

/// Render an [`autocomplete_input`] that is pre-filled from `changeset`.
///
/// The field is [`AutocompleteConfig::value_name`]. In multiple mode, each
/// [`Changeset::field_values`](crate::form::Changeset::field_values) entry
/// is a chip. Else [`Changeset::field_value`](crate::form::Changeset::field_value)
/// fills the hidden input. A missing field falls back to the config.
///
/// The label of a value comes from [`AutocompleteConfig::selected`], then
/// from [`AutocompleteConfig::fallback_options`]. Else the label is the
/// value, which is correct for tags.
///
/// # Example
///
/// ```rust,ignore
/// use autumn_web::widgets::{AutocompleteConfig, autocomplete_input_for};
///
/// // `PostForm { #[serde(default)] tags: Vec<String>, .. }`
/// let config = AutocompleteConfig::new("/tags/autocomplete", "tags")
///     .multiple()
///     .free_text();
/// html! { (autocomplete_input_for(&changeset, "tags", "Tags", &config)) }
/// ```
#[must_use]
pub fn autocomplete_input_for<T: serde::Serialize>(
    changeset: &crate::form::Changeset<T>,
    id: &str,
    label: &str,
    config: &AutocompleteConfig<'_>,
) -> maud::Markup {
    if config.multiple {
        let values = changeset.field_values(config.value_name);
        if values.is_empty() {
            return autocomplete_input(id, label, config);
        }
        let chips: Vec<(&str, &str)> = values
            .iter()
            .map(|value| (value.as_str(), label_of(config, value)))
            .collect();
        let seed = Seed {
            query: "",
            value: None,
            chips: &chips,
        };
        return render_autocomplete(id, label, config, &seed);
    }
    let Some(value) = changeset.field_value(config.value_name) else {
        return autocomplete_input(id, label, config);
    };
    let seed = Seed {
        query: label_of(config, &value),
        value: Some(&value),
        chips: &[],
    };
    render_autocomplete(id, label, config, &seed)
}

/// The label of `value`: from `selected`, then `fallback_options`, else
/// the value.
fn label_of<'v>(config: &AutocompleteConfig<'v>, value: &'v str) -> &'v str {
    config
        .selected
        .iter()
        .chain(config.fallback_options.unwrap_or(&[]))
        .find(|(v, _)| *v == value)
        .map_or(value, |(_, label)| label)
}

/// The values that an autocomplete widget shows first.
struct Seed<'s> {
    /// Text of the visible query input.
    query: &'s str,
    /// Value of the single hidden input (single mode).
    value: Option<&'s str>,
    /// `(value, label)` chips (multiple mode).
    chips: &'s [(&'s str, &'s str)],
}

fn render_autocomplete(
    id: &str,
    label: &str,
    config: &AutocompleteConfig<'_>,
    seed: &Seed<'_>,
) -> maud::Markup {
    let query_id = format!("{id}-query");
    let label_id = format!("{id}-label");
    let options_id = format!("{id}-options");
    let trigger = build_trigger(config.debounce_ms, false);
    let target = format!("#{options_id}");
    let multiple = config.multiple;
    let value_id = (!multiple).then(|| format!("{id}-value"));
    let fallback = config.fallback_options.unwrap_or(&[]);
    let class = if multiple {
        "autumn-autocomplete autumn-autocomplete--multiple"
    } else {
        "autumn-autocomplete"
    };

    // autumn-widgets.js reads these data-* attributes:
    //
    //  data-ac-value-id     — id of the single hidden input (single mode)
    //  data-ac-multiple     — tags mode: chips, one hidden input each
    //  data-ac-value-name   — form field name of the hidden input(s)
    //  data-ac-remove-label — first word of a remove button's aria-label
    //  data-ac-free-text    — typed text is a value (see free_text())
    //  data-ac-query        — marks the visible search input
    //  data-ac-min-length   — minimum characters before htmx fires a request
    //
    // With JS off, only the <noscript><select> submits. The single hidden
    // input has no name, and the chip inputs are disabled, until the
    // runtime runs.

    maud::html! {
        div
            id=(format!("{id}-wrapper"))
            class=(class)
            data-ac-value-id=[value_id.as_deref()]
            data-ac-multiple[multiple]
            data-ac-value-name=(config.value_name)
            data-ac-remove-label=[multiple.then_some(config.remove_label)]
            data-ac-free-text[config.free_text] {
            label id=(label_id) for=(query_id) class="autumn-autocomplete__label" { (label) }
            @if multiple {
                ul
                    id=(format!("{id}-chips"))
                    class="autumn-autocomplete__chips"
                    aria-labelledby=(label_id)
                    data-ac-chips
                    data-autumn-roving="[data-ac-chip-remove]"
                    data-autumn-orientation="horizontal"
                    data-autumn-wrap="false"
                    hidden {
                    @for (value, text) in seed.chips {
                        (chip(config, value, text, true))
                    }
                }
                template data-ac-chip-template { (chip(config, "", "", false)) }
            }
            input
                type="search"
                id=(query_id)
                name=(config.query_param)
                autocomplete="off"
                role="combobox"
                aria-expanded="false"
                aria-autocomplete="list"
                aria-controls=(options_id)
                value=(seed.query)
                placeholder=[config.placeholder]
                class="autumn-autocomplete__input"
                data-ac-query
                data-ac-min-length=(config.min_length)
                hx-get=(config.action)
                hx-trigger=(trigger)
                hx-target=(target)
                hx-indicator=[config.indicator];
            @if let Some(value_id) = &value_id {
                input type="hidden" id=(value_id) value=(seed.value.unwrap_or(""));
            }
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
                @if multiple {
                    select name=(config.value_name) aria-label=(label) multiple {
                        @for (val, lbl) in fallback {
                            option value=(val) selected[seed.chips.iter().any(|(v, _)| v == val)] { (lbl) }
                        }
                        // A chip with no fallback option (a new tag) keeps its value.
                        @for (val, lbl) in seed.chips {
                            @if !fallback.iter().any(|(v, _)| v == val) {
                                option value=(val) selected { (lbl) }
                            }
                        }
                    }
                } @else {
                    select name=(config.value_name) aria-label=(label) {
                        option value="" { "— select —" }
                        @for (val, lbl) in fallback {
                            option value=(val) selected[seed.value == Some(*val)] { (lbl) }
                        }
                    }
                }
            }
        }
    }
}

/// One chip: its label, its hidden input and its remove button.
///
/// A server chip's input is `disabled` until the runtime runs. A template
/// chip is a clone source, so its input is live.
fn chip(config: &AutocompleteConfig<'_>, value: &str, label: &str, server: bool) -> maud::Markup {
    let remove = if label.is_empty() {
        config.remove_label.to_owned()
    } else {
        format!("{} {label}", config.remove_label)
    };
    maud::html! {
        li class="autumn-chip" data-ac-chip {
            span class="autumn-chip__label" data-ac-chip-label { (label) }
            input type="hidden" name=(config.value_name) value=(value) disabled[server];
            button
                type="button"
                class="autumn-chip__remove"
                tabindex="-1"
                aria-label=(remove)
                data-ac-chip-remove {
                span aria-hidden="true" { "×" }
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

/// Render a "create new" option partial for tags mode (issue #3410).
///
/// Return it from your handler, after the matches, when no record has the
/// typed text. `value` is the new value. `label` is the visible text, for
/// example `Create “zig”`. A pick adds a chip that shows `value`, not
/// `label`. Check the new value on the server, as for all input.
///
/// # Example response fragment
///
/// ```rust,ignore
/// use autumn_web::widgets::{autocomplete_create_option, autocomplete_option};
///
/// html! {
///     @for tag in &tags { (autocomplete_option(&tag.name, &tag.name)) }
///     @if !tags.iter().any(|t| t.name == q) {
///         (autocomplete_create_option(q, &format!("Create “{q}”")))
///     }
/// }
/// ```
#[must_use]
pub fn autocomplete_create_option(value: &str, label: &str) -> maud::Markup {
    maud::html! {
        div
            role="option"
            tabindex="-1"
            class="autumn-autocomplete__option autumn-autocomplete__option--create"
            data-value=(value)
            data-label=(value)
            data-ac-create {
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

    // ── multiple mode (#3410) ──────────────────────────────────────────

    /// The markup of the first element that has `marker`, up to its `>`.
    fn tag_with<'h>(html: &'h str, marker: &str) -> &'h str {
        let at = html
            .find(marker)
            .unwrap_or_else(|| panic!("no {marker}: {html}"));
        let start = html[..=at].rfind('<').expect("tag start");
        let end = at + html[at..].find('>').expect("tag end");
        &html[start..=end]
    }

    /// The `<li>` chips of the chip list. The `<template>` is not a chip.
    fn chips(html: &str) -> Vec<&str> {
        let list = html
            .split(r#"data-ac-chips"#)
            .nth(1)
            .and_then(|rest| rest.split("</ul>").next())
            .expect("chip list");
        list.split("<li").skip(1).collect()
    }

    const PICKED: &[(&str, &str)] = &[("42", "rust"), ("7", "web")];

    #[test]
    fn multiple_defaults_off() {
        let c = AutocompleteConfig::new("/ac", "tags");
        assert!(!c.multiple);
        assert!(c.selected.is_empty());
        assert_eq!(c.remove_label, "Remove");
    }

    #[test]
    fn single_mode_has_no_chip_markup() {
        let config = AutocompleteConfig::new("/ac", "tag_id");
        let html = autocomplete_input("x", "Tag", &config).into_string();
        assert!(!html.contains("data-ac-multiple"), "{html}");
        assert!(!html.contains("data-ac-chips"), "{html}");
        assert!(!html.contains("<template"), "{html}");
        assert!(!html.contains("multiple"), "{html}");
    }

    #[test]
    fn multiple_wrapper_has_runtime_data_attributes() {
        let config = AutocompleteConfig::new("/ac", "tags").multiple();
        let html = autocomplete_input("x", "Tags", &config).into_string();
        let wrapper = tag_with(&html, r#"id="x-wrapper""#);
        assert!(wrapper.contains("data-ac-multiple"), "{wrapper}");
        assert!(
            wrapper.contains(r#"data-ac-value-name="tags""#),
            "{wrapper}"
        );
        assert!(
            wrapper.contains(r#"data-ac-remove-label="Remove""#),
            "{wrapper}"
        );
        // The single-value hidden field is not rendered.
        assert!(!wrapper.contains("data-ac-value-id"), "{wrapper}");
        assert!(!html.contains(r#"id="x-value""#), "{html}");
    }

    #[test]
    fn multiple_chip_list_is_a_roving_list_named_by_the_label() {
        let config = AutocompleteConfig::new("/ac", "tags").multiple();
        let html = autocomplete_input("x", "Tags", &config).into_string();
        assert!(
            html.contains(r#"<label id="x-label" for="x-query""#),
            "{html}"
        );
        let list = tag_with(&html, "data-ac-chips");
        assert!(list.starts_with("<ul"), "{list}");
        assert!(list.contains(r#"id="x-chips""#), "{list}");
        assert!(list.contains(r#"aria-labelledby="x-label""#), "{list}");
        assert!(
            list.contains(r#"data-autumn-roving="[data-ac-chip-remove]""#),
            "{list}"
        );
        assert!(
            list.contains(r#"data-autumn-orientation="horizontal""#),
            "{list}"
        );
        assert!(list.contains(r#"data-autumn-wrap="false""#), "{list}");
        // Hidden until the runtime runs: with JS off, the select is the control.
        assert!(list.contains(" hidden"), "{list}");
    }

    #[test]
    fn multiple_renders_a_chip_for_each_selected_pair() {
        let config = AutocompleteConfig::new("/ac", "tags")
            .multiple()
            .selected(PICKED);
        let html = autocomplete_input("x", "Tags", &config).into_string();
        let chips = chips(&html);
        assert_eq!(chips.len(), 2, "{html}");
        assert!(chips[0].contains(">rust<"), "{}", chips[0]);
        assert!(chips[1].contains(">web<"), "{}", chips[1]);
        assert!(chips[0].contains("data-ac-chip"), "{}", chips[0]);
        assert!(chips[0].contains(r#"class="autumn-chip""#), "{}", chips[0]);
    }

    #[test]
    fn each_chip_has_one_named_hidden_input_disabled_until_the_runtime_runs() {
        let config = AutocompleteConfig::new("/ac", "tags")
            .multiple()
            .selected(PICKED);
        let html = autocomplete_input("x", "Tags", &config).into_string();
        for (chip, value) in chips(&html).iter().zip(["42", "7"]) {
            let input = tag_with(chip, r#"type="hidden""#);
            assert!(input.contains(r#"name="tags""#), "{input}");
            assert!(input.contains(&format!(r#"value="{value}""#)), "{input}");
            // With JS off the <noscript> select submits. Not both.
            assert!(input.contains("disabled"), "{input}");
            assert_eq!(chip.matches(r#"type="hidden""#).count(), 1, "{chip}");
        }
    }

    #[test]
    fn each_chip_has_a_labelled_remove_button() {
        let config = AutocompleteConfig::new("/ac", "tags")
            .multiple()
            .selected(PICKED)
            .remove_label("Delete");
        let html = autocomplete_input("x", "Tags", &config).into_string();
        let chip = chips(&html)[0];
        let button = tag_with(chip, "data-ac-chip-remove");
        assert!(button.starts_with("<button"), "{button}");
        assert!(button.contains(r#"type="button""#), "{button}");
        assert!(button.contains(r#"aria-label="Delete rust""#), "{button}");
        assert!(button.contains(r#"tabindex="-1""#), "{button}");
        assert!(chip.contains(r#"aria-hidden="true""#), "{chip}");
    }

    #[test]
    fn chip_labels_and_values_are_escaped() {
        let pairs: &[(&str, &str)] = &[(r#"a"b"#, "<script>x</script>")];
        let config = AutocompleteConfig::new("/ac", "tags")
            .multiple()
            .selected(pairs);
        let html = autocomplete_input("x", "Tags", &config).into_string();
        assert!(!html.contains("<script>"), "{html}");
        assert!(html.contains("&lt;script&gt;x&lt;/script&gt;"), "{html}");
        assert!(html.contains(r#"value="a&quot;b""#), "{html}");
    }

    #[test]
    fn multiple_has_an_empty_chip_template_for_the_runtime() {
        let config = AutocompleteConfig::new("/ac", "tags")
            .multiple()
            .selected(PICKED);
        let html = autocomplete_input("x", "Tags", &config).into_string();
        let template = html
            .split("<template data-ac-chip-template>")
            .nth(1)
            .and_then(|rest| rest.split("</template>").next())
            .expect("chip template");
        assert!(template.contains("data-ac-chip"), "{template}");
        assert!(template.contains("data-ac-chip-label"), "{template}");
        assert!(template.contains(r#"name="tags""#), "{template}");
        assert!(template.contains("data-ac-chip-remove"), "{template}");
        assert!(!template.contains("rust"), "{template}");
        // A clone is live at once, so the template input is not disabled.
        assert!(!template.contains("disabled"), "{template}");
    }

    #[test]
    fn multiple_noscript_is_a_select_multiple_with_the_selection() {
        let opts: &[(&str, &str)] = &[("42", "rust"), ("7", "web"), ("13", "axum")];
        let config = AutocompleteConfig::new("/ac", "tags")
            .multiple()
            .fallback_options(opts)
            .selected(&[("13", "axum"), ("42", "rust")]);
        let html = autocomplete_input("x", "Tags", &config).into_string();
        let noscript = html
            .split("<noscript>")
            .nth(1)
            .and_then(|rest| rest.split("</noscript>").next())
            .expect("noscript");
        let select = tag_with(noscript, "<select");
        assert!(select.contains(r#"name="tags""#), "{select}");
        assert!(select.contains("multiple"), "{select}");
        assert!(noscript.contains(r#"value="42" selected"#), "{noscript}");
        assert!(noscript.contains(r#"value="13" selected"#), "{noscript}");
        assert!(!noscript.contains(r#"value="7" selected"#), "{noscript}");
        // No "— select —" placeholder: an empty multi-select posts nothing.
        assert!(!noscript.contains(r#"value="""#), "{noscript}");
    }

    #[test]
    fn multiple_noscript_keeps_a_selected_value_with_no_fallback_option() {
        // A created tag is not in `fallback_options`. A no-JS resubmit
        // must keep it.
        let opts: &[(&str, &str)] = &[("rust", "rust")];
        let config = AutocompleteConfig::new("/ac", "tags")
            .multiple()
            .fallback_options(opts)
            .selected(&[("brand-new", "brand-new")]);
        let html = autocomplete_input("x", "Tags", &config).into_string();
        assert!(
            html.contains(r#"<option value="brand-new" selected>brand-new</option>"#),
            "{html}"
        );
        assert!(!html.contains(r#"value="rust" selected"#), "{html}");
    }

    #[test]
    fn multiple_free_text_sets_its_data_attribute() {
        let config = AutocompleteConfig::new("/ac", "tags")
            .multiple()
            .free_text();
        let html = autocomplete_input("x", "Tags", &config).into_string();
        let wrapper = tag_with(&html, r#"id="x-wrapper""#);
        assert!(wrapper.contains("data-ac-free-text"), "{wrapper}");
    }

    #[test]
    fn multiple_query_input_is_still_an_apg_combobox() {
        let config = AutocompleteConfig::new("/ac", "tags").multiple();
        let html = autocomplete_input("x", "Tags", &config).into_string();
        let input = tag_with(&html, r#"id="x-query""#);
        assert!(input.contains(r#"role="combobox""#), "{input}");
        assert!(input.contains(r#"aria-controls="x-options""#), "{input}");
        assert!(input.contains(r#"value="""#), "{input}");
        assert!(html.contains(r##"hx-target="#x-options""##), "{html}");
    }

    // ── autocomplete_input_for (#3410) ─────────────────────────────────

    #[derive(serde::Serialize)]
    struct Post {
        tags: Vec<String>,
        tag_ids: Vec<i64>,
        category_id: Option<i64>,
    }

    fn post() -> crate::form::Changeset<Post> {
        crate::form::Changeset::new(Post {
            tags: vec!["rust".to_owned(), "new-tag".to_owned()],
            tag_ids: vec![13, 42],
            category_id: Some(7),
        })
    }

    #[test]
    fn input_for_prefills_chips_from_a_sequence_field() {
        let config = AutocompleteConfig::new("/ac", "tags").multiple();
        let html = autocomplete_input_for(&post(), "x", "Tags", &config).into_string();
        let chips = chips(&html);
        assert_eq!(chips.len(), 2, "{html}");
        // No fallback label: the value is the label (tags mode).
        assert!(chips[0].contains(">rust<"), "{}", chips[0]);
        assert!(chips[0].contains(r#"value="rust""#), "{}", chips[0]);
        assert!(chips[1].contains(">new-tag<"), "{}", chips[1]);
    }

    #[test]
    fn input_for_finds_chip_labels_in_the_fallback_options() {
        let opts: &[(&str, &str)] = &[("42", "rust"), ("13", "axum")];
        let config = AutocompleteConfig::new("/ac", "tag_ids")
            .multiple()
            .fallback_options(opts);
        let html = autocomplete_input_for(&post(), "x", "Tags", &config).into_string();
        let chips = chips(&html);
        assert_eq!(chips.len(), 2, "{html}");
        assert!(chips[0].contains(">axum<"), "{}", chips[0]);
        assert!(chips[0].contains(r#"value="13""#), "{}", chips[0]);
        assert!(chips[1].contains(">rust<"), "{}", chips[1]);
        assert!(html.contains(r#"value="42" selected"#), "{html}");
    }

    #[test]
    fn input_for_finds_chip_labels_in_the_selected_pairs() {
        let config = AutocompleteConfig::new("/ac", "tag_ids")
            .multiple()
            .selected(&[("42", "rust")]);
        let html = autocomplete_input_for(&post(), "x", "Tags", &config).into_string();
        let chips = chips(&html);
        // The changeset is the source of truth: 13 and 42, not only 42.
        assert_eq!(chips.len(), 2, "{html}");
        assert!(chips[0].contains(">13<"), "{}", chips[0]);
        assert!(chips[1].contains(">rust<"), "{}", chips[1]);
    }

    #[test]
    fn input_for_an_empty_field_renders_no_chips() {
        let config = AutocompleteConfig::new("/ac", "missing").multiple();
        let html = autocomplete_input_for(&post(), "x", "Tags", &config).into_string();
        assert!(chips(&html).is_empty(), "{html}");
    }

    #[test]
    fn input_for_single_mode_prefills_the_value_and_its_label() {
        let opts: &[(&str, &str)] = &[("7", "News"), ("8", "Blog")];
        let config = AutocompleteConfig::new("/ac", "category_id").fallback_options(opts);
        let html = autocomplete_input_for(&post(), "x", "Category", &config).into_string();
        let hidden = tag_with(&html, r#"type="hidden" id="x-value""#);
        assert!(hidden.contains(r#"value="7""#), "{hidden}");
        let query = tag_with(&html, r#"id="x-query""#);
        assert!(query.contains(r#"value="News""#), "{query}");
        assert!(html.contains(r#"value="7" selected"#), "{html}");
    }

    // ── autocomplete_create_option (#3410) ─────────────────────────────

    #[test]
    fn create_option_is_an_option_with_the_new_value_as_its_label() {
        let html = autocomplete_create_option("go", "Create “go”").into_string();
        assert!(html.contains(r#"role="option""#), "{html}");
        assert!(html.contains(r#"tabindex="-1""#), "{html}");
        assert!(html.contains(r#"data-value="go""#), "{html}");
        assert!(html.contains(r#"data-label="go""#), "{html}");
        assert!(html.contains("data-ac-create"), "{html}");
        assert!(
            html.contains("autumn-autocomplete__option--create"),
            "{html}"
        );
        assert!(html.contains("Create “go”"), "{html}");
    }

    #[test]
    fn create_option_escapes_its_value() {
        let html = autocomplete_create_option(r#""><b>"#, "x").into_string();
        assert!(!html.contains("<b>"), "{html}");
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
