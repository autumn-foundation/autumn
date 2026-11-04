//! Tests for the `accordion` and `collapsible` widgets (issue #3401).
//!
//! Run with `cargo test --test integration_tests widgets_accordion`.

#![allow(clippy::must_use_candidate)]

use autumn_web::widgets::{
    AccordionConfig, CollapsibleConfig, HeadingLevel, accordion, collapsible, collapsible_with,
};
use maud::html;

fn items() -> Vec<(&'static str, maud::Markup)> {
    vec![
        ("Shipping", html! { p { "Ships in 2 days." } }),
        ("Returns", html! { p { "30 day returns." } }),
        ("Support", html! { p { "Mail us." } }),
    ]
}

/// The opening tags of every `<details ...>` in `html`.
fn details_tags(html: &str) -> Vec<&str> {
    html.match_indices("<details")
        .map(|(i, _)| &html[i..i + html[i..].find('>').expect("tag end")])
        .collect()
}

fn is_open(tag: &str) -> bool {
    tag.split_whitespace()
        .any(|w| w == "open" || w == r#"open="""#)
}

// ── accordion: exclusive vs multi ───────────────────────────────────────

#[test]
fn exclusive_items_share_one_name() {
    let html = accordion(&items(), &AccordionConfig::new("faq")).into_string();
    let tags = details_tags(&html);
    assert_eq!(tags.len(), 3, "{html}");
    for tag in tags {
        assert!(tag.contains(r#"name="faq""#), "{tag}");
    }
}

#[test]
fn multi_open_items_have_no_name() {
    let config = AccordionConfig::new("faq").multiple();
    let html = accordion(&items(), &config).into_string();
    let tags = details_tags(&html);
    assert_eq!(tags.len(), 3, "{html}");
    for tag in tags {
        assert!(!tag.contains("name="), "{tag}");
    }
}

#[test]
fn root_has_id_and_class() {
    let html = accordion(&items(), &AccordionConfig::new("faq")).into_string();
    assert!(
        html.starts_with(r#"<div id="faq" class="autumn-accordion""#),
        "{html}"
    );
}

#[test]
fn each_item_has_summary_and_body() {
    let html = accordion(&items(), &AccordionConfig::new("faq")).into_string();
    assert_eq!(html.matches("<summary").count(), 3, "{html}");
    assert!(html.contains("autumn-accordion__item"), "{html}");
    assert!(html.contains("autumn-accordion__summary"), "{html}");
    assert!(html.contains("autumn-accordion__body"), "{html}");
    assert!(html.contains("<p>Ships in 2 days.</p>"), "{html}");
}

#[test]
fn empty_items_render_an_empty_root() {
    let html = accordion(&[], &AccordionConfig::new("faq")).into_string();
    assert!(details_tags(&html).is_empty(), "{html}");
    assert!(html.ends_with("></div>"), "{html}");
    assert!(
        html.starts_with(r#"<div id="faq" class="autumn-accordion""#),
        "{html}"
    );
}

// ── default open ────────────────────────────────────────────────────────

#[test]
fn nothing_is_open_by_default() {
    let html = accordion(&items(), &AccordionConfig::new("faq")).into_string();
    assert!(details_tags(&html).iter().all(|t| !is_open(t)), "{html}");
}

#[test]
fn open_marks_one_item() {
    let config = AccordionConfig::new("faq").open(1);
    let html = accordion(&items(), &config).into_string();
    let open: Vec<bool> = details_tags(&html).iter().map(|t| is_open(t)).collect();
    assert_eq!(open, [false, true, false], "{html}");
}

#[test]
fn exclusive_keeps_only_the_first_open_item() {
    let config = AccordionConfig::new("faq").open(2).open(0);
    let html = accordion(&items(), &config).into_string();
    let open: Vec<bool> = details_tags(&html).iter().map(|t| is_open(t)).collect();
    assert_eq!(open, [true, false, false], "{html}");
}

#[test]
fn exclusive_open_is_the_first_in_page_order() {
    let open = |config: AccordionConfig<'_>| -> Vec<bool> {
        let html = accordion(&items(), &config).into_string();
        details_tags(&html).iter().map(|t| is_open(t)).collect()
    };
    let faq = || AccordionConfig::new("faq");
    assert_eq!(open(faq().open(2).open(1)), [false, true, false]);
    assert_eq!(open(faq().open(9).open(1)), [false, true, false]);
    assert_eq!(open(faq().open(1).open(1)), [false, true, false]);
}

#[test]
fn multi_open_keeps_every_open_item() {
    let config = AccordionConfig::new("faq").multiple().open(0).open(2);
    let html = accordion(&items(), &config).into_string();
    let open: Vec<bool> = details_tags(&html).iter().map(|t| is_open(t)).collect();
    assert_eq!(open, [true, false, true], "{html}");
}

#[test]
fn an_out_of_range_open_index_is_ignored() {
    let config = AccordionConfig::new("faq").open(9);
    let html = accordion(&items(), &config).into_string();
    assert!(details_tags(&html).iter().all(|t| !is_open(t)), "{html}");
}

// ── heading level ───────────────────────────────────────────────────────

#[test]
fn heading_defaults_to_h3_inside_the_summary() {
    let html = accordion(&items(), &AccordionConfig::new("faq")).into_string();
    assert!(
        html.contains(r#"<summary class="autumn-accordion__summary"><h3 class="autumn-accordion__heading">Shipping</h3>"#),
        "{html}"
    );
}

#[test]
fn heading_level_is_configurable() {
    let config = AccordionConfig::new("faq").level(HeadingLevel::H4);
    let html = accordion(&items(), &config).into_string();
    assert!(
        html.contains(r#"<h4 class="autumn-accordion__heading">"#),
        "{html}"
    );
    assert!(!html.contains("<h3"), "{html}");
}

// ── escaping and passthrough ────────────────────────────────────────────

#[test]
fn titles_are_escaped_and_bodies_are_markup() {
    let items = [("<script>alert(1)</script>", html! { b { "bold" } })];
    let html = accordion(&items, &AccordionConfig::new("faq")).into_string();
    assert!(!html.contains("<script>"), "{html}");
    assert!(html.contains("&lt;script&gt;"), "{html}");
    assert!(html.contains("<b>bold</b>"), "{html}");
}

#[test]
fn name_is_escaped() {
    let html = accordion(&items(), &AccordionConfig::new(r#"a"b"#)).into_string();
    assert!(!html.contains(r#"name="a"b""#), "{html}");
    assert!(html.contains("a&quot;b"), "{html}");
}

#[test]
fn attrs_pass_through_to_the_root() {
    let config = AccordionConfig::new("faq")
        .class("wide")
        .attrs(&[("data-test-id", "faq"), ("hx-get", "/x")]);
    let html = accordion(&items(), &config).into_string();
    assert!(html.contains(r#"class="autumn-accordion wide""#), "{html}");
    assert!(html.contains(r#"data-test-id="faq""#), "{html}");
    assert!(html.contains(r#"hx-get="/x""#), "{html}");
}

#[test]
fn attrs_drop_handlers_style_and_owned_names() {
    let config = AccordionConfig::new("faq").attrs(&[
        ("onclick", "x()"),
        ("hx-on:click", "x()"),
        ("style", "color:red"),
        ("id", "evil"),
        ("class", "evil"),
    ]);
    let html = accordion(&items(), &config).into_string();
    assert!(!html.contains("onclick"), "{html}");
    assert!(!html.contains("hx-on"), "{html}");
    assert!(!html.contains("style="), "{html}");
    assert!(!html.contains("evil"), "{html}");
    assert!(
        html.starts_with(r#"<div id="faq" class="autumn-accordion""#),
        "{html}"
    );
}

#[test]
fn no_script_and_no_inline_style() {
    let config = AccordionConfig::new("faq").open(0);
    let html = accordion(&items(), &config).into_string();
    assert!(!html.contains("<script"), "{html}");
    assert!(!html.contains("style="), "{html}");
}

// ── collapsible ─────────────────────────────────────────────────────────

#[test]
fn collapsible_is_one_closed_details() {
    let html = collapsible("More", &html! { p { "Body" } }).into_string();
    assert_eq!(
        html,
        r#"<details class="autumn-collapsible"><summary class="autumn-collapsible__summary">More</summary><div class="autumn-collapsible__body"><p>Body</p></div></details>"#
    );
}

#[test]
fn collapsible_open_and_class() {
    let config = CollapsibleConfig::new().open(true).class("card");
    let html = collapsible_with("More", &html! { "x" }, &config).into_string();
    let tags = details_tags(&html);
    assert!(is_open(tags[0]), "{html}");
    assert!(
        tags[0].contains(r#"class="autumn-collapsible card""#),
        "{html}"
    );
}

#[test]
fn collapsible_escapes_the_summary() {
    let html = collapsible("<i>x</i>", &html! { "y" }).into_string();
    assert!(html.contains("&lt;i&gt;x&lt;/i&gt;"), "{html}");
}

#[test]
fn collapsible_attrs_are_cleaned() {
    let config = CollapsibleConfig::new().attrs(&[
        ("data-test-id", "c"),
        ("ontoggle", "x()"),
        ("name", "grp"),
    ]);
    let html = collapsible_with("More", &html! { "x" }, &config).into_string();
    assert!(html.contains(r#"data-test-id="c""#), "{html}");
    assert!(
        !html.contains("onToggle") && !html.contains("ontoggle"),
        "{html}"
    );
    // A collapsible is a single disclosure: the app may group it itself.
    assert!(html.contains(r#"name="grp""#), "{html}");
}

#[test]
fn collapsible_open_belongs_to_the_config() {
    let attrs = [("open", "")];
    let closed = CollapsibleConfig::new().attrs(&attrs);
    let html = collapsible_with("More", &html! { "x" }, &closed).into_string();
    assert!(!is_open(details_tags(&html)[0]), "open(false) wins: {html}");

    let open = CollapsibleConfig::new().open(true).attrs(&attrs);
    let html = collapsible_with("More", &html! { "x" }, &open).into_string();
    assert_eq!(
        html.matches("open").count(),
        1,
        "one open attribute: {html}"
    );
    assert!(is_open(details_tags(&html)[0]), "{html}");
}

// ── stylesheet ──────────────────────────────────────────────────────────

fn accordion_css() -> String {
    let css = autumn_web::ui::WIDGETS_COMPONENT_CSS.replace("\r\n", "\n");
    let start = css.find("/* ── Accordion").expect("accordion css section");
    let rest = &css[start + 3..];
    let end = rest.find("\n/* ── ").map_or(css.len(), |e| start + 3 + e);
    css[start..end].to_owned()
}

#[test]
fn stylesheet_backs_every_class() {
    let css = accordion_css();
    for class in [
        ".autumn-accordion",
        ".autumn-accordion__item",
        ".autumn-accordion__summary",
        ".autumn-accordion__heading",
        ".autumn-accordion__body",
        ".autumn-collapsible",
        ".autumn-collapsible__summary",
        ".autumn-collapsible__body",
    ] {
        assert!(css.contains(class), "missing {class}");
    }
}

#[test]
fn stylesheet_uses_tokens_only() {
    let css = accordion_css();
    assert!(!css.contains('#'), "hex color in accordion css");
    assert!(!css.contains("rgb"), "rgb color in accordion css");
    assert!(css.contains("var(--border"), "no token use");
    assert!(css.contains("var(--space-"), "no spacing token use");
}

#[test]
fn stylesheet_respects_reduced_motion() {
    let css = accordion_css();
    // Any motion is opt-in, so reduced motion is the default.
    assert!(
        css.contains("@media (prefers-reduced-motion: no-preference)"),
        "{css}"
    );
    let first_transition = css.find("transition").expect("a transition");
    let guard = css
        .find("@media (prefers-reduced-motion: no-preference)")
        .unwrap();
    assert!(
        guard < first_transition,
        "a transition sits outside the guard"
    );
}

#[test]
fn stylesheet_has_a_visible_focus_ring() {
    let css = accordion_css();
    for class in [".autumn-accordion__summary", ".autumn-collapsible__summary"] {
        // The selector may sit in a list: the rule body follows the next `{`.
        let at = css
            .find(&format!("{class}:focus-visible"))
            .unwrap_or_else(|| panic!("no rule for {class}"));
        let start = at + css[at..].find('{').expect("rule start");
        let block = &css[start..start + css[start..].find('}').expect("rule end")];
        assert!(
            block.contains("outline: 2px solid var(--primary)"),
            "{block}"
        );
    }
}

#[test]
fn every_transition_sits_in_the_motion_guard() {
    let css = accordion_css();
    let guard = css
        .find("@media (prefers-reduced-motion: no-preference)")
        .expect("guard");
    for (at, _) in css.match_indices("transition") {
        assert!(at > guard, "a transition sits before the guard");
    }
}
