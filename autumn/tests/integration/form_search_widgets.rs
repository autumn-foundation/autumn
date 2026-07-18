//! Tests for active search and autocomplete form primitives (issue #768).
//!
//! Run with `cargo test --test form_search_widgets`.

#![allow(clippy::must_use_candidate)]

#[cfg(feature = "maud")]
mod active_search_tests {
    use autumn_web::widgets::{
        ActiveSearchConfig, AutocompleteConfig, SearchMethod, active_search,
        active_search_empty_state, active_search_input, active_search_results,
        autocomplete_empty_state, autocomplete_input, autocomplete_option,
    };

    // ── active_search_input: HTTP method ──────────────────────────────

    #[test]
    fn active_search_defaults_to_get() {
        let config = ActiveSearchConfig::new("/search", "#results");
        let html = active_search_input("q", "Search", &config).into_string();
        assert!(html.contains(r#"hx-get="/search""#), "{html}");
        assert!(!html.contains("hx-post"), "{html}");
    }

    #[test]
    fn active_search_post_opt_in() {
        let config = ActiveSearchConfig::new("/search", "#results").post();
        let html = active_search_input("q", "Search", &config).into_string();
        assert!(html.contains(r#"hx-post="/search""#), "{html}");
        assert!(!html.contains("hx-get"), "{html}");
    }

    #[test]
    fn active_search_method_enum_default_is_get() {
        assert_eq!(SearchMethod::default(), SearchMethod::Get);
    }

    // ── active_search_input: htmx trigger ────────────────────────────

    #[test]
    fn active_search_emits_hx_trigger() {
        let config = ActiveSearchConfig::new("/search", "#results");
        let html = active_search_input("q", "Search", &config).into_string();
        assert!(html.contains("hx-trigger"), "{html}");
    }

    #[test]
    fn active_search_trigger_has_input_changed_modifier() {
        let config = ActiveSearchConfig::new("/search", "#results");
        let html = active_search_input("q", "Search", &config).into_string();
        assert!(html.contains("input"), "{html}");
        assert!(html.contains("changed"), "{html}");
    }

    #[test]
    fn active_search_default_debounce_is_300ms() {
        let config = ActiveSearchConfig::new("/search", "#results");
        let html = active_search_input("q", "Search", &config).into_string();
        assert!(html.contains("delay:300ms"), "{html}");
    }

    #[test]
    fn active_search_configurable_debounce() {
        let config = ActiveSearchConfig::new("/search", "#results").debounce(500);
        let html = active_search_input("q", "Search", &config).into_string();
        assert!(html.contains("delay:500ms"), "{html}");
        assert!(!html.contains("delay:300ms"), "{html}");
    }

    #[test]
    fn active_search_configurable_min_length() {
        let config = ActiveSearchConfig::new("/search", "#results").min_length(3);
        let html = active_search_input("q", "Search", &config).into_string();
        // min_length is enforced server-side; no filter expression in the trigger
        assert!(html.contains("hx-trigger"), "{html}");
        assert!(!html.contains("this.value.length"), "{html}");
    }

    #[test]
    fn active_search_no_min_length_filter_when_zero() {
        let config = ActiveSearchConfig::new("/search", "#results").min_length(0);
        let html = active_search_input("q", "Search", &config).into_string();
        assert!(!html.contains("this.value.length"), "{html}");
    }

    #[test]
    fn active_search_initial_load_in_trigger() {
        let config = ActiveSearchConfig::new("/search", "#results").initial_load();
        let html = active_search_input("q", "Search", &config).into_string();
        assert!(html.contains(", load"), "{html}");
    }

    #[test]
    fn active_search_no_initial_load_by_default() {
        let config = ActiveSearchConfig::new("/search", "#results");
        let html = active_search_input("q", "Search", &config).into_string();
        // trigger should not contain ", load"
        assert!(!html.contains(", load"), "{html}");
    }

    // ── active_search_input: target & indicator ───────────────────────

    #[test]
    fn active_search_target_selector() {
        let config = ActiveSearchConfig::new("/search", "#my-results");
        let html = active_search_input("q", "Search", &config).into_string();
        assert!(html.contains("hx-target=\"#my-results\""), "{html}");
    }

    #[test]
    fn active_search_indicator_when_configured() {
        let config = ActiveSearchConfig::new("/search", "#results").indicator("#spinner");
        let html = active_search_input("q", "Search", &config).into_string();
        assert!(html.contains("hx-indicator=\"#spinner\""), "{html}");
    }

    #[test]
    fn active_search_no_indicator_by_default() {
        let config = ActiveSearchConfig::new("/search", "#results");
        let html = active_search_input("q", "Search", &config).into_string();
        assert!(!html.contains("hx-indicator"), "{html}");
    }

    // ── active_search_input: accessibility ───────────────────────────

    #[test]
    fn active_search_renders_label() {
        let config = ActiveSearchConfig::new("/search", "#results");
        let html = active_search_input("q", "Search Posts", &config).into_string();
        assert!(html.contains("Search Posts"), "{html}");
        assert!(html.contains("<label"), "{html}");
    }

    #[test]
    fn active_search_label_for_matches_input_id() {
        let config = ActiveSearchConfig::new("/search", "#results");
        let html = active_search_input("my-search", "Search", &config).into_string();
        assert!(html.contains(r#"for="my-search""#), "{html}");
        assert!(html.contains(r#"id="my-search""#), "{html}");
    }

    #[test]
    fn active_search_has_aria_controls_pointing_at_results() {
        let config = ActiveSearchConfig::new("/search", "#my-results");
        let html = active_search_input("q", "Search", &config).into_string();
        assert!(html.contains("aria-controls"), "{html}");
        // aria-controls takes bare ID (no #)
        assert!(html.contains("my-results"), "{html}");
    }

    #[test]
    fn active_search_type_is_search() {
        let config = ActiveSearchConfig::new("/search", "#results");
        let html = active_search_input("q", "Search", &config).into_string();
        assert!(html.contains(r#"type="search""#), "{html}");
    }

    // ── active_search_input: misc ─────────────────────────────────────

    #[test]
    fn active_search_placeholder_when_configured() {
        let config = ActiveSearchConfig::new("/search", "#results").placeholder("Type to search…");
        let html = active_search_input("q", "Search", &config).into_string();
        assert!(html.contains("Type to search"), "{html}");
    }

    #[test]
    fn active_search_no_placeholder_by_default() {
        let config = ActiveSearchConfig::new("/search", "#results");
        let html = active_search_input("q", "Search", &config).into_string();
        assert!(!html.contains("placeholder"), "{html}");
    }

    #[test]
    fn active_search_custom_param_name() {
        let config = ActiveSearchConfig::new("/search", "#results").param_name("query");
        let html = active_search_input("q", "Search", &config).into_string();
        assert!(html.contains(r#"name="query""#), "{html}");
    }

    #[test]
    fn active_search_default_param_name_is_q() {
        let config = ActiveSearchConfig::new("/search", "#results");
        let html = active_search_input("q", "Search", &config).into_string();
        assert!(html.contains(r#"name="q""#), "{html}");
    }

    // ── active_search_results ─────────────────────────────────────────

    #[test]
    fn active_search_results_correct_id() {
        let html = active_search_results("my-results").into_string();
        assert!(html.contains(r#"id="my-results""#), "{html}");
    }

    #[test]
    fn active_search_results_has_role_status() {
        let html = active_search_results("r").into_string();
        assert!(html.contains(r#"role="status""#), "{html}");
    }

    #[test]
    fn active_search_results_has_aria_live_polite() {
        let html = active_search_results("r").into_string();
        assert!(html.contains(r#"aria-live="polite""#), "{html}");
    }

    #[test]
    fn active_search_results_has_aria_atomic() {
        let html = active_search_results("r").into_string();
        assert!(html.contains(r#"aria-atomic="true""#), "{html}");
    }

    // ── active_search (full widget) ───────────────────────────────────

    #[test]
    fn active_search_widget_includes_input_and_results() {
        let config = ActiveSearchConfig::new("/search", "#s-results");
        let html = active_search("s", "Search", &config).into_string();
        assert!(html.contains(r#"type="search""#), "{html}");
        assert!(html.contains(r#"id="s-results""#), "{html}");
    }

    #[test]
    fn active_search_widget_noscript_fallback_present() {
        let config = ActiveSearchConfig::new("/search", "#results");
        let html = active_search("s", "Search", &config).into_string();
        assert!(html.contains("<noscript>"), "{html}");
        assert!(html.contains("<form"), "{html}");
        assert!(html.contains(r#"type="submit""#), "{html}");
    }

    #[test]
    fn active_search_noscript_fallback_get_default() {
        let config = ActiveSearchConfig::new("/search", "#results");
        let html = active_search("s", "Search", &config).into_string();
        assert!(html.contains(r#"method="get""#), "{html}");
    }

    #[test]
    fn active_search_noscript_fallback_post_when_configured() {
        let config = ActiveSearchConfig::new("/search", "#results").post();
        let html = active_search("s", "Search", &config).into_string();
        assert!(html.contains(r#"method="post""#), "{html}");
    }

    #[test]
    fn active_search_noscript_form_action_matches_handler() {
        let config = ActiveSearchConfig::new("/users/search", "#results");
        let html = active_search("s", "Search", &config).into_string();
        assert!(html.contains(r#"action="/users/search""#), "{html}");
    }

    // ── autocomplete_input ────────────────────────────────────────────

    #[test]
    fn autocomplete_has_visible_search_input() {
        let config = AutocompleteConfig::new("/autocomplete", "tag_id");
        let html = autocomplete_input("tag", "Tag", &config).into_string();
        assert!(html.contains(r#"type="search""#), "{html}");
        // visible input uses query_param (default "q") so htmx sends ?q=...
        assert!(html.contains(r#"name="q""#), "{html}");
    }

    #[test]
    fn autocomplete_has_hidden_value_field() {
        let config = AutocompleteConfig::new("/autocomplete", "tag_id");
        let html = autocomplete_input("tag", "Tag", &config).into_string();
        assert!(html.contains(r#"type="hidden""#), "{html}");
        // Hidden input has no name attribute in HTML; autumn-widgets.js sets
        // it on first interaction to prevent duplicate submission in no-JS mode.
        assert!(html.contains(r#"id="tag-value""#), "{html}");
    }

    #[test]
    fn autocomplete_wrapper_data_attributes_for_runtime() {
        let config = AutocompleteConfig::new("/autocomplete", "tag_id");
        let html = autocomplete_input("tag", "Tag", &config).into_string();
        assert!(html.contains(r#"data-ac-value-id="tag-value""#), "{html}");
        assert!(html.contains(r#"data-ac-value-name="tag_id""#), "{html}");
        assert!(html.contains("data-ac-query"), "{html}");
        assert!(html.contains("data-ac-min-length"), "{html}");
    }

    #[test]
    fn autocomplete_hidden_field_initial_value_is_empty() {
        let config = AutocompleteConfig::new("/autocomplete", "tag_id");
        let html = autocomplete_input("tag", "Tag", &config).into_string();
        assert!(html.contains(r#"type="hidden""#), "{html}");
        assert!(html.contains(r#"value="""#), "{html}");
    }

    #[test]
    fn autocomplete_seeds_initial_value_into_visible_and_hidden_inputs() {
        // Re-rendering the widget after a validation error (e.g. a free-text
        // tag field on a rejected changeset submission) must not silently
        // drop what the user already typed.
        let config = AutocompleteConfig::new("/autocomplete", "tag_id")
            .free_text()
            .initial_value("my-tag");
        let html = autocomplete_input("tag", "Tag", &config).into_string();
        assert_eq!(
            html.matches(r#"value="my-tag""#).count(),
            2,
            "both the visible query input and the hidden field must carry \
             the previously submitted value: {html}"
        );
        assert!(
            html.contains(r#"id="tag-value" value="my-tag""#),
            "hidden field must carry the previously submitted value: {html}"
        );
    }

    #[test]
    fn autocomplete_has_listbox_container() {
        let config = AutocompleteConfig::new("/autocomplete", "tag_id");
        let html = autocomplete_input("tag", "Tag", &config).into_string();
        assert!(html.contains(r#"role="listbox""#), "{html}");
    }

    #[test]
    fn autocomplete_visible_input_has_combobox_role() {
        let config = AutocompleteConfig::new("/autocomplete", "tag_id");
        let html = autocomplete_input("tag", "Tag", &config).into_string();
        assert!(html.contains(r#"role="combobox""#), "{html}");
    }

    #[test]
    fn autocomplete_aria_expanded_false_initially() {
        let config = AutocompleteConfig::new("/autocomplete", "tag_id");
        let html = autocomplete_input("tag", "Tag", &config).into_string();
        assert!(html.contains(r#"aria-expanded="false""#), "{html}");
    }

    #[test]
    fn autocomplete_aria_autocomplete_attribute() {
        let config = AutocompleteConfig::new("/autocomplete", "tag_id");
        let html = autocomplete_input("tag", "Tag", &config).into_string();
        assert!(html.contains(r#"aria-autocomplete="list""#), "{html}");
    }

    #[test]
    fn autocomplete_has_aria_controls() {
        let config = AutocompleteConfig::new("/autocomplete", "tag_id");
        let html = autocomplete_input("tag", "Tag", &config).into_string();
        assert!(html.contains("aria-controls"), "{html}");
    }

    #[test]
    fn autocomplete_renders_label() {
        let config = AutocompleteConfig::new("/autocomplete", "tag_id");
        let html = autocomplete_input("tag", "Tag Name", &config).into_string();
        assert!(html.contains("Tag Name"), "{html}");
        assert!(html.contains("<label"), "{html}");
    }

    #[test]
    fn autocomplete_label_for_matches_visible_input_id() {
        let config = AutocompleteConfig::new("/autocomplete", "tag_id");
        let html = autocomplete_input("tag", "Tag", &config).into_string();
        assert!(html.contains(r#"for="tag-query""#), "{html}");
        assert!(html.contains(r#"id="tag-query""#), "{html}");
    }

    #[test]
    fn autocomplete_has_noscript_fallback() {
        let config = AutocompleteConfig::new("/autocomplete", "tag_id");
        let html = autocomplete_input("tag", "Tag", &config).into_string();
        assert!(html.contains("<noscript>"), "{html}");
        assert!(html.contains("<select"), "{html}");
    }

    #[test]
    fn autocomplete_noscript_select_has_value_name() {
        let config = AutocompleteConfig::new("/autocomplete", "tag_id");
        let html = autocomplete_input("tag", "Tag", &config).into_string();
        // The noscript select should use the value field name
        assert!(html.contains(r#"name="tag_id""#), "{html}");
    }

    #[test]
    fn autocomplete_has_htmx_get_on_visible_input() {
        let config = AutocompleteConfig::new("/autocomplete", "tag_id");
        let html = autocomplete_input("tag", "Tag", &config).into_string();
        assert!(html.contains(r#"hx-get="/autocomplete""#), "{html}");
    }

    #[test]
    fn autocomplete_has_hx_trigger_with_debounce() {
        let config = AutocompleteConfig::new("/autocomplete", "tag_id");
        let html = autocomplete_input("tag", "Tag", &config).into_string();
        assert!(html.contains("hx-trigger"), "{html}");
        assert!(html.contains("delay:300ms"), "{html}");
    }

    #[test]
    fn autocomplete_configurable_debounce() {
        let config = AutocompleteConfig::new("/autocomplete", "tag_id").debounce(600);
        let html = autocomplete_input("tag", "Tag", &config).into_string();
        assert!(html.contains("delay:600ms"), "{html}");
    }

    #[test]
    fn autocomplete_configurable_min_length() {
        let config = AutocompleteConfig::new("/autocomplete", "tag_id").min_length(2);
        let html = autocomplete_input("tag", "Tag", &config).into_string();
        // min_length is enforced server-side; no filter expression in the trigger
        assert!(html.contains("hx-trigger"), "{html}");
        assert!(!html.contains("this.value.length"), "{html}");
    }

    #[test]
    fn autocomplete_indicator_when_configured() {
        let config = AutocompleteConfig::new("/autocomplete", "tag_id").indicator("#loader");
        let html = autocomplete_input("tag", "Tag", &config).into_string();
        assert!(html.contains("hx-indicator=\"#loader\""), "{html}");
    }

    #[test]
    fn autocomplete_no_indicator_by_default() {
        let config = AutocompleteConfig::new("/autocomplete", "tag_id");
        let html = autocomplete_input("tag", "Tag", &config).into_string();
        assert!(!html.contains("hx-indicator"), "{html}");
    }

    #[test]
    fn autocomplete_listbox_has_aria_live() {
        let config = AutocompleteConfig::new("/autocomplete", "tag_id");
        let html = autocomplete_input("tag", "Tag", &config).into_string();
        assert!(html.contains("aria-live"), "{html}");
    }

    // ── autocomplete_option ───────────────────────────────────────────

    #[test]
    fn autocomplete_option_renders_label() {
        let html = autocomplete_option("42", "My Tag").into_string();
        assert!(html.contains("My Tag"), "{html}");
    }

    #[test]
    fn autocomplete_option_has_data_value() {
        let html = autocomplete_option("42", "My Tag").into_string();
        assert!(html.contains(r#"data-value="42""#), "{html}");
    }

    #[test]
    fn autocomplete_option_has_role_option() {
        let html = autocomplete_option("1", "Option").into_string();
        assert!(html.contains(r#"role="option""#), "{html}");
    }

    #[test]
    fn autocomplete_option_is_keyboard_focusable() {
        let html = autocomplete_option("1", "Option").into_string();
        assert!(html.contains("tabindex"), "{html}");
    }

    // ── autocomplete_empty_state ──────────────────────────────────────

    #[test]
    fn autocomplete_empty_state_renders_message() {
        let html = autocomplete_empty_state("No results found").into_string();
        assert!(html.contains("No results found"), "{html}");
    }

    #[test]
    fn autocomplete_empty_state_is_announced_to_screen_readers() {
        let html = autocomplete_empty_state("No results").into_string();
        assert!(
            html.contains(r#"role="status""#) || html.contains("aria-live"),
            "{html}"
        );
    }

    // ── active_search_empty_state ─────────────────────────────────────

    #[test]
    fn search_empty_state_renders_message() {
        let html = active_search_empty_state("No matching posts").into_string();
        assert!(html.contains("No matching posts"), "{html}");
    }

    #[test]
    fn search_empty_state_is_announced_to_screen_readers() {
        let html = active_search_empty_state("Nothing found").into_string();
        assert!(
            html.contains(r#"role="status""#) || html.contains("aria-live"),
            "{html}"
        );
    }
}
