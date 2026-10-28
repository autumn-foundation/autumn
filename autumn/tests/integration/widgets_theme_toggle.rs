//! Tests for the `theme_toggle` widget (issue #3396).
//!
//! Run with `cargo test --test integration_tests widgets_theme_toggle`.

#![cfg(feature = "maud")]

use autumn_web::ui::theme::{THEME_PATH, Theme};
use autumn_web::widgets::{ThemeToggleConfig, theme_toggle};

#[test]
fn renders_a_post_form_to_the_theme_route() {
    let out = theme_toggle(&ThemeToggleConfig::new(Theme::System)).into_string();
    assert!(
        out.starts_with(&format!(
            r#"<form method="post" action="{THEME_PATH}" class="autumn-theme-toggle" data-autumn-theme-toggle>"#
        )),
        "{out}"
    );
    assert!(
        out.contains(r#"<div class="autumn-theme-toggle__group" role="group" aria-label="Theme">"#),
        "{out}"
    );
}

#[test]
fn one_submit_button_per_theme_and_the_current_one_is_pressed() {
    let out = theme_toggle(&ThemeToggleConfig::new(Theme::Dark)).into_string();
    for (value, label, pressed) in [
        ("system", "System", "false"),
        ("light", "Light", "false"),
        ("dark", "Dark", "true"),
    ] {
        let button = format!(
            r#"<button type="submit" name="theme" value="{value}" class="autumn-theme-toggle__option" aria-pressed="{pressed}">{label}</button>"#
        );
        assert!(out.contains(&button), "missing {button} in {out}");
    }
}

#[test]
fn no_csrf_or_return_to_field_unless_set() {
    let out = theme_toggle(&ThemeToggleConfig::new(Theme::Light)).into_string();
    assert!(!out.contains(r#"type="hidden""#), "{out}");
}

#[test]
fn csrf_and_return_to_render_hidden_fields() {
    let out = theme_toggle(
        &ThemeToggleConfig::new(Theme::Light)
            .csrf("tok\"en", "_csrf")
            .return_to("/posts?page=2"),
    )
    .into_string();
    assert!(
        out.contains(r#"<input type="hidden" name="_csrf" value="tok&quot;en">"#),
        "{out}"
    );
    assert!(
        out.contains(r#"<input type="hidden" name="return_to" value="/posts?page=2">"#),
        "{out}"
    );
}

#[test]
fn labels_action_and_class_are_configurable_and_escaped() {
    let out = theme_toggle(
        &ThemeToggleConfig::new(Theme::System)
            .label("Thème")
            .option_labels("Auto", "Clair", "<b>Sombre</b>")
            .action("/prefs/theme")
            .class("ml-auto"),
    )
    .into_string();
    assert!(out.contains(r#"action="/prefs/theme""#), "{out}");
    assert!(
        out.contains(r#"class="autumn-theme-toggle ml-auto""#),
        "{out}"
    );
    assert!(out.contains(r#"aria-label="Thème""#), "{out}");
    assert!(out.contains(">Auto</button>"), "{out}");
    assert!(out.contains(">&lt;b&gt;Sombre&lt;/b&gt;</button>"), "{out}");
}

#[test]
fn emits_no_inline_style_or_script() {
    let out = theme_toggle(&ThemeToggleConfig::new(Theme::Dark)).into_string();
    assert!(!out.contains("style="), "{out}");
    assert!(!out.contains("<script"), "{out}");
}

/// The widget script enhances the form: no reload, no request.
#[cfg(feature = "htmx")]
#[test]
fn widget_script_enhances_the_toggle() {
    let js = std::str::from_utf8(autumn_web::htmx::AUTUMN_WIDGETS_JS).unwrap();
    for needle in [
        "data-autumn-theme-toggle",
        "autumn.theme=",
        "document.documentElement",
        "aria-pressed",
    ] {
        assert!(js.contains(needle), "autumn-widgets.js lacks `{needle}`");
    }
}
