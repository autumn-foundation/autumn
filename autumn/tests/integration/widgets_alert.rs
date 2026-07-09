//! Tests for the `alert` / `error_summary` widget (issue #1314).
//!
//! Run with `cargo test --test integration_tests widgets_alert`.

#![cfg(feature = "maud")]

use std::collections::HashMap;

use autumn_web::form::Changeset;
use autumn_web::widgets::{AlertConfig, AlertVariant, alert, alert_with, error_summary};
use maud::html;

// ── variant → role / class ───────────────────────────────────────────────────

#[test]
fn variant_maps_to_role_and_class() {
    for (variant, role, class) in [
        (AlertVariant::Info, "status", "alert alert-info"),
        (AlertVariant::Success, "status", "alert alert-success"),
        (AlertVariant::Warning, "alert", "alert alert-warning"),
        (AlertVariant::Error, "alert", "alert alert-error"),
    ] {
        let out = alert(variant, html! { "hi" }).into_string();
        assert!(out.contains(&format!(r#"role="{role}""#)), "{out}");
        assert!(out.contains(&format!(r#"class="{class}""#)), "{out}");
    }
}

#[test]
fn as_str_matches_flash_level_names() {
    assert_eq!(AlertVariant::Info.as_str(), "info");
    assert_eq!(AlertVariant::Success.as_str(), "success");
    assert_eq!(AlertVariant::Warning.as_str(), "warning");
    assert_eq!(AlertVariant::Error.as_str(), "error");
}

#[test]
fn body_is_rendered() {
    let out = alert(AlertVariant::Info, html! { p { "No posts yet." } }).into_string();
    assert!(out.contains("<p>No posts yet.</p>"), "{out}");
    assert!(out.contains(r#"class="alert__body""#), "{out}");
}

// ── builder: title / icon / dismiss ──────────────────────────────────────────

#[test]
fn title_renders_as_heading() {
    let out = alert_with(
        AlertVariant::Error,
        html! { "body" },
        &AlertConfig::new().title("Payment failed"),
    )
    .into_string();
    assert!(
        out.contains(r#"<h2 class="alert__title">Payment failed</h2>"#),
        "{out}"
    );
}

#[test]
fn icon_is_inline_svg_and_opt_in() {
    let without = alert(AlertVariant::Info, html! { "b" }).into_string();
    assert!(!without.contains("<svg"), "no icon by default: {without}");

    let with = alert_with(
        AlertVariant::Info,
        html! { "b" },
        &AlertConfig::new().icon(true),
    )
    .into_string();
    assert!(with.contains("<svg"), "inline svg icon: {with}");
    assert!(with.contains(r#"aria-hidden="true""#), "decorative: {with}");
    // No icon-font dependency / no external asset link.
    assert!(!with.contains("<link"), "{with}");
}

#[test]
fn dismiss_control_is_opt_in_and_js_free() {
    let plain = alert(AlertVariant::Warning, html! { "b" }).into_string();
    assert!(
        !plain.contains("alert__dismiss"),
        "no close control by default: {plain}"
    );
    assert!(!plain.contains("<script"), "no JS: {plain}");

    let dismissible = alert_with(
        AlertVariant::Warning,
        html! { "b" },
        &AlertConfig::new().dismissible(true),
    )
    .into_string();
    assert!(dismissible.contains("alert__dismiss"), "{dismissible}");
    assert!(dismissible.contains(r#"type="checkbox""#), "{dismissible}");
    assert!(!dismissible.contains("<script"), "zero JS: {dismissible}");
    assert!(!dismissible.contains("onclick"), "zero JS: {dismissible}");
}

// ── escaping ─────────────────────────────────────────────────────────────────

#[test]
fn body_markup_is_escaped() {
    // Mirrors the admin XSS test: caller text renders inert.
    let out = alert(
        AlertVariant::Error,
        html! { (String::from("<script>alert('xss')</script>")) },
    )
    .into_string();
    assert!(!out.contains("<script>"), "{out}");
    assert!(out.contains("&lt;script&gt;"), "{out}");
}

#[test]
fn alert_emits_no_inline_style() {
    let out = alert_with(
        AlertVariant::Success,
        html! { "b" },
        &AlertConfig::new()
            .title("Done")
            .icon(true)
            .dismissible(true),
    )
    .into_string();
    assert!(!out.contains("style="), "no inline styles: {out}");
}

// ── error_summary ────────────────────────────────────────────────────────────

#[test]
fn error_summary_is_none_when_valid() {
    let valid: Changeset<()> = Changeset::new(());
    assert!(error_summary(&valid).is_none());
}

#[test]
fn error_summary_lists_all_field_errors_as_error_alert() {
    let mut errors = HashMap::new();
    errors.insert("email".to_string(), vec!["is invalid".to_string()]);
    errors.insert(
        "name".to_string(),
        vec!["must be <b>set</b>".to_string(), "is too short".to_string()],
    );
    let changeset = Changeset::from_errors((), errors);

    let out = error_summary(&changeset).unwrap().into_string();
    assert!(out.contains(r#"class="alert alert-error""#), "{out}");
    assert!(out.contains(r#"role="alert""#), "{out}");
    assert!(out.contains("<ul"), "{out}");
    assert!(out.contains("is invalid"), "{out}");
    assert!(out.contains("is too short"), "{out}");
    // Message text is HTML-escaped (renders inert).
    assert!(!out.contains("<b>set</b>"), "{out}");
    assert!(out.contains("must be &lt;b&gt;set&lt;/b&gt;"), "{out}");
}

#[test]
fn error_summary_is_deterministic() {
    let mut errors = HashMap::new();
    errors.insert("b".to_string(), vec!["err-b".to_string()]);
    errors.insert("a".to_string(), vec!["err-a".to_string()]);
    errors.insert("c".to_string(), vec!["err-c".to_string()]);
    let changeset = Changeset::from_errors((), errors);

    let first = error_summary(&changeset).unwrap().into_string();
    // Stable order across renders (sorted by field then message).
    for _ in 0..5 {
        assert_eq!(error_summary(&changeset).unwrap().into_string(), first);
    }
    // Sorted by field: err-a before err-b before err-c.
    let ia = first.find("err-a").unwrap();
    let ib = first.find("err-b").unwrap();
    let ic = first.find("err-c").unwrap();
    assert!(ia < ib && ib < ic, "{first}");
}
