//! Tests for `drawer`, `DialogSize` and the modal close button (issue #3403).
//!
//! Run with `cargo test --test integration_tests widgets_drawer`.

#![cfg(feature = "maud")]

use autumn_web::widgets::{
    DialogSize, DrawerConfig, DrawerSide, HeadingLevel, ModalConfig, drawer, drawer_trigger, modal,
};
use maud::html;

fn render(config: &DrawerConfig) -> String {
    drawer("d", "Filters", &html! { p { "Body" } }, config).into_string()
}

fn render_modal(config: &ModalConfig) -> String {
    modal("m", "Title", &html! { p { "Body" } }, config).into_string()
}

// ── drawer: structure ───────────────────────────────────────────────────────

#[test]
fn drawer_is_a_native_dialog_with_default_classes() {
    let out = render(&DrawerConfig::new());
    assert!(out.starts_with("<dialog"), "{out}");
    assert!(out.contains(r#"id="d""#), "{out}");
    assert!(
        out.contains(r#"class="autumn-drawer autumn-drawer--right""#),
        "{out}"
    );
}

#[test]
fn drawer_side_sets_the_modifier_class() {
    for (side, class) in [
        (DrawerSide::Left, "autumn-drawer--left"),
        (DrawerSide::Right, "autumn-drawer--right"),
        (DrawerSide::Top, "autumn-drawer--top"),
        (DrawerSide::Bottom, "autumn-drawer--bottom"),
    ] {
        let out = render(&DrawerConfig::new().side(side));
        assert!(out.contains(class), "{class}: {out}");
    }
}

#[test]
fn drawer_size_sets_the_modifier_class_and_medium_adds_none() {
    for (size, class) in [
        (DialogSize::Sm, "autumn-drawer--sm"),
        (DialogSize::Lg, "autumn-drawer--lg"),
        (DialogSize::Full, "autumn-drawer--full"),
    ] {
        let out = render(&DrawerConfig::new().size(size));
        assert!(out.contains(class), "{class}: {out}");
    }
    let out = render(&DrawerConfig::new().size(DialogSize::Md));
    assert!(!out.contains("autumn-drawer--md"), "{out}");
}

#[test]
fn drawer_extra_class_comes_last() {
    let out = render(&DrawerConfig::new().size(DialogSize::Sm).class("filters"));
    assert!(
        out.contains(r#"class="autumn-drawer autumn-drawer--right autumn-drawer--sm filters""#),
        "{out}"
    );
}

#[test]
fn drawer_renders_title_body_and_footer() {
    let config = DrawerConfig::new().footer(html! { button { "Apply" } });
    let out = render(&config);
    assert!(out.contains(r#"class="autumn-drawer__title""#), "{out}");
    assert!(out.contains("Filters"), "{out}");
    assert!(out.contains(r#"class="autumn-drawer__body""#), "{out}");
    assert!(out.contains("Body"), "{out}");
    assert!(out.contains(r#"class="autumn-drawer__footer""#), "{out}");
    assert!(!render(&DrawerConfig::new()).contains("__footer"));
}

#[test]
fn drawer_title_is_escaped_and_labels_the_dialog() {
    let out = drawer(
        "d",
        "<b>x</b>",
        &html! { p { "x" } },
        &DrawerConfig::new().level(HeadingLevel::H3),
    )
    .into_string();
    assert!(out.contains("&lt;b&gt;"), "{out}");
    assert!(out.contains(r#"aria-labelledby="d-title""#), "{out}");
    assert!(out.contains(r#"<h3 id="d-title""#), "{out}");
}

// ── drawer: modes and dismissal ─────────────────────────────────────────────

#[test]
fn drawer_is_modal_and_light_dismissible_by_default() {
    let out = render(&DrawerConfig::new());
    assert!(out.contains(r#"role="dialog""#), "{out}");
    assert!(out.contains(r#"aria-modal="true""#), "{out}");
    assert!(out.contains(r#"closedby="any""#), "{out}");
}

#[test]
fn non_modal_drawer_closes_on_escape_not_on_outside_click() {
    let out = render(&DrawerConfig::new().modal(false));
    assert!(out.contains(r#"closedby="closerequest""#), "{out}");
    assert!(!out.contains(r#"closedby="any""#), "{out}");
}

#[test]
fn drawer_light_dismiss_can_be_turned_off() {
    for modal in [true, false] {
        let out = render(&DrawerConfig::new().modal(modal).light_dismiss(false));
        assert!(!out.contains("closedby"), "{out}");
    }
}

#[test]
fn non_modal_drawer_has_no_aria_modal() {
    let out = render(&DrawerConfig::new().modal(false));
    assert!(!out.contains("aria-modal"), "{out}");
    assert!(out.contains(r#"role="dialog""#), "{out}");
}

#[test]
fn drawer_open_adds_the_open_attribute() {
    assert!(!render(&DrawerConfig::new().modal(false)).contains(" open"));
    let out = render(&DrawerConfig::new().modal(false).open(true));
    assert!(out.contains(" open"), "{out}");
}

// ── drawer: close button ────────────────────────────────────────────────────

#[test]
fn drawer_has_no_close_button_by_default() {
    assert!(!render(&DrawerConfig::new()).contains("__close"));
}

#[test]
fn drawer_close_button_has_an_accessible_name_and_closes_the_dialog() {
    let out = render(&DrawerConfig::new().close_button("Close filters"));
    assert!(out.contains(r#"class="autumn-drawer__close""#), "{out}");
    assert!(out.contains(r#"aria-label="Close filters""#), "{out}");
    assert!(out.contains(r#"type="button""#), "{out}");
    assert!(out.contains(r#"command="close""#), "{out}");
    assert!(out.contains(r#"commandfor="d""#), "{out}");
    assert!(out.contains(r#"data-modal-close="d""#), "{out}");
    assert!(
        out.contains(r#"<span aria-hidden="true">×</span>"#),
        "{out}"
    );
}

#[test]
fn drawer_close_label_is_escaped() {
    let out = render(&DrawerConfig::new().close_button("\"><script>"));
    assert!(!out.contains("<script>"), "{out}");
}

// ── drawer_trigger ──────────────────────────────────────────────────────────

#[test]
fn modal_drawer_trigger_uses_the_invoker_command() {
    let out = drawer_trigger("Filters", "d", &DrawerConfig::new(), Some("btn")).into_string();
    assert!(out.contains(r#"command="show-modal""#), "{out}");
    assert!(out.contains(r#"commandfor="d""#), "{out}");
    assert!(out.contains(r#"data-modal-open="d""#), "{out}");
    assert!(out.contains(r#"class="btn""#), "{out}");
    assert!(out.contains("Filters"), "{out}");
}

#[test]
fn non_modal_drawer_trigger_never_calls_show_modal() {
    let out = drawer_trigger("Filters", "d", &DrawerConfig::new().modal(false), None).into_string();
    assert!(!out.contains("command="), "{out}");
    assert!(!out.contains("data-modal-open"), "{out}");
    assert!(out.contains(r#"data-drawer-open="d""#), "{out}");
    assert!(out.contains(r#"aria-controls="d""#), "{out}");
}

// ── modal: sizes and close button ───────────────────────────────────────────

#[test]
fn modal_default_markup_is_unchanged() {
    let out = render_modal(&ModalConfig::new());
    assert!(out.contains(r#"class="autumn-modal""#), "{out}");
    assert!(!out.contains("__close"), "{out}");
}

#[test]
fn modal_size_sets_the_modifier_class() {
    for (size, class) in [
        (DialogSize::Sm, r#"class="autumn-modal autumn-modal--sm""#),
        (DialogSize::Lg, r#"class="autumn-modal autumn-modal--lg""#),
        (
            DialogSize::Full,
            r#"class="autumn-modal autumn-modal--full""#,
        ),
        (DialogSize::Md, r#"class="autumn-modal""#),
    ] {
        let out = render_modal(&ModalConfig::new().size(size));
        assert!(out.contains(class), "{class}: {out}");
    }
}

#[test]
fn modal_extra_class_follows_the_size_class() {
    let out = render_modal(&ModalConfig::new().size(DialogSize::Lg).class("wide"));
    assert!(
        out.contains(r#"class="autumn-modal autumn-modal--lg wide""#),
        "{out}"
    );
}

#[test]
fn modal_close_button_has_an_accessible_name_and_closes_the_dialog() {
    let out = render_modal(&ModalConfig::new().close_button("Close dialog"));
    assert!(out.contains(r#"class="autumn-modal__close""#), "{out}");
    assert!(out.contains(r#"aria-label="Close dialog""#), "{out}");
    assert!(out.contains(r#"command="close""#), "{out}");
    assert!(out.contains(r#"commandfor="m""#), "{out}");
    assert!(out.contains(r#"data-modal-close="m""#), "{out}");
    assert!(
        out.contains(r#"<span aria-hidden="true">×</span>"#),
        "{out}"
    );
}
