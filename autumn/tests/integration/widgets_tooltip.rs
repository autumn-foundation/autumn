//! Render tests for the `tooltip` widget (issue #3404).
//!
//! Run with `cargo test --test integration_tests widgets_tooltip`.

#![cfg(feature = "maud")]

use autumn_web::widgets::{TooltipConfig, TooltipPlacement, tooltip, tooltip_with};
use maud::html;

fn copy_button(tip: &str) -> maud::Markup {
    html! { button type="button" aria-describedby=(tip) { "Copy" } }
}

#[test]
fn tooltip_links_the_text_to_the_trigger() {
    let out = tooltip("copy-tip", "Copy the link", copy_button).into_string();
    assert!(out.contains(r#"aria-describedby="copy-tip""#), "{out}");
    assert!(
        out.contains(r#"id="copy-tip" class="autumn-tooltip__bubble" role="tooltip""#),
        "{out}"
    );
    assert!(out.contains(">Copy the link</span>"), "{out}");
}

#[test]
fn the_trigger_gets_the_bare_id() {
    let mut seen = String::new();
    let _ = tooltip("#copy-tip", "Copy", |tip| {
        seen = tip.to_owned();
        html! {}
    });
    assert_eq!(seen, "copy-tip", "a leading `#` is removed");
}

#[test]
fn the_trigger_comes_before_the_text() {
    let out = tooltip("t", "Text", copy_button).into_string();
    let trigger = out.find("<button").expect("trigger");
    let bubble = out.find(r#"role="tooltip""#).expect("bubble");
    assert!(trigger < bubble, "{out}");
}

#[test]
fn the_root_carries_the_runtime_hook_and_the_default_placement() {
    let out = tooltip("t", "Text", copy_button).into_string();
    assert!(
        out.starts_with(r#"<span class="autumn-tooltip autumn-tooltip--top" data-autumn-tooltip>"#),
        "{out}"
    );
}

#[test]
fn each_placement_emits_its_class() {
    for (placement, class) in [
        (TooltipPlacement::Top, "autumn-tooltip--top"),
        (TooltipPlacement::Bottom, "autumn-tooltip--bottom"),
        (TooltipPlacement::Start, "autumn-tooltip--start"),
        (TooltipPlacement::End, "autumn-tooltip--end"),
    ] {
        let config = TooltipConfig::new().placement(placement);
        let out = tooltip_with("t", "Text", &config, copy_button).into_string();
        assert!(
            out.contains(&format!(r#"class="autumn-tooltip {class}""#)),
            "{out}"
        );
    }
    assert_eq!(TooltipPlacement::default(), TooltipPlacement::Top);
}

#[test]
fn an_extra_class_is_appended_to_the_root() {
    let config = TooltipConfig::new().class("help");
    let out = tooltip_with("t", "Text", &config, copy_button).into_string();
    assert!(
        out.contains(r#"class="autumn-tooltip autumn-tooltip--top help""#),
        "{out}"
    );
}

/// Escape closes the tooltip. A click or focus outside does not.
#[test]
fn the_text_uses_the_escape_only_dismiss_mode() {
    let out = tooltip("t", "Text", copy_button).into_string();
    assert!(out.contains(r#"data-autumn-dismiss="escape""#), "{out}");
}

/// `popover` and `interestfor` are Chromium enhancements. The runtime adds
/// them. The server markup must work everywhere.
#[test]
fn the_server_markup_has_no_chromium_only_attributes() {
    let out = tooltip("t", "Text", copy_button).into_string();
    for attr in ["popover", "interestfor", "hidden", "style="] {
        assert!(!out.contains(attr), "`{attr}` in {out}");
    }
}

/// A tooltip holds text only. Markup in it is escaped.
#[test]
fn the_text_is_escaped() {
    let out = tooltip("t", "<a href=\"/x\">x</a>", copy_button).into_string();
    assert!(!out.contains("<a href"), "{out}");
    assert!(out.contains("&lt;a href="), "{out}");
}

#[test]
fn the_id_is_escaped() {
    let out = tooltip("a\"b", "Text", |_| html! {}).into_string();
    assert!(out.contains(r#"id="a&quot;b""#), "{out}");
}

/// An icon-only trigger can use the text as its name.
#[test]
fn the_trigger_can_use_the_id_as_its_label() {
    let out = tooltip("del-tip", "Delete", |tip| {
        html! { button type="button" aria-labelledby=(tip) { "🗑" } }
    })
    .into_string();
    assert!(out.contains(r#"aria-labelledby="del-tip""#), "{out}");
    assert!(out.contains(r#"id="del-tip""#), "{out}");
}

#[test]
fn tooltip_is_tooltip_with_the_default_config() {
    assert_eq!(
        tooltip("t", "Text", copy_button).into_string(),
        tooltip_with("t", "Text", &TooltipConfig::new(), copy_button).into_string()
    );
}

#[test]
fn the_prelude_exports_the_tooltip() {
    use autumn_web::prelude::{
        TooltipConfig as C, TooltipPlacement as P, tooltip as t, tooltip_with as w,
    };
    let config = C::new().placement(P::Bottom);
    let a = t("x", "y", |_| html! {}).into_string();
    let b = w("x", "y", &config, |_| html! {}).into_string();
    assert_ne!(a, b);
}
