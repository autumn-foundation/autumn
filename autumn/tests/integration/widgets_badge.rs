//! Tests for the `badge` / `status_tag` widget (issue #1259).
//!
//! Run with `cargo test --test integration_tests widgets_badge`.

#![cfg(feature = "maud")]

use autumn_web::widgets::{
    BadgeConfig, BadgeVariant, Column, DataTableConfig, badge, badge_with, data_table, status_tag,
};
use maud::html;

// ── variants ────────────────────────────────────────────────────────────────

#[test]
fn each_variant_emits_stable_semantic_class() {
    for (variant, class) in [
        (BadgeVariant::Neutral, "badge badge--neutral"),
        (BadgeVariant::Info, "badge badge--info"),
        (BadgeVariant::Success, "badge badge--success"),
        (BadgeVariant::Warning, "badge badge--warning"),
        (BadgeVariant::Danger, "badge badge--danger"),
    ] {
        let out = badge("Label", variant).into_string();
        assert!(out.contains(&format!(r#"class="{class}""#)), "{out}");
        // Text content is present (color is never the only signal).
        assert!(out.contains(">Label<"), "{out}");
    }
}

#[test]
fn badge_emits_no_inline_style() {
    let out = badge("x", BadgeVariant::Success).into_string();
    assert!(!out.contains("style="), "no inline styles: {out}");
}

#[test]
fn badge_escapes_label() {
    let out = badge("<script>alert(1)</script>", BadgeVariant::Danger).into_string();
    assert!(!out.contains("<script>"), "{out}");
    assert!(out.contains("&lt;script&gt;"), "{out}");
}

// ── status_tag shorthand ─────────────────────────────────────────────────────

#[test]
fn status_tag_renders_neutral_badge() {
    let out = status_tag("Archived").into_string();
    assert!(out.contains(r#"class="badge badge--neutral""#), "{out}");
    assert!(out.contains(">Archived<"), "{out}");
}

// ── for_label determinism ────────────────────────────────────────────────────

#[test]
fn for_label_maps_known_vocabulary() {
    assert_eq!(BadgeVariant::for_label("published"), BadgeVariant::Success);
    assert_eq!(BadgeVariant::for_label("Active"), BadgeVariant::Success);
    assert_eq!(BadgeVariant::for_label("draft"), BadgeVariant::Warning);
    assert_eq!(BadgeVariant::for_label("failed"), BadgeVariant::Danger);
    assert_eq!(BadgeVariant::for_label("new"), BadgeVariant::Info);
    assert_eq!(BadgeVariant::for_label(""), BadgeVariant::Neutral);
    assert_eq!(BadgeVariant::for_label("archived"), BadgeVariant::Neutral);
}

#[test]
fn for_label_is_case_and_whitespace_insensitive() {
    assert_eq!(
        BadgeVariant::for_label("  PUBLISHED  "),
        BadgeVariant::for_label("published")
    );
}

#[test]
fn for_label_is_deterministic_for_unknown_values() {
    // Same arbitrary input → same variant, every call.
    let a = BadgeVariant::for_label("wibble-status-42");
    let b = BadgeVariant::for_label("wibble-status-42");
    assert_eq!(a, b);
    // Unknown values still map to a colored (never inline-styled) variant.
    let out = badge("wibble-status-42", a).into_string();
    assert!(out.contains("badge badge--"), "{out}");
}

// ── config: title / aria-label ───────────────────────────────────────────────

#[test]
fn badge_with_sets_title_and_aria_label() {
    let out = badge_with(
        "WIP",
        BadgeVariant::Info,
        &BadgeConfig::new()
            .title("Work in progress")
            .aria_label("Work in progress"),
    )
    .into_string();
    assert!(out.contains(r#"title="Work in progress""#), "{out}");
    assert!(out.contains(r#"aria-label="Work in progress""#), "{out}");
}

// ── composes inside a data_table cell ────────────────────────────────────────

#[test]
fn badge_renders_inside_a_data_table_cell() {
    struct Post {
        title: String,
        status: String,
    }
    let posts = vec![
        Post {
            title: "A".into(),
            status: "published".into(),
        },
        Post {
            title: "B".into(),
            status: "draft".into(),
        },
    ];
    let cols: Vec<Column<Post>> = vec![
        Column::new("Title", |p: &Post| html! { (p.title.as_str()) }),
        Column::new("Status", |p: &Post| {
            badge(&p.status, BadgeVariant::for_label(&p.status))
        }),
    ];
    let out = data_table(&posts, &cols, &DataTableConfig::new("No posts.")).into_string();
    assert!(out.contains("badge--success"), "published → success: {out}");
    assert!(out.contains("badge--warning"), "draft → warning: {out}");
}
