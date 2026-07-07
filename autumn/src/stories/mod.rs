//! Widget story gallery: a browsable `/_stories` UI plus a CI anti-rot
//! registry for the built-in maud widgets (issue #1526).
//!
//! Mirrors the mail-preview registry precedent (`crate::mail::MailPreview` /
//! `MailPreviewRegistry`): each story is a zero-arg pure `fn() -> Markup`
//! paired with the source snippet that produced it, collected into a
//! `StoryRegistry` served at `GET /_stories` (grouped index) and
//! `GET /_stories/{slug}` (live render + Source + Rendered HTML tabs).
//!
//! RED phase note: this module currently contains only the failing unit
//! tests below. The GREEN phase implements, above the test module:
//!
//! - `pub const STORIES_PATH: &str = "/_stories";`
//! - `Story { group, name, slug, render: fn() -> maud::Markup, source }`
//!   with `new` (panics on empty slug), accessors, and a `catch_unwind`
//!   `render()` returning `Result<maud::Markup, StoryRenderError>`
//! - `fn slugify(name: &str) -> String`
//! - `StoryRegistry` (`new` panics on duplicate slugs, `stories`, `find`,
//!   `grouped`) and `pub fn builtin() -> StoryRegistry`
//! - `StoryGallery` (`new`, `builtin`, `extend`, `stories`, `routes<S>()`,
//!   `pub(crate) into_registry`)
//! - `StoriesConfig { enabled: bool }` (default **false**) for the
//!   `[stories]` config section
//! - `pub(crate) fn story_router<S>()` plus the pure page renderers
//!   `render_story_index(&StoryRegistry)` and `render_story_detail(&Story)`
//! - `pub use autumn_macros::story;`

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
        let page = render_story_index(&registry).into_string();
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
        let page = render_story_detail(&story).into_string();
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
        let page = render_story_index(&StoryRegistry::default()).into_string();
        assert!(
            page.contains("with_story_gallery"),
            "empty state must explain how to register stories: {page}"
        );
    }
}
